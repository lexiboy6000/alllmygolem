//! The chain runner: executes one logical "run" (a target workflow plus its
//! dependencies, then any `run_after` follow-ups) under panic isolation.
//!
//! This runs in its own spawned tokio task so the engine's command loop stays
//! responsive (Stop/Pause/Prompt are processed concurrently). It is written so
//! it can **never** panic out of the task: every workflow body is wrapped in
//! [`catch_unwind`], every channel send is best-effort, and every error path
//! ends by emitting [`EngineEvent::ChainFinished`] and clearing the busy flag.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::FutureExt;
use uuid::Uuid;

use crate::checkpoint::RunState;
use crate::context::{Control, PromptBus, WorkflowCtx};
use crate::error::GolemError;
use crate::backend::{BrowserBackend, InputBackend};
use crate::messages::{
    CommandTx, EngineEvent, EngineStatus, EventTx, LogLevel, OutcomeSummary, PromptKind,
    PromptRequest, PromptResponse,
};
use crate::registry::WorkflowRegistry;
use crate::settings::Settings;
use crate::workflow::WorkflowOutcome;

/// Everything the spawned runner needs. Bundled into a struct so the engine can
/// build it once and `tokio::spawn(args.run())`.
pub struct ChainArgs {
    pub registry: Arc<WorkflowRegistry>,
    pub browser: Arc<dyn BrowserBackend>,
    pub input: Arc<dyn InputBackend>,
    pub control: Arc<Control>,
    pub prompts: Arc<PromptBus>,
    pub events: EventTx,
    pub settings: Settings,
    pub busy: Arc<AtomicBool>,
    /// Sender back into the engine's own command queue, so workflows can
    /// enqueue follow-up work (e.g. the pipeline's next round).
    pub commands: CommandTx,
    /// The explicit, ordered list of target workflow names requested by the
    /// user. Each is expanded via [`WorkflowRegistry::resolve_order`] so its
    /// dependencies run first.
    pub targets: Vec<String>,
    /// Inputs supplied by the user. Shared with every workflow in the chain.
    pub inputs: BTreeMap<String, String>,
    /// When resuming from a checkpoint, the saved state to seed into the
    /// matching workflow's context before it runs.
    pub restore: Option<RunState>,
    /// Whether to ask the user to confirm before running resolved prerequisites
    /// that weren't explicitly listed. `true` for a manual single Run; `false`
    /// for an explicit programmatic RunChain (e.g. the pipeline).
    pub confirm_prereqs: bool,
}

/// Best-effort event send; a closed GUI channel must never fail a run.
fn emit(events: &EventTx, event: EngineEvent) {
    let _ = events.send(event);
}

