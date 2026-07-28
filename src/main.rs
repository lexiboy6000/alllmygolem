//! Golem — a stability-first workflow automation tool that drives a real Chrome
//! session over CDP with human-like mouse/keyboard motion, plus native OS input.
//!
//! Process shape (see `docs/STABILITY.md`):
//! - GUI (egui) runs on the main thread.
//! - The engine runs on a background multi-thread tokio runtime.
//! - They communicate only through channels; neither can block the other.
//! - A non-aborting panic hook logs panics so a crash in one task never tears
//!   down a multi-hour run.

// Hide the console window on Windows in release builds.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]
// Golem is a small framework: the workflow-authoring API (context/backend/
// settings/messages) is intentionally broader than the built-in workflows
// currently exercise. Allow dead_code so that surface doesn't generate noise;
// real bugs still surface as errors, and clippy still enforces the no-panic rule.
#![allow(dead_code)]

mod backend;
mod cdp;
mod checkpoint;
mod context;
mod engine;
mod error;
mod geometry;
mod gui;
mod humanize;
mod input;
mod messages;
mod prelude;
mod registry;
mod settings;
mod workflow;
mod workflows;

use eframe::egui;

use crate::backend::{BrowserBackend, MouseButton};
use crate::cdp::{CdpBrowser, ConnectionConfig};
use crate::engine::Engine;
use crate::geometry::Point;
use crate::gui::GolemApp;
use crate::registry::WorkflowRegistry;
use crate::settings::Settings;

