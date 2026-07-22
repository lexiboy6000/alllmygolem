//! The guided "Task pipeline" — a separate full-screen mode that drives a feather
//! task end-to-end (URL → solve → review → open VM → execute → graphs → stop →
//! auto-fill → QA) with a few clearly-signposted human gates and an automatic
//! middle. It is a GUI-side state machine: each automatic stage fires a
//! `RunChain` and advances when the matching `ChainFinished` arrives; the human
//! gates are just states that render a card and wait for a click. The engine,
//! the live log, the prompt modal, the netlist editor and the bundle viewer are
//! all reused unchanged.
//!
//! Phase 1 wires every EXISTING workflow with the gates; the VM-typing step is a
//! manual gate (Phase 2 replaces it with an automated workflow).

use std::collections::BTreeMap;

use eframe::egui;
use rand::RngExt;

use crate::messages::{ConnState, EngineStatus, OutcomeSummary, UiCommand};

use super::GolemApp;

/// The ordered stages of the pipeline. Auto stages fire a `RunChain`; the rest
/// are human gates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Stage {
    Start,
    GetData,
    Solve,
    FormatReview,
    Checkpoints,
    Review,
    OpenVm,
    Execute,
    Graphs,
    StopVagon,
    AutoFill,
    Done,
}

impl Stage {
    const ORDER: [Stage; 12] = [
        Stage::Start,
        Stage::GetData,
        Stage::Solve,
        Stage::FormatReview,
        Stage::Checkpoints,
        Stage::Review,
        Stage::OpenVm,
        Stage::Execute,
        Stage::Graphs,
        Stage::StopVagon,
        Stage::AutoFill,
        Stage::Done,
    ];

    fn index(self) -> usize {
        Self::ORDER.iter().position(|s| *s == self).unwrap_or(0)
    }

    fn next(self) -> Stage {
        Self::ORDER.get(self.index() + 1).copied().unwrap_or(Stage::Done)
    }

    /// Auto stages run a workflow chain; gates wait for the user.
    fn is_auto(self) -> bool {
        matches!(
            self,
            Stage::GetData
                | Stage::Solve
                | Stage::FormatReview
                | Stage::Checkpoints
                | Stage::OpenVm
                | Stage::Execute
                | Stage::StopVagon
                | Stage::AutoFill
        )
    }

    /// Short label for the left-hand stepper.
    fn title(self) -> &'static str {
        match self {
            Stage::Start => "Task URL",
            Stage::GetData => "Get task data",
            Stage::Solve => "Solve (Claude)",
            Stage::FormatReview => "Format netlist",
            Stage::Checkpoints => "Plan checkpoints",
            Stage::Review => "Review netlist & params",
            Stage::OpenVm => "Connect & open VM",
            Stage::Execute => "Type netlist on VM",
            Stage::Graphs => "Run & graph (you)",
            Stage::StopVagon => "Stop Vagon",
            Stage::AutoFill => "Auto-fill & checks",
            Stage::Done => "Final QA (you)",
        }
    }
}

/// Run parameters collected at the review gate (text buffers for editing).
pub(super) struct PipelineParams {
    pub runtime_minutes: String,
    pub cir_filename: String,
    pub typos: bool,
    pub seed: String,
}

impl Default for PipelineParams {
    fn default() -> Self {
        Self {
            runtime_minutes: "180".to_string(),
            cir_filename: "solution".to_string(),
            typos: false,
            seed: String::new(),
        }
    }
}

/// All pipeline state. Lives on `GolemApp`; the draw + transition logic are
/// `GolemApp` methods (below) so they can reach `send`, the editor and bundles.
pub(super) struct PipelineState {
    pub stage: Stage,
    pub task_url: String,
    pub task_id: String,
    pub params: PipelineParams,
    pub anticipated_hours: Option<f64>,
    /// Checkpoint notes (Claude-generated, operator-editable at the review gate).
    pub checkpoints: Vec<String>,
    /// An auto stage's chain is in flight.
    pub running: bool,
    /// The last workflow in the running chain (its success = stage success).
    pub terminal_workflow: String,
    /// Success/failure recorded as the chain's workflows finish.
    pub result: Option<Result<(), String>>,
    /// Failure message to show on the active stage's card.
    pub last_error: Option<String>,
}