/// Map a successful [`WorkflowOutcome`] to the GUI summary type.
fn outcome_summary(outcome: WorkflowOutcome) -> OutcomeSummary {
    match outcome {
        WorkflowOutcome::Completed => OutcomeSummary::Completed,
        WorkflowOutcome::CompletedWith(value) => {
            let summary = match value {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            OutcomeSummary::CompletedWith(summary)
        }
    }
}

impl ChainArgs {
    /// Run the chain to completion (or until a terminal outcome aborts it).
    /// Never panics out of the task.
    pub async fn run(self) {
        let ChainArgs {
            registry,
            browser,
            input,
            control,
            prompts,
            events,
            settings,
            busy,
            commands,
            targets,
            inputs,
            restore,
            confirm_prereqs,
        } = self;

        busy.store(true, Ordering::SeqCst);
        // Clear any leftover stop/pause from a previous run before we begin.
        control.reset();

        run_inner(
            &registry, &browser, &input, &control, &prompts, &events, &settings, &commands,
            &targets, &inputs, restore.as_ref(), confirm_prereqs,
        )
        .await;

        // Finalize on every path: the chain is done, the engine is idle-but-
        // connected again, and a new run may be accepted.
        busy.store(false, Ordering::SeqCst);
        emit(&events, EngineEvent::ChainFinished);
        emit(&events, EngineEvent::Status(EngineStatus::Connected));
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    registry: &Arc<WorkflowRegistry>,
    browser: &Arc<dyn BrowserBackend>,
    input: &Arc<dyn InputBackend>,
    control: &Arc<Control>,
    prompts: &Arc<PromptBus>,
    events: &EventTx,
    settings: &Settings,
    commands: &CommandTx,
    targets: &[String],
    inputs: &BTreeMap<String, String>,
    restore: Option<&RunState>,
    confirm_prereqs: bool,
) {
    // `queued` dedups across resolve_order expansions and run_after follow-ups so
    // nothing runs twice. `pending` is the deterministic FIFO of resolved names.
    let mut queued: BTreeSet<String> = BTreeSet::new();
    let mut pending: VecDeque<String> = VecDeque::new();

    // Seed the queue with each target's dependency-resolved order.
    for target in targets {
        if let Err(e) = enqueue_resolved(registry, target, &mut queued, &mut pending) {
            emit(events, EngineEvent::Error(e.to_string()));
            return;
        }
    }

    // If this run pulls in prerequisite workflows the user did not explicitly
    // select (resolved recursively), ask them to confirm before doing anything.
    // Running a workflow with no dependencies shows no prompt.
    let target_set: BTreeSet<&str> = targets.iter().map(String::as_str).collect();
    let prereqs: Vec<String> = pending
        .iter()
        .filter(|n| !target_set.contains(n.as_str()))
        .cloned()
        .collect();
    if confirm_prereqs && !prereqs.is_empty() {
        let target_label = targets.join("\", \"");
        let list = prereqs
            .iter()
            .map(|n| format!("  - {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let message = format!(
            "Running \"{target_label}\" normally runs its prerequisite workflow(s) first, in \
             order:\n\n{list}\n\nRun them too, or skip straight to \"{target_label}\" (use skip \
             when the prerequisites' work is already done, e.g. re-running just this leg)?"
        );
        match ask_prereqs(prompts, events, control, message).await {
            PrereqDecision::RunAll => {}
            PrereqDecision::SkipPrereqs => {
                pending.retain(|n| target_set.contains(n.as_str()));
                emit(
                    events,
                    EngineEvent::Log {
                        level: LogLevel::Info,
                        message: format!(
                            "Skipping {} prerequisite(s); running only \"{target_label}\".",
                            prereqs.len()
                        ),
                    },
                );
            }
            PrereqDecision::Cancel => {
                emit(
                    events,
                    EngineEvent::Log {
                        level: LogLevel::Info,
                        message: format!(
                            "Run cancelled: declined prerequisites for \"{target_label}\"."
                        ),
                    },
                );
                return;
            }
        }
    }

    while let Some(name) = pending.pop_front() {
        let wf = match registry.get(&name) {
            Some(wf) => wf,
            None => {
                emit(
                    events,
                    EngineEvent::Error(format!("unknown workflow '{name}'; aborting chain")),
                );
                return;
            }
        };

        let run_id = Uuid::new_v4().to_string();
        let mut ctx = match WorkflowCtx::new(
            browser.clone(),
            input.clone(),
            control.clone(),
            prompts.clone(),
            events.clone(),
            settings.clone(),
            commands.clone(),
            run_id.clone(),
            name.clone(),
            inputs.clone(),
        ) {
            Ok(ctx) => ctx,
            Err(e) => {
                emit(
                    events,
                    EngineEvent::Error(format!("failed to build context for '{name}': {e}")),
                );
                return;
            }
        };

        // Let the workflow see (and re-queue) the chain it is part of -- the
        // download workflows' skip-and-restart recovery uses this.
        ctx.set_chain_targets(targets.to_vec());

        // v1 resume semantics: the workflow re-runs from its first step, but its
        // store + step index are pre-populated from the checkpoint so any data it
        // already extracted is available. (We do not skip steps.)
        if let Some(rs) = restore
            && rs.workflow == name {
                ctx.restore(rs);
            }

        emit(events, EngineEvent::WorkflowStarted { name: name.clone() });

        // Panic isolation: a buggy workflow that panics must not take down the
        // engine task. AssertUnwindSafe is sound here because on a caught panic
        // we abort the whole chain and drop `ctx` rather than reusing it.
        let result = std::panic::AssertUnwindSafe(wf.run(&mut ctx))
            .catch_unwind()
            .await;

        match result {
            Ok(Ok(outcome)) => {
                let summary = outcome_summary(outcome);
                emit(
                    events,
                    EngineEvent::WorkflowFinished {
                        name: name.clone(),
                        outcome: summary,
                    },
                );
                // Completed cleanly: its checkpoint is no longer needed.
                let _ = RunState::delete(&settings.checkpoint_dir(), &run_id);

                // Queue this workflow's run_after follow-ups (each resolved with
                // its own dependencies, deduped against everything already run).
                for after in wf.run_after() {
                    if let Err(e) = enqueue_resolved(registry, after, &mut queued, &mut pending) {
                        emit(events, EngineEvent::Error(e.to_string()));
                        return;
                    }
                }
            }
            Ok(Err(GolemError::StoppedByUser)) => {
                emit(
                    events,
                    EngineEvent::WorkflowFinished {
                        name: name.clone(),
                        outcome: OutcomeSummary::Stopped,
                    },
                );
                emit(events, EngineEvent::Status(EngineStatus::Stopped));
                return;
            }
            Ok(Err(GolemError::Halted(message))) => {
                emit(
                    events,
                    EngineEvent::WorkflowFinished {
                        name: name.clone(),
                        outcome: OutcomeSummary::Halted(message),
                    },
                );
                return;
            }
            Ok(Err(e)) => {
                let message = e.to_string();
                emit(events, EngineEvent::Error(message.clone()));
                emit(
                    events,
                    EngineEvent::WorkflowFinished {
                        name: name.clone(),
                        outcome: OutcomeSummary::Failed(message),
                    },
                );
                return;
            }
            Err(_panic) => {
                emit(
                    events,
                    EngineEvent::PanicCaught(format!("workflow '{name}' panicked; isolated")),
                );
                emit(
                    events,
                    EngineEvent::WorkflowFinished {
                        name: name.clone(),
                        outcome: OutcomeSummary::Failed("panic".to_string()),
                    },
                );
                return;
            }
        }
    }
}

/// What the user chose to do about a target's resolved prerequisites.
enum PrereqDecision {
    /// Run the full chain (prerequisites first, then the target).
    RunAll,
    /// Run only the explicitly selected target(s) -- their prerequisites'
    /// work is already done (e.g. re-running the last leg of a pipeline).
    SkipPrereqs,
    /// Don't run anything.
    Cancel,
}

/// Ask, through the normal prompt UI, how to handle resolved prerequisites.
/// Stop or a dismissed prompt count as cancel -- never as "go ahead".
async fn ask_prereqs(
    prompts: &PromptBus,
    events: &EventTx,
    control: &Control,
    message: String,
) -> PrereqDecision {
    if control.is_stopped() {
        return PrereqDecision::Cancel;
    }
    let id = Uuid::new_v4();
    let rx = prompts.register(id);
    emit(
        events,
        EngineEvent::Prompt(PromptRequest {
            id,
            message,
            kind: PromptKind::Choice {
                options: vec![
                    "Run prerequisites first (full chain)".to_string(),
                    "Skip prerequisites -- run only the selected workflow".to_string(),
                    "Cancel".to_string(),
                ],
            },
        }),
    );
    match rx.await {
        Ok(PromptResponse::Choice(0)) => PrereqDecision::RunAll,
        Ok(PromptResponse::Choice(1)) => PrereqDecision::SkipPrereqs,
        _ => PrereqDecision::Cancel,
    }
}

/// Resolve `name`'s dependency order and append any not-yet-queued names to
/// `pending`, recording them in `queued` so nothing runs twice.
fn enqueue_resolved(
    registry: &WorkflowRegistry,
    name: &str,
    queued: &mut BTreeSet<String>,
    pending: &mut VecDeque<String>,
) -> crate::error::Result<()> {
    let order = registry.resolve_order(name)?;
    for resolved in order {
        if queued.insert(resolved.clone()) {
            pending.push_back(resolved);
        }
    }
    Ok(())
}