fn main() -> eframe::Result<()> {
    let mut settings = Settings::load();

    // Anchor a relative output dir to an absolute path (the current working
    // directory at launch), so all output goes to a stable, visible location
    // regardless of where Golem was started from.
    if settings.output_dir.is_relative()
        && let Ok(cwd) = std::env::current_dir()
    {
        settings.output_dir = cwd.join(&settings.output_dir);
    }

    // Best-effort: create the output tree before logging so the file appender
    // has somewhere to write. Failures degrade to stderr-only logging.
    if let Err(e) = settings.ensure_dirs() {
        eprintln!("golem: could not create output dirs: {e}");
    }

    // Keep the logging guard alive for the whole process.
    let _log_guard = init_logging(&settings);
    install_panic_hook();

    tracing::info!(
        "Golem starting; output dir = {}",
        settings.output_dir.display()
    );

    // Diagnostics subcommand: `golem selftest` connects to Chrome, exercises the
    // CDP backend + human motion against a throwaway page, prints results, and
    // exits. Lets the user (and CI) verify connectivity without the GUI.
    if std::env::args().any(|a| a == "selftest") {
        let code = run_selftest(&settings);
        std::process::exit(code);
    }

    // Diagnostics subcommand: `golem extract <task_url>` connects, navigates to
    // the task, and runs the exact extraction "Get task data" uses, printing
    // what it found (prompt, files, starting state) plus tab/rubric presence.
    // Run it against your live, logged-in Chrome to see what the workflow sees.
    {
        let args: Vec<String> = std::env::args().collect();
        if let Some(pos) = args.iter().position(|a| a == "extract") {
            let code = run_extract(&settings, args.get(pos + 1).cloned());
            std::process::exit(code);
        }
    }

    // Diagnostics subcommand: `golem typing-preview <netlist> [minutes=N seed=N
    // typos=true]` generates the human-typing schedule and prints summary stats
    // + a timeline, so the realism (rhythm spread, thinking pauses, typos, save
    // cadence) can be inspected without driving a browser for minutes.
    {
        let args: Vec<String> = std::env::args().collect();
        if let Some(pos) = args.iter().position(|a| a == "typing-preview") {
            let rest = args.get(pos + 1..).unwrap_or(&[]);
            let code = run_typing_preview(args.get(pos + 1).cloned(), rest);
            std::process::exit(code);
        }
    }

    // Headless runner: `golem run "<workflow name>" [key=value ...]` runs a
    // workflow (and its dependencies) without the GUI, streaming engine events to
    // stdout and auto-answering prompts (Confirm=yes, others dismissed). Useful
    // for the Docker/Claude solve pipeline and for automation.
    {
        let args: Vec<String> = std::env::args().collect();
        if let Some(pos) = args.iter().position(|a| a == "run") {
            let workflow = args.get(pos + 1).cloned();
            let inputs: std::collections::BTreeMap<String, String> = args
                .iter()
                .skip(pos + 2)
                .filter_map(|a| a.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
                .collect();
            let code = run_workflow_cli(settings.clone(), workflow, inputs);
            std::process::exit(code);
        }
    }

    // Build the workflow registry once; snapshot its info for the GUI.
    let mut registry = WorkflowRegistry::new();
    workflows::register_all(&mut registry);
    let workflow_infos = registry.list();

    // Channels between GUI and engine.
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();

    // Spawn the engine on a background tokio runtime. A panic inside a task is
    // isolated by tokio; if the whole runtime thread somehow dies, the GUI keeps
    // running and the user can see it in the connection/status indicators.
    let engine_settings = settings.clone();
    let engine_cmd_tx = cmd_tx.clone();
    std::thread::Builder::new()
        .name("golem-engine".into())
        .spawn(move || {
            match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => {
                    rt.block_on(async move {
                        if let Err(e) =
                            Engine::run(engine_settings, registry, cmd_rx, evt_tx, engine_cmd_tx)
                                .await
                        {
                            tracing::error!("engine exited with error: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("failed to build tokio runtime: {e}");
                }
            }
        })
        .ok();

    // Attach to the browser without waiting for someone to press Connect.
    // Turned on by `--connect` or GOLEM_AUTO_CONNECT=1, so a Golem that starts
    // on a remote desktop is usable the moment its window appears instead of
    // sitting there disconnected. The channel is unbounded, so this queues
    // until the supervisor is ready to read it.
    if std::env::args().any(|a| a == "--connect")
        || std::env::var("GOLEM_AUTO_CONNECT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    {
        tracing::info!("auto-connect requested; attaching to the browser at startup");
        let _ = cmd_tx.send(crate::messages::UiCommand::Connect);
    }

    // Run the GUI on the main thread.
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 740.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("Golem")
            // Wayland app id so the compositor (Hyprland) identifies the window
            // for rules / ping handling.
            .with_app_id("Golem"),
        // Disable vsync: on Wayland a vsync'd buffer swap to an OCCLUDED surface
        // (e.g. Golem on another workspace) blocks waiting for a frame callback
        // that never arrives, which freezes the event loop and makes the
        // compositor mark the app "not responding". Without vsync the swap
        // returns immediately, so the loop keeps answering compositor pings.
        vsync: false,
        // The glow (OpenGL) renderer — see the Cargo.toml note. Explicit so it
        // can't silently fall back to wgpu.
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    let settings_for_gui = settings;
    eframe::run_native(
        "Golem",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(GolemApp::new(
                cc,
                cmd_tx,
                evt_rx,
                settings_for_gui,
                workflow_infos,
            )))
        }),
    )
}

/// Initialise tracing to both stderr and a rolling on-disk log. Returns the
/// non-blocking writer guard, which must be kept alive.
fn init_logging(settings: &Settings) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;

    // Default verbosity (overridable with RUST_LOG). Silence two noisy third-party
    // sources that are harmless here:
    //   - wgpu_hal Vulkan "Found no drivers!" — no Vulkan ICD on this box; the
    //     renderer just falls back to GL.
    //   - chromiumoxide::handler "WS Invalid message ..." — chromiumoxide can't
    //     deserialize some newer CDP events and drops them; our flows don't use
    //     them, so it's pure spam.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "info,golem=debug,wgpu_hal=off,wgpu_core=warn,chromiumoxide::handler=error",
        )
    });

    let appender = tracing_appender::rolling::daily(settings.log_dir(), "golem.log");
    let (nb, guard) = tracing_appender::non_blocking(appender);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(nb);

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    Some(guard)
}