impl Default for PipelineState {
    fn default() -> Self {
        Self {
            stage: Stage::Start,
            task_url: String::new(),
            task_id: String::new(),
            params: PipelineParams::default(),
            anticipated_hours: None,
            checkpoints: Vec::new(),
            running: false,
            terminal_workflow: String::new(),
            result: None,
            last_error: None,
        }
    }
}

/// One deferred action, collected inside egui closures and applied after them
/// (so we never call a `&mut self` method while `self` is borrowed by a widget).
enum Action {
    Exit,
    Begin,
    Goto(Stage),
    SubmitReview,
    Skip,
    Retry,
    Reset,
    EditNetlist,
    ViewBundle,
}

impl GolemApp {
    // ---- transitions -----------------------------------------------------

    /// Build the (workflows, shared inputs) a given auto stage runs.
    fn pipeline_chain_for(&self, stage: Stage) -> Option<(Vec<String>, BTreeMap<String, String>)> {
        let url = self.pipeline.task_url.clone();
        let id = self.pipeline.task_id.clone();
        let by_url = || BTreeMap::from([("task_url".to_string(), url.clone())]);
        match stage {
            Stage::GetData => Some((vec!["Get task data".to_string()], by_url())),
            // task_id blank/explicit; "Solve task" pulls in "Solve: preflight".
            Stage::Solve => Some((
                vec!["Solve task (Claude + Docker)".to_string()],
                BTreeMap::from([("task_id".to_string(), id)]),
            )),
            Stage::FormatReview => Some((
                vec!["Solve: format review".to_string()],
                BTreeMap::from([("task_id".to_string(), id)]),
            )),
            Stage::Checkpoints => Some((
                vec!["Solve: checkpoints".to_string()],
                BTreeMap::from([("task_id".to_string(), id)]),
            )),
            Stage::Execute => Some((
                vec!["Execute on VM (type netlist + checkpoints)".to_string()],
                BTreeMap::from([
                    ("task_id".to_string(), id),
                    ("duration_minutes".to_string(), self.pipeline.params.runtime_minutes.clone()),
                    ("seed".to_string(), self.pipeline.params.seed.clone()),
                    ("typos".to_string(), self.pipeline.params.typos.to_string()),
                ]),
            )),
            Stage::OpenVm => Some((
                vec!["Open VM terminal".to_string()],
                BTreeMap::from([
                    ("task_url".to_string(), url),
                    ("filename".to_string(), self.pipeline.params.cir_filename.clone()),
                ]),
            )),
            Stage::StopVagon => Some((vec!["Stop Vagon".to_string()], by_url())),
            Stage::AutoFill => Some((vec!["Auto-fill submission".to_string()], by_url())),
            _ => None,
        }
    }

    /// Fire the current auto stage's chain (RunChain skips the prereq prompt and
    /// shares the inputs across the resolved chain).
    fn pipeline_fire(&mut self, stage: Stage) {
        if let Some((workflows, stage_inputs)) = self.pipeline_chain_for(stage) {
            self.pipeline.running = true;
            self.pipeline.result = None;
            self.pipeline.last_error = None;
            self.pipeline.terminal_workflow = workflows.last().cloned().unwrap_or_default();
            // Seed the chain with every persisted per-workflow default (flattened),
            // so dependency workflows pulled into the chain get their SAVED inputs —
            // notably "Navigate and verify integrity"'s expected_sha (otherwise it
            // falls back to the hardcoded SHA and prompts on every run). Then overlay
            // this stage's explicit inputs (task_url / task_id / filename / …), which
            // take precedence.
            let mut inputs: BTreeMap<String, String> = BTreeMap::new();
            for vals in self.settings.workflow_inputs.values() {
                for (k, v) in vals {
                    inputs.insert(k.clone(), v.clone());
                }
            }
            inputs.extend(stage_inputs);
            self.send(UiCommand::RunChain { workflows, inputs });
        }
    }

    /// Move to `next`, auto-firing it if it's an automatic stage.
    fn pipeline_goto(&mut self, next: Stage) {
        self.pipeline.stage = next;
        self.pipeline.running = false;
        self.pipeline.result = None;
        self.pipeline.last_error = None;
        if next.is_auto() {
            self.pipeline_fire(next);
        }
    }

    fn pipeline_reset(&mut self) {
        self.pipeline = PipelineState::default();
    }