/// `golem selftest`: connect to the configured Chrome, navigate a throwaway
/// page, evaluate JS, fetch an element rect, then move + click it with
/// human-like motion — verifying the whole CDP path end-to-end. Returns a
/// process exit code (0 = ok).
fn run_selftest(settings: &Settings) -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("selftest: runtime build failed: {e}");
            return 1;
        }
    };
    rt.block_on(async {
        // Drain engine events so the connection's senders never block.
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move { while evt_rx.recv().await.is_some() {} });

        let cfg = ConnectionConfig::from_settings(settings);
        println!("selftest: connecting to {} ...", cfg.devtools_http());
        let browser = match CdpBrowser::connect(cfg, evt_tx).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("selftest: connect failed: {e}");
                return 1;
            }
        };

        let html = "data:text/html,<html><body><button id=b style=\"position:absolute;left:120px;top:140px;width:160px;height:48px\" onclick=\"window.__golem_clicked=(window.__golem_clicked||0)+1\">Click me</button></body></html>";
        if let Err(e) = browser.navigate(html).await {
            eprintln!("selftest: navigate failed: {e}");
            return 1;
        }
        match browser.eval("2+2").await {
            Ok(v) => println!("selftest: eval 2+2 => {v}"),
            Err(e) => {
                eprintln!("selftest: eval failed: {e}");
                return 1;
            }
        }
        let rect = match browser.get_rect("#b").await {
            Ok(Some(r)) => r,
            Ok(None) => {
                eprintln!("selftest: button #b not found");
                return 1;
            }
            Err(e) => {
                eprintln!("selftest: get_rect failed: {e}");
                return 1;
            }
        };
        println!("selftest: button rect = {rect:?}");

        let target = rect.center();
        let path = {
            let mut rng = rand::rng();
            crate::humanize::mouse_path(Point::ZERO, target, &settings.humanize, &mut rng)
        };
        println!(
            "selftest: human path = {} points -> ({:.1},{:.1})",
            path.len(),
            target.x,
            target.y
        );
        for s in &path {
            if let Err(e) = browser.mouse_move(s.point.x, s.point.y).await {
                eprintln!("selftest: mouse_move failed: {e}");
                return 1;
            }
        }
        let _ = browser.mouse_press(MouseButton::Left, target.x, target.y).await;
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let _ = browser
            .mouse_release(MouseButton::Left, target.x, target.y)
            .await;

        match browser.eval("window.__golem_clicked||0").await {
            Ok(v) => println!("selftest: click registered => {v}"),
            Err(e) => eprintln!("selftest: click check failed: {e}"),
        }
        println!("selftest: OK");
        0
    })
}

/// `golem extract <task_url>`: connect, navigate to the task, and run the same
/// extraction "Get task data" uses, printing the result and key presence checks.
fn run_extract(settings: &Settings, url: Option<String>) -> i32 {
    let Some(url) = url else {
        eprintln!("usage: golem extract <task_url>");
        return 2;
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("extract: runtime build failed: {e}");
            return 1;
        }
    };
    rt.block_on(async {
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move { while evt_rx.recv().await.is_some() {} });

        let cfg = ConnectionConfig::from_settings(settings);
        println!("extract: connecting to {} ...", cfg.devtools_http());
        let browser = match CdpBrowser::connect(cfg, evt_tx).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("extract: connect failed: {e}");
                return 1;
            }
        };
        println!("extract: navigating to {url}");
        if let Err(e) = browser.navigate(&url).await {
            eprintln!("extract: navigate failed: {e}");
            return 1;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

        // Poll (like the workflow's wait_for_text) for up to ~8s, lenient match
        // against any [role=tab], since SPA tabs render a beat after load.
        const TAB_JS: &str = r#"(function(){var els=document.querySelectorAll('[role="tab"]');for(var i=0;i<els.length;i++){if((els[i].textContent||'').trim().indexOf(__T__)!==-1)return true;}return false;})()"#;
        for tab in ["Prompt definition", "Task execution"] {
            let q = serde_json::to_string(tab).unwrap_or_default();
            let js = TAB_JS.replace("__T__", &q);
            let mut present = false;
            for _ in 0..40u32 {
                if matches!(browser.eval(&js).await, Ok(ref v) if v.as_bool().unwrap_or(false)) {
                    present = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            println!("  tab '{tab}' present: {present}");
        }
        // Activate the "Prompt definition" tab (Radix unmounts inactive content,
        // and /stage/execution opens on "Task execution"), then wait for the
        // prompt field to mount — mirroring the workflow.
        const ACTIVATE_PROMPT_TAB: &str = r#"(function(){var els=document.querySelectorAll('[role="tab"]');for(var i=0;i<els.length;i++){if((els[i].textContent||'').trim().indexOf('Prompt definition')!==-1){els[i].click();return true;}}return false;})()"#;
        const PROMPT_PRESENT: &str = r#"!!(document.getElementById('root_task_prompt')||document.querySelector('textarea[name="task_prompt"]'))"#;
        let _ = browser.eval(ACTIVATE_PROMPT_TAB).await;
        for _ in 0..40u32 {
            if matches!(browser.eval(PROMPT_PRESENT).await, Ok(ref v) if v.as_bool().unwrap_or(false))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        match browser.query_exists("[data-testid=\"ContentCopyIcon\"]").await {
            Ok(b) => println!("  rubric copy button present: {b}"),
            Err(e) => println!("  rubric check error: {e}"),
        }

        let wrapped = format!(
            "(function() {{ {} }})()",
            golem_extract_js()
        );
        match browser.eval(&wrapped).await {
            Ok(v) => {
                println!("extract: extraction result:");
                println!(
                    "{}",
                    serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
                );
            }
            Err(e) => {
                eprintln!("extract: extraction eval failed: {e}");
                return 1;
            }
        }

        // Probe rubric capture: hook clipboard, programmatically click the copy
        // button, and read what it copied (the workflow uses a human click, but
        // the hook is what matters for capturing the value).
        const RUBRIC_PROBE: &str = r#"(function(){window.__golem_clip=null;try{var cb=navigator.clipboard;if(!cb){cb={};try{Object.defineProperty(navigator,'clipboard',{value:cb,configurable:true});}catch(e2){}}cb.writeText=function(t){window.__golem_clip=String(t);return Promise.resolve();};}catch(e){}var btn=document.querySelector('[data-testid="ContentCopyIcon"]');var c=btn?(btn.closest('button')||btn):null;if(c)c.click();return window.__golem_clip;})()"#;
        match browser.eval(RUBRIC_PROBE).await {
            Ok(v) => println!(
                "  rubric captured via copy button: {}",
                if v.is_null() { "<none>".to_string() } else { v.to_string() }
            ),
            Err(e) => println!("  rubric probe error: {e}"),
        }
        println!("extract: done");
        0
    })
}

/// The shared extraction script used by both "Get task data" and the diagnostic.
fn golem_extract_js() -> &'static str {
    crate::workflows::feather::get_task_data::EXTRACT_JS
}