    /// Record per-workflow outcomes for the running stage (called from the event
    /// drain). Only the terminal workflow's success counts as stage success; any
    /// failure/halt/stop fails the stage.
    pub(super) fn pipeline_on_finished(&mut self, name: &str, outcome: &OutcomeSummary) {
        if !self.pipeline.running {
            return;
        }
        match outcome {
            OutcomeSummary::Completed | OutcomeSummary::CompletedWith(_) => {
                if name == self.pipeline.terminal_workflow {
                    self.pipeline.result = Some(Ok(()));
                }
            }
            OutcomeSummary::Halted(m) | OutcomeSummary::Failed(m) => {
                self.pipeline.result = Some(Err(m.clone()));
            }
            OutcomeSummary::Stopped => {
                self.pipeline.result = Some(Err("stopped by user".to_string()));
            }
        }
    }

    /// The running chain finished — advance on success, or surface the failure.
    pub(super) fn pipeline_on_chain_finished(&mut self) {
        if !self.pipeline.running {
            return;
        }
        self.pipeline.running = false;
        let stage = self.pipeline.stage;
        match self.pipeline.result.take() {
            Some(Ok(())) => {
                if stage == Stage::GetData {
                    self.pipeline_load_task_defaults();
                }
                if stage == Stage::Checkpoints {
                    self.pipeline_load_checkpoints();
                }
                self.pipeline_goto(stage.next());
            }
            other => {
                self.pipeline.last_error =
                    Some(other.and_then(Result::err).unwrap_or_else(|| {
                        "the step did not complete (see the log)".to_string()
                    }));
            }
        }
    }

    /// After Get task data, read the saved bundle for the anticipated hours and
    /// seed a jittered default runtime.
    fn pipeline_load_task_defaults(&mut self) {
        let path = self
            .settings
            .data_dir()
            .join(format!("task-{}.json", self.pipeline.task_id));
        let hours = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("anticipated_hours").and_then(|x| x.as_f64()));
        self.pipeline.anticipated_hours = hours;
        self.pipeline.params.runtime_minutes = default_minutes_from_hours(hours).to_string();
    }

    fn pipeline_checkpoints_path(&self) -> std::path::PathBuf {
        self.settings
            .output_dir
            .join("solve")
            .join(&self.pipeline.task_id)
            .join("checkpoints.json")
    }

    /// Load `checkpoints.json` into the editable list for the review gate.
    fn pipeline_load_checkpoints(&mut self) {
        let list = std::fs::read_to_string(self.pipeline_checkpoints_path())
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("checkpoints").and_then(|x| x.as_array()).cloned())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.pipeline.checkpoints = list;
    }

    /// Write the operator's edited checkpoints back to `checkpoints.json` (so the
    /// Execute-on-VM workflow posts the reviewed notes).
    fn pipeline_save_checkpoints(&mut self) {
        let cps: Vec<String> = self
            .pipeline
            .checkpoints
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let path = self.pipeline_checkpoints_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let doc = serde_json::json!({ "checkpoints": cps });
        if let Ok(text) = serde_json::to_string_pretty(&doc)
            && let Err(e) = std::fs::write(&path, text)
        {
            self.push_log(format!("[!] could not save checkpoints: {e}"));
        }
    }

    // ---- gate side-effects (reuse editor + bundles) ----------------------

    fn pipeline_solution_path(&self) -> std::path::PathBuf {
        self.settings
            .output_dir
            .join("solve")
            .join(&self.pipeline.task_id)
            .join("final")
            .join("solution.cir")
    }

    fn pipeline_edit_netlist(&mut self) {
        let path = self.pipeline_solution_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                self.editor_buf = text;
                self.editor_path = Some(path);
                self.editor_status = String::new();
                self.editor_open = true;
            }
            Err(e) => self.push_log(format!("[!] could not open netlist {}: {e}", path.display())),
        }
    }

    fn pipeline_view_bundle(&mut self) {
        // Bundle viewer focused on this task.
        self.bundle_selected = Some(format!("task-{}.json", self.pipeline.task_id));
        self.bundles_files = None;
        self.bundle_view = None;
        self.bundles_open = true;
        // Plots gallery (if the solve produced any).
        let plots = self
            .settings
            .output_dir
            .join("solve")
            .join(&self.pipeline.task_id)
            .join("final")
            .join("plots");
        let mut imgs: Vec<std::path::PathBuf> = std::fs::read_dir(&plots)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("png")))
            .collect();
        imgs.sort();
        if !imgs.is_empty() {
            self.images_title = format!("Plots — {}", self.pipeline.task_id);
            self.image_paths = imgs;
            self.images_open = true;
        }
    }

    // ---- drawing ---------------------------------------------------------

    pub(super) fn draw_pipeline_panel(&mut self, ui: &mut egui::Ui) {
        let mut action: Option<Action> = None;

        // Left: the guided steps + the active stage card (resizable). A fixed-ish
        // width here leaves the rest of the window for the log, so long lines (URLs,
        // saved-default notices) don't wrap awkwardly in a cramped half-width pane.
        egui::Panel::left("pipeline_steps")
            .resizable(true)
            .default_size(470.0)
            .size_range(360.0..=680.0)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Task pipeline");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Exit").clicked() {
                            action = Some(Action::Exit);
                        }
                        if ui.button("Reset").clicked() {
                            action = Some(Action::Reset);
                        }
                    });
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("pipe_steps_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let cur = self.pipeline.stage;
                        for (i, st) in Stage::ORDER.iter().enumerate() {
                            let (icon, color) = self.pipeline_step_icon(*st);
                            let mut rt =
                                egui::RichText::new(format!("{icon} {}. {}", i + 1, st.title()))
                                    .color(color);
                            if *st == cur {
                                rt = rt.strong();
                            }
                            ui.label(rt);
                        }
                        ui.separator();
                        self.draw_pipeline_card(ui, &mut action);
                    });
            });

        // Centre: the full detailed output log — the same colored log the normal run
        // shows (with Copy/Clear), now with most of the window width so it's readable.
        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Output");
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(self.log.join("\n"));
                }
                if ui.button("Clear").clicked() {
                    self.log.clear();
                }
            });
            self.render_log(ui);
        });

        if let Some(a) = action {
            self.apply_pipeline_action(a);
        }
    }

    fn pipeline_step_icon(&self, s: Stage) -> (&'static str, egui::Color32) {
        let cur = self.pipeline.stage;
        if s.index() < cur.index() {
            ("[x]", egui::Color32::LIGHT_GREEN)
        } else if s == cur {
            if self.pipeline.last_error.is_some() {
                ("[!]", egui::Color32::LIGHT_RED)
            } else if self.pipeline.running {
                ("[>]", egui::Color32::LIGHT_BLUE)
            } else {
                ("[*]", egui::Color32::YELLOW)
            }
        } else {
            ("[ ]", egui::Color32::GRAY)
        }
    }

    fn draw_pipeline_card(&mut self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        match self.pipeline.stage {
            Stage::Start => {
                ui.heading("Start a task");
                ui.label(
                    "Claim the task in your browser, then paste its Feather URL below. Golem will \
                     get the task data, solve it with Claude, and stop for your review.",
                );
                ui.add_space(6.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.pipeline.task_url)
                        .hint_text("https://feather.openai.com/tasks/<id>/stage/execution")
                        .desired_width(420.0),
                );
                let valid = task_id_from_url(&self.pipeline.task_url).is_some();
                let connected = matches!(self.conn, ConnState::Connected { .. });
                if !valid && !self.pipeline.task_url.trim().is_empty() {
                    ui.colored_label(egui::Color32::LIGHT_RED, "Not a /tasks/<id>/ URL.");
                }
                if !connected {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        "Connect to Chrome first (top bar) — the task steps need the browser.",
                    );
                }
                ui.add_space(6.0);
                if ui
                    .add_enabled(valid && connected, egui::Button::new("Begin pipeline"))
                    .clicked()
                {
                    *action = Some(Action::Begin);
                }
            }
            Stage::GetData
            | Stage::Solve
            | Stage::FormatReview
            | Stage::Checkpoints
            | Stage::OpenVm
            | Stage::Execute
            | Stage::StopVagon
            | Stage::AutoFill => {
                self.draw_auto_card(ui, action);
            }
            Stage::Review => {
                ui.heading("Review the solved netlist");
                ui.label(
                    "Edit the netlist if Claude got anything wrong, set the run parameters, then \
                     submit. Nothing else will need your input until the graphs step.",
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Edit netlist").clicked() {
                        *action = Some(Action::EditNetlist);
                    }
                    if ui.button("View bundle & plots").clicked() {
                        *action = Some(Action::ViewBundle);
                    }
                });
                ui.separator();
                ui.label(egui::RichText::new("Run parameters").strong());
                if let Some(h) = self.pipeline.anticipated_hours {
                    ui.label(
                        egui::RichText::new(format!(
                            "Task estimate: {h} hour(s) -> default runtime auto-filled (jittered)."
                        ))
                        .weak(),
                    );
                }
                egui::Grid::new("pipe_params").num_columns(2).show(ui, |ui| {
                    ui.label("Target runtime (min)");
                    ui.add(egui::TextEdit::singleline(&mut self.pipeline.params.runtime_minutes).desired_width(120.0));
                    ui.end_row();
                    ui.label("Netlist filename (.cir)");
                    ui.add(egui::TextEdit::singleline(&mut self.pipeline.params.cir_filename).desired_width(220.0));
                    ui.end_row();
                    ui.label("Simulate typos");
                    ui.checkbox(&mut self.pipeline.params.typos, "");
                    ui.end_row();
                    ui.label("Seed (blank = random)");
                    ui.add(egui::TextEdit::singleline(&mut self.pipeline.params.seed).desired_width(120.0));
                    ui.end_row();
                });

                ui.add_space(8.0);
                ui.separator();
                ui.label(egui::RichText::new("Checkpoint notes").strong());
                ui.label(
                    egui::RichText::new(
                        "Posted to the Vagon activity log at evenly-spaced points while typing. \
                         Edit, add or remove as you like.",
                    )
                    .weak(),
                );
                let mut remove: Option<usize> = None;
                for (i, cp) in self.pipeline.checkpoints.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}.", i + 1));
                        ui.add(egui::TextEdit::singleline(cp).desired_width(360.0));
                        if ui.small_button("x").clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    self.pipeline.checkpoints.remove(i);
                }
                if ui.button("+ add checkpoint").clicked() {
                    self.pipeline.checkpoints.push(String::new());
                }

                ui.add_space(6.0);
                let ok = !self.pipeline.params.cir_filename.trim().is_empty()
                    && self.pipeline.params.runtime_minutes.trim().parse::<f64>().is_ok();
                if !ok {
                    ui.colored_label(
                        egui::Color32::LIGHT_RED,
                        "Set a filename and a numeric runtime.",
                    );
                }
                if ui
                    .add_enabled(ok, egui::Button::new("Submit reviewed netlist ->"))
                    .clicked()
                {
                    *action = Some(Action::SubmitReview);
                }
            }
            Stage::Graphs => {
                ui.heading("Run the netlist & capture graphs");
                ui.label(
                    "In the VM: run the netlist, generate the plots, screenshot them, and compare \
                     against the task bundle. When you're done, hand control back to Golem and it \
                     will stop the VM and fill in the submission.",
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("View bundle & plots").clicked() {
                        *action = Some(Action::ViewBundle);
                    }
                    if ui.button("Done - hand back to Golem").clicked() {
                        *action = Some(Action::Goto(Stage::StopVagon));
                    }
                });
            }
            Stage::Done => {
                ui.heading("Pipeline complete");
                ui.label(
                    "Golem stopped the VM, synced the assets, auto-filled the submission and ran \
                     the checks (it never submits). Do your final QA / format review, then submit \
                     yourself.",
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("View bundle & plots").clicked() {
                        *action = Some(Action::ViewBundle);
                    }
                    if ui.button("Start another task").clicked() {
                        *action = Some(Action::Reset);
                    }
                });
            }
        }
    }

    fn draw_auto_card(&mut self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        ui.heading(self.pipeline.stage.title());
        if let Some(err) = self.pipeline.last_error.clone() {
            ui.colored_label(egui::Color32::LIGHT_RED, "This step did not complete:");
            ui.label(err);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Retry step").clicked() {
                    *action = Some(Action::Retry);
                }
                // Optional stages — let the user proceed past a failure of either.
                let skip_label = match self.pipeline.stage {
                    Stage::Checkpoints => Some("Skip (no checkpoints)"),
                    Stage::FormatReview => Some("Skip (keep unformatted)"),
                    _ => None,
                };
                if let Some(lbl) = skip_label
                    && ui.button(lbl).clicked()
                {
                    *action = Some(Action::Skip);
                }
                if ui.button("Abort pipeline").clicked() {
                    *action = Some(Action::Reset);
                }
            });
        } else {
            // Live progress: a percentage bar (or spinner) + the workflow's own
            // progress label (e.g. "line 45/81, 3 saves"), the current activity
            // (note_status / step, e.g. "checkpoint 2/5: posting Vagon log"), and
            // a hint. The streaming output log is shown below the card.
            let (fraction, label) = self.progress.clone();
            ui.horizontal(|ui| {
                match fraction {
                    Some(f) => {
                        ui.add(
                            egui::ProgressBar::new(f.clamp(0.0, 1.0))
                                .desired_width(260.0)
                                .show_percentage(),
                        );
                    }
                    None => {
                        ui.add(egui::Spinner::new());
                    }
                }
                if !label.is_empty() {
                    ui.label(&label);
                }
            });
            if let EngineStatus::Running { step, .. } = &self.status
                && !step.is_empty()
            {
                ui.label(egui::RichText::new(step).color(egui::Color32::LIGHT_BLUE));
            }
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new("Working… press Stop in the top bar to interrupt.").weak(),
            );
        }
    }

    fn apply_pipeline_action(&mut self, action: Action) {
        match action {
            Action::Exit => self.pipeline_open = false,
            Action::Begin => {
                self.pipeline.task_id =
                    task_id_from_url(&self.pipeline.task_url).unwrap_or_default();
                self.pipeline_goto(Stage::GetData);
            }
            Action::Goto(s) => self.pipeline_goto(s),
            Action::SubmitReview => {
                self.pipeline_save_checkpoints();
                self.pipeline_goto(Stage::OpenVm);
            }
            Action::Skip => {
                let next = self.pipeline.stage.next();
                self.pipeline_goto(next);
            }
            Action::Retry => {
                let s = self.pipeline.stage;
                self.pipeline.last_error = None;
                self.pipeline_fire(s);
            }
            Action::Reset => self.pipeline_reset(),
            Action::EditNetlist => self.pipeline_edit_netlist(),
            Action::ViewBundle => self.pipeline_view_bundle(),
        }
    }
}