/// `golem typing-preview <netlist> [minutes=N seed=N typos=true]`: generate the
/// human-typing schedule and print summary stats + a timeline so the realism can
/// be inspected without driving a browser. Exit 0 on success.
fn run_typing_preview(file: Option<String>, rest: &[String]) -> i32 {
    use crate::workflows::complete::typing::{self, Action, TypingConfig};
    use std::time::Duration;

    let Some(file) = file else {
        eprintln!("usage: golem typing-preview <netlist-file> [minutes=N seed=N typos=true] [head=N]");
        return 2;
    };
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("read {file}: {e}");
            return 1;
        }
    };
    let kv: std::collections::BTreeMap<String, String> = rest
        .iter()
        .filter_map(|a| a.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();
    let minutes: f64 = kv
        .get("minutes")
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|m| m.is_finite())
        .unwrap_or(60.0)
        .clamp(0.05, 24.0 * 60.0);
    let seed: u64 = kv.get("seed").and_then(|s| s.parse().ok()).unwrap_or(1);
    let head: usize = kv.get("head").and_then(|s| s.parse().ok()).unwrap_or(160);
    let typos = kv.get("typos").map(|s| s.eq_ignore_ascii_case("true")).unwrap_or(false);

    let mut cfg = TypingConfig::default();
    if typos {
        cfg.typo_chance = 0.012;
        cfg.long_typo_chance = 0.0025;
    }
    let target = Duration::from_secs_f64(minutes * 60.0);
    let plan = typing::generate(&text, target, &cfg, seed);

    let dur = |d: Duration| -> String {
        let s = d.as_secs();
        if s >= 3600 {
            format!("{}h{:02}m{:02}s", s / 3600, (s % 3600) / 60, s % 60)
        } else if s >= 60 {
            format!("{}m{:02}s", s / 60, s % 60)
        } else {
            format!("{:.1}s", d.as_secs_f64())
        }
    };

    // Inter-key intervals = the delays after ordinary character keystrokes.
    let mut iki: Vec<f64> = plan
        .events
        .iter()
        .filter(|e| matches!(e.action, Action::Char(_)))
        .map(|e| e.delay_after.as_secs_f64() * 1000.0)
        .collect();
    iki.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pct = |v: &[f64], p: f64| -> f64 {
        if v.is_empty() {
            return 0.0;
        }
        let idx = ((p * (v.len() as f64 - 1.0)).round() as usize).min(v.len() - 1);
        v.get(idx).copied().unwrap_or(0.0)
    };
    let mean = if iki.is_empty() {
        0.0
    } else {
        iki.iter().sum::<f64>() / iki.len() as f64
    };

    // Pause buckets over every event delay.
    let buckets = [0.3, 1.0, 4.0, 15.0, f64::INFINITY];
    let labels = ["<0.3s", "0.3-1s", "1-4s", "4-15s", ">15s"];
    let mut counts = [0usize; 5];
    for e in &plan.events {
        let s = e.delay_after.as_secs_f64();
        for (bi, edge) in buckets.iter().enumerate() {
            if s < *edge {
                if let Some(c) = counts.get_mut(bi) {
                    *c += 1;
                }
                break;
            }
        }
    }
    let backspaces = plan
        .events
        .iter()
        .filter(|e| e.action == Action::Backspace)
        .count();

    let ratio = if target.as_secs_f64() > 0.0 {
        plan.total.as_secs_f64() / target.as_secs_f64()
    } else {
        0.0
    };
    println!("== typing-preview: {file} ==");
    println!(
        "chars={} lines={} keystrokes={} saves={} backspaces={} typos={}",
        text.chars().count(),
        plan.lines,
        plan.keystrokes,
        plan.saves,
        backspaces,
        typos
    );
    println!(
        "target={} estimated={} ratio={:.3} seed={seed}",
        dur(target),
        dur(plan.total),
        ratio
    );
    println!(
        "inter-key ms: min={:.0} p50={:.0} mean={:.0} p95={:.0} max={:.0} (n={})",
        iki.first().copied().unwrap_or(0.0),
        pct(&iki, 0.50),
        mean,
        pct(&iki, 0.95),
        iki.last().copied().unwrap_or(0.0),
        iki.len()
    );
    print!("pause buckets:");
    for (l, c) in labels.iter().zip(counts.iter()) {
        print!("  {l}={c}");
    }
    println!();

    println!("--- timeline (first {head} events; T = cumulative) ---");
    let mut t = 0.0_f64;
    for (n, e) in plan.events.iter().enumerate() {
        if n >= head {
            println!("... ({} more events)", plan.events.len().saturating_sub(head));
            break;
        }
        let label = match e.action {
            Action::EnterInsert => "i  (insert)".to_string(),
            Action::Char(' ') => "._ (space)".to_string(),
            Action::Char(c) => format!("'{c}'"),
            Action::Enter => "\\n (newline)".to_string(),
            Action::Backspace => "<BS>".to_string(),
            Action::Escape => "<Esc>".to_string(),
            Action::Key(c) => format!("{c} (key)"),
            Action::CmdEnter => "<CR> :w  *** SAVE ***".to_string(),
        };
        let d = e.delay_after.as_secs_f64();
        let mark = if d >= 1.5 { "   <-- pause" } else { "" };
        println!("T+{:>8}  {:<22} +{:>6.0}ms{mark}", dur(Duration::from_secs_f64(t)), label, d * 1000.0);
        t += d;
    }
    0
}