/// Pull the task id out of a `.../tasks/<id>/...` URL.
fn task_id_from_url(url: &str) -> Option<String> {
    let after = url.split("/tasks/").nth(1)?;
    let id = after.split(['/', '?', '#']).next().unwrap_or("");
    (!id.is_empty()).then(|| id.to_string())
}

/// Baseline minutes from the anticipated hours (default 3h when unknown).
fn base_minutes(hours: Option<f64>) -> f64 {
    match hours {
        Some(h) if h > 0.0 => h * 60.0,
        _ => 180.0,
    }
}

/// Apply a ±10% jitter; `factor` in [-1.0, 1.0]. Never below 1 minute.
fn apply_jitter(base: f64, factor: f64) -> u32 {
    let scaled = base * (1.0 + factor.clamp(-1.0, 1.0) * 0.10);
    scaled.round().max(1.0) as u32
}

/// Default runtime in minutes: anticipated hours, jittered a little (the realized
/// typing time then varies again ±20% inside the typing engine).
fn default_minutes_from_hours(hours: Option<f64>) -> u32 {
    let factor = rand::rng().random_range(-1.0f64..=1.0);
    apply_jitter(base_minutes(hours), factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_task_id() {
        assert_eq!(
            task_id_from_url("https://feather.openai.com/tasks/d3bb5691-2ad7/stage/execution")
                .as_deref(),
            Some("d3bb5691-2ad7")
        );
        assert_eq!(
            task_id_from_url("https://feather.openai.com/tasks/abc?x=1").as_deref(),
            Some("abc")
        );
        assert_eq!(task_id_from_url("https://feather.openai.com/home"), None);
    }

    #[test]
    fn jitter_bounds_and_base() {
        assert_eq!(base_minutes(Some(5.0)), 300.0);
        assert_eq!(base_minutes(None), 180.0);
        assert_eq!(base_minutes(Some(0.0)), 180.0);
        // ±10% envelope, clamped.
        assert_eq!(apply_jitter(300.0, 0.0), 300);
        assert_eq!(apply_jitter(300.0, 1.0), 330);
        assert_eq!(apply_jitter(300.0, -1.0), 270);
        assert_eq!(apply_jitter(300.0, 5.0), 330); // clamps
        assert!(apply_jitter(1.0, -1.0) >= 1);
    }

    #[test]
    fn stage_order_advances_to_done() {
        let mut s = Stage::Start;
        for _ in 0..20 {
            s = s.next();
        }
        assert_eq!(s, Stage::Done);
        assert!(Stage::GetData.is_auto());
        assert!(!Stage::Review.is_auto());
    }
}