/// `golem run "<workflow>" [k=v ...]`: run a workflow (and its dependencies)
/// headlessly, streaming engine events to stdout and auto-answering prompts
/// (Confirm=yes, others dismissed). Exit 0 unless a workflow failed/halted/stopped.
fn run_workflow_cli(
    mut settings: Settings,
    workflow: Option<String>,
    inputs: std::collections::BTreeMap<String, String>,
) -> i32 {
    use crate::messages::{
        ConnState, EngineEvent, EngineStatus, OutcomeSummary, PromptKind, PromptResponse,
        UiCommand,
    };

    // Headless-test overrides: point the CLI at an isolated Chrome (e.g. a
    // dedicated debug port) without mutating the user's saved settings.
    if let Ok(h) = std::env::var("GOLEM_CHROME_HOST")
        && !h.trim().is_empty()
    {
        settings.chrome_host = h;
    }
    if let Ok(p) = std::env::var("GOLEM_CHROME_PORT")
        && let Ok(p) = p.trim().parse::<u16>()
    {
        settings.chrome_port = p;
    }

    let Some(workflow) = workflow else {
        eprintln!("usage: golem run \"<workflow name>\" [key=value ...]");
        return 2;
    };
    let mut registry = WorkflowRegistry::new();
    workflows::register_all(&mut registry);
    let Some(target) = registry.get(&workflow) else {
        eprintln!(
            "unknown workflow: {workflow}\navailable: {}",
            registry.names().join(", ")
        );
        return 2;
    };
    // Browser workflows need a live CDP connection before they can run; connect
    // first and only send Run once Chrome is attached.
    let needs_browser = target.requires_browser();

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime build failed: {e}");
            return 1;
        }
    };
    rt.block_on(async move {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = tokio::spawn(Engine::run(settings, registry, cmd_rx, evt_tx, cmd_tx.clone()));
        let mut run_sent = false;
        if needs_browser {
            println!("[connect] attaching to Chrome before running '{workflow}'...");
            let _ = cmd_tx.send(UiCommand::Connect);
        } else {
            let _ = cmd_tx.send(UiCommand::Run {
                workflow: workflow.clone(),
                inputs: inputs.clone(),
            });
            run_sent = true;
        }

        let mut exit_code = 0;
        while let Some(ev) = evt_rx.recv().await {
            match ev {
                EngineEvent::Output(s) => println!("{s}"),
                EngineEvent::Log { level, message } => println!("[{level:?}] {message}"),
                EngineEvent::Error(e) => eprintln!("[error] {e}"),
                EngineEvent::Status(s) => {
                    println!("[status] {s:?}");
                    if needs_browser && !run_sent && matches!(s, EngineStatus::Connected) {
                        run_sent = true;
                        let _ = cmd_tx.send(UiCommand::Run {
                            workflow: workflow.clone(),
                            inputs: inputs.clone(),
                        });
                    }
                }
                EngineEvent::Connection(c) => {
                    println!("[conn] {c:?}");
                    if needs_browser && !run_sent && matches!(c, ConnState::Disconnected) {
                        eprintln!("[error] could not attach to Chrome; aborting");
                        let _ = cmd_tx.send(UiCommand::Shutdown);
                        exit_code = 1;
                        break;
                    }
                }
                EngineEvent::Progress { label, .. } if !label.is_empty() => {
                    println!("[progress] {label}");
                }
                EngineEvent::WorkflowStarted { name } => println!(">> {name}"),
                EngineEvent::WorkflowFinished { name, outcome } => {
                    println!("[done] {name}: {outcome:?}");
                    if matches!(
                        outcome,
                        OutcomeSummary::Failed(_) | OutcomeSummary::Halted(_) | OutcomeSummary::Stopped
                    ) {
                        exit_code = 1;
                    }
                }
                EngineEvent::Prompt(p) => {
                    println!("[prompt] {}", p.message);
                    let value = match p.kind {
                        PromptKind::Confirm => PromptResponse::Bool(true),
                        PromptKind::Text { default } => PromptResponse::Text(default),
                        PromptKind::Choice { .. } => PromptResponse::Choice(0),
                        PromptKind::Info => PromptResponse::Dismiss,
                    };
                    println!("[auto-answer] {value:?}");
                    let _ = cmd_tx.send(UiCommand::PromptResponse { id: p.id, value });
                }
                EngineEvent::ChainFinished => {
                    let _ = cmd_tx.send(UiCommand::Shutdown);
                    break;
                }
                _ => {}
            }
        }
        let _ = engine.await;
        exit_code
    })
}

/// Replace the default panic hook with one that logs (with location) but does
/// NOT abort the process. Worker-task panics are already isolated by tokio; this
/// makes them visible and keeps a record for long unattended runs.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".into());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        tracing::error!(target: "panic", "caught panic at {location}: {payload}");
        // Still call the default hook so we get a backtrace on stderr in dev.
        default(info);
    }));
}
