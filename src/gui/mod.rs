//! The egui front-end: main window (workflow list, controls, output log,
//! settings) plus a movable always-on-top overlay. Talks to the engine only via
//! channels.
//!
//! The whole window body is rendered inside a `catch_unwind` so a rendering bug
//! can never take the process down (stability is requirement #1): a caught
//! panic is logged and replaced with a tiny error label for that frame.

mod overlay;
mod pipeline;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use parking_lot::Mutex;

use crate::error::Result;
use crate::messages::{
    CommandTx, ConnState, EngineEvent, EngineStatus, EventRx, PromptKind, PromptRequest,
    PromptResponse, UiCommand,
};
use crate::registry::WorkflowInfo;
use crate::settings::{InputStrategy, Settings};

use overlay::OverlayState;

pub struct GolemApp {
    commands: CommandTx,
    events: EventRx,
    settings: Settings,
    workflows: Vec<WorkflowInfo>,
    /// Per-workflow, per-input-key text buffers.
    input_buffers: BTreeMap<String, BTreeMap<String, String>>,
    log: Vec<String>,
    status: EngineStatus,
    conn: ConnState,
    progress: (Option<f32>, String),
    prompt: Option<PromptRequest>,
    prompt_input: String,
    settings_open: bool,
    bundles_open: bool,
    /// Selected bundle filename in the bundles viewer.
    bundle_selected: Option<String>,
    /// Netlist editor: open flag, file path, editable buffer, status line.
    editor_open: bool,
    editor_path: Option<std::path::PathBuf>,
    editor_buf: String,
    editor_status: String,
    /// Image viewer (plots or reference images): open flag, title, image paths.
    images_open: bool,
    images_title: String,
    image_paths: Vec<std::path::PathBuf>,
    /// A resumable checkpoint the user may continue: (run_id, workflow, step).
    resume: Option<(String, String, String)>,
    /// Shared snapshot driving the overlay viewport.
    overlay_state: Arc<Mutex<OverlayState>>,
    /// Chrome profile (user-data-dir) the "Launch Chrome" button will use.
    chrome_profile: String,
    /// The chrome profile last persisted to settings — used to detect a
    /// ComboBox change and save the new choice without a per-frame disk write.
    chrome_profile_saved: String,
    /// Selectable profiles for the launch dropdown: (label, user-data-dir path).
    profile_choices: Vec<(String, String)>,
    /// Cached bundle directory listing (None = needs a refresh). Avoids a
    /// blocking `read_dir` on the UI thread every frame the window is open.
    bundles_files: Option<Vec<std::path::PathBuf>>,
    /// The currently displayed bundle, parsed once per selection (not per frame).
    bundle_view: Option<BundleView>,
    /// The guided "Task pipeline" mode + its state machine.
    pipeline_open: bool,
    pipeline: pipeline::PipelineState,
}

/// A parsed task bundle for the viewer, loaded once when the selection changes
/// (reading + JSON-parsing every frame blocks the UI thread / event loop).
struct BundleView {
    /// The selected bundle filename this view was loaded from.
    name: String,
    /// The parsed bundle JSON.
    value: serde_json::Value,
    /// Rubric loaded from the solved bundle when the bundle's own rubric is null.
    fallback_rubric: Option<serde_json::Value>,
}

impl GolemApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        commands: CommandTx,
        events: EventRx,
        mut settings: Settings,
        workflows: Vec<WorkflowInfo>,
    ) -> Self {
        // Scale the whole UI per the configured zoom — the default egui text is
        // small on most displays. Applies to widgets too, so everything stays
        // proportional. Adjustable live in Settings.
        cc.egui_ctx.set_zoom_factor(settings.zoom.clamp(0.5, 4.0));

        // Keep the event loop alive even when Golem's window is occluded / on
        // another Wayland workspace — otherwise the compositor stops sending
        // frame callbacks and marks the app "not responding". A background thread
        // nudges a repaint a few times a second regardless of visibility.
        let repaint_ctx = cc.egui_ctx.clone();
        std::thread::Builder::new()
            .name("golem-repaint".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(120));
                    repaint_ctx.request_repaint();
                }
            })
            .ok();

        // Enable the file:// + image loaders so we can show PNG plots in-app.
        egui_extras::install_image_loaders(&cc.egui_ctx);

        // Pair the launch profile to the chosen binary: a chromium binary must not
        // be pointed at google-chrome's profile dir (or vice-versa) — it can't open
        // it cleanly, so the debug port never comes up. Correct a cross-family
        // mismatch and persist it so the dropdown reflects the fix next time.
        let chrome_profile = resolve_launch_profile(&settings);
        if settings.chrome_user_data_dir.as_deref() != Some(chrome_profile.as_str()) {
            settings.chrome_user_data_dir = Some(chrome_profile.clone());
            let _ = settings.save();
        }
        // Chrome profiles selectable from the "Launch Chrome" dropdown.
        let profile_choices = discover_chrome_profiles(&settings);

        let mut app = GolemApp {
            commands,
            events,
            settings,
            workflows: Vec::new(),
            input_buffers: BTreeMap::new(),
            log: Vec::new(),
            status: EngineStatus::Idle,
            conn: ConnState::Disconnected,
            progress: (None, String::new()),
            prompt: None,
            prompt_input: String::new(),
            settings_open: false,
            bundles_open: false,
            bundle_selected: None,
            editor_open: false,
            editor_path: None,
            editor_buf: String::new(),
            editor_status: String::new(),
            images_open: false,
            images_title: String::new(),
            image_paths: Vec::new(),
            resume: None,
            overlay_state: Arc::new(Mutex::new(OverlayState::default())),
            chrome_profile_saved: chrome_profile.clone(),
            chrome_profile,
            profile_choices,
            bundles_files: None,
            bundle_view: None,
            pipeline_open: false,
            pipeline: pipeline::PipelineState::default(),
        };
        app.set_workflows(workflows);
        app
    }

    /// Replace the workflow list and (re)seed input buffers — preferring the
    /// persisted value, then the declared default.
    fn set_workflows(&mut self, workflows: Vec<WorkflowInfo>) {
        for wf in &workflows {
            let saved = self.settings.workflow_inputs.get(&wf.name).cloned();
            let entry = self.input_buffers.entry(wf.name.clone()).or_default();
            for spec in &wf.inputs {
                let initial = saved
                    .as_ref()
                    .and_then(|m| m.get(&spec.key).cloned())
                    .or_else(|| spec.default.clone())
                    .unwrap_or_default();
                entry.entry(spec.key.clone()).or_insert(initial);
            }
        }
        self.workflows = workflows;
    }

    /// Persist the selected Chrome launch profile so the dropdown defaults to it
    /// next launch (the engine also relaunches into it). Saved into the same
    /// `chrome_user_data_dir` field the connection/relaunch logic already uses.
    fn persist_chrome_profile(&mut self) {
        let udd = self.chrome_profile.trim().to_string();
        self.settings.chrome_user_data_dir = (!udd.is_empty()).then(|| udd.clone());
        match self.settings.save() {
            Ok(()) => self.chrome_profile_saved = self.chrome_profile.clone(),
            Err(e) => self.push_log(format!("[!] failed to save chrome profile: {e}")),
        }
    }

    /// Persist the current input buffers for `workflow` to settings (so the
    /// last-entered values survive restarts).
    fn persist_inputs(&mut self, workflow: &str) {
        if let Some(buf) = self.input_buffers.get(workflow) {
            self.settings
                .workflow_inputs
                .insert(workflow.to_string(), buf.clone());
            if let Err(e) = self.settings.save() {
                self.push_log(format!("[!] failed to save inputs: {e}"));
            }
        }
    }

    fn push_log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > 2000 {
            let drop = self.log.len().saturating_sub(2000);
            self.log.drain(0..drop);
        }
    }

    fn send(&self, cmd: UiCommand) {
        let _ = self.commands.send(cmd);
    }

    fn answer_prompt(&mut self, value: PromptResponse) {
        if let Some(p) = self.prompt.take() {
            self.send(UiCommand::PromptResponse { id: p.id, value });
        }
        self.prompt_input.clear();
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                EngineEvent::Status(s) => self.status = s,
                EngineEvent::Connection(c) => self.conn = c,
                EngineEvent::Output(s) => self.push_log(format!("> {s}")),
                EngineEvent::Log { level, message } => {
                    self.push_log(format!("[{level:?}] {message}"))
                }
                EngineEvent::Error(e) => self.push_log(format!("[!] {e}")),
                EngineEvent::Progress { fraction, label } => {
                    self.progress = (fraction, label);
                }
                EngineEvent::Prompt(p) => {
                    if let PromptKind::Text { default } = &p.kind {
                        self.prompt_input = default.clone();
                    } else {
                        self.prompt_input.clear();
                    }
                    self.prompt = Some(p);
                }
                EngineEvent::PromptCancelled(id) => {
                    if self.prompt.as_ref().map(|p| p.id) == Some(id) {
                        self.prompt = None;
                        self.prompt_input.clear();
                    }
                }
                EngineEvent::WorkflowStarted { name } => self.push_log(format!(">> {name}")),
                EngineEvent::WorkflowFinished { name, outcome } => {
                    self.push_log(format!("[done] {name}: {outcome:?}"));
                    self.pipeline_on_finished(&name, &outcome);
                }
                EngineEvent::ChainFinished => {
                    self.progress = (None, String::new());
                    self.push_log("--- chain finished ---");
                    self.pipeline_on_chain_finished();
                }
                EngineEvent::PanicCaught(p) => self.push_log(format!("[panic] caught: {p}")),
                EngineEvent::Workflows(w) => self.set_workflows(w),
                EngineEvent::ResumeAvailable {
                    run_id,
                    workflow,
                    step,
                } => {
                    self.push_log(format!(
                        "resume available: {workflow} @ {step} ({run_id})"
                    ));
                    self.resume = Some((run_id, workflow, step));
                }
                EngineEvent::SetWorkflowInput {
                    workflow,
                    key,
                    value,
                } => {
                    self.input_buffers
                        .entry(workflow.clone())
                        .or_default()
                        .insert(key.clone(), value.clone());
                    self.settings
                        .workflow_inputs
                        .entry(workflow.clone())
                        .or_default()
                        .insert(key, value);
                    match self.settings.save() {
                        Ok(()) => self.push_log(format!("saved new default for {workflow}")),
                        Err(e) => self.push_log(format!("[!] failed to save default: {e}")),
                    }
                }
            }
        }
    }

    /// Push the current state into the shared overlay snapshot, and pull back
    /// any prompt answered from the overlay so our mirror clears.
    fn sync_overlay(&mut self) -> bool {
        let visible = matches!(
            self.status,
            EngineStatus::Running { .. } | EngineStatus::Paused
        ) || self.prompt.is_some();

        let mut st = self.overlay_state.lock();

        // If the overlay answered the active prompt, clear our copy.
        if let Some(id) = st.answered_prompt.take()
            && self.prompt.as_ref().map(|p| p.id) == Some(id) {
                self.prompt = None;
                self.prompt_input.clear();
            }

        st.visible = visible;
        st.status_text = status_text(&self.status);
        st.paused = matches!(self.status, EngineStatus::Paused);
        match &self.status {
            EngineStatus::Running { workflow, step } => {
                st.workflow = workflow.clone();
                st.step = step.clone();
            }
            _ => {
                st.workflow.clear();
                st.step.clear();
            }
        }
        st.progress_fraction = self.progress.0;
        st.progress_label = self.progress.1.clone();
        st.sync_prompt(&self.prompt);

        visible
    }

    /// Entry point wrapped in `catch_unwind` by [`eframe::App::ui`].
    fn draw(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        _frame: &mut eframe::Frame,
    ) -> Result<()> {
        ctx.request_repaint_after(Duration::from_millis(80));
        self.drain_events();

        // Persist a changed launch-profile selection (cheap; only writes on an
        // actual change, not every frame).
        if self.chrome_profile != self.chrome_profile_saved {
            self.persist_chrome_profile();
        }

        // Esc closes the auxiliary windows (settings / bundles / editor / plots).
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.settings_open = false;
            self.bundles_open = false;
            self.editor_open = false;
            self.images_open = false;
        }

        // Apply the configured UI zoom (updates live when changed in Settings).
        let zoom = self.settings.zoom.clamp(0.5, 4.0);
        if (ctx.zoom_factor() - zoom).abs() > 0.001 {
            ctx.set_zoom_factor(zoom);
        }

        // Panels attach to the root `ui` via show_inside (egui 0.34 API). Order
        // matters: top/side panels reserve space before the central panel.
        self.draw_top_panel(ui);
        self.draw_resume_banner(ui);
        if self.pipeline_open {
            // The pipeline takes over the body; the normal workflow list + log
            // are hidden so nothing competes with the guided run.
            self.draw_pipeline_panel(ui);
        } else {
            self.draw_workflows_panel(ui);
            self.draw_central_panel(ui);
        }

        // Floating windows + the overlay viewport attach to the context.
        self.draw_settings_window(ctx);
        self.draw_bundles_window(ctx);
        self.draw_editor_window(ctx);
        self.draw_images_window(ctx);

        // Draw the overlay first, then the prompt modal LAST, so the modal is
        // painted on top of the (embedded) overlay window instead of behind it.
        // The overlay can be disabled (e.g. if it steals focus on Wayland).
        let show_overlay = self.sync_overlay() && self.settings.overlay_enabled;
        if show_overlay {
            overlay::show(ctx, &self.overlay_state, &self.commands);
        }
        self.draw_prompt_modal(ctx);

        Ok(())
    }

    fn draw_top_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show_inside(ui, |ui| {
            // Keep the horizontal scrollbar at its thin idle width even when
            // hovered/dragged — the floating bar would otherwise expand to
            // `bar_width` and overlay (obscure) the bar's controls on this short
            // panel.
            {
                let scroll = &mut ui.style_mut().spacing.scroll;
                scroll.bar_width = scroll.floating_width;
            }
            // The bar holds a lot of controls; scroll horizontally instead of
            // clipping them off the right edge on a narrow window.
            egui::ScrollArea::horizontal()
                .auto_shrink([false, true])
                .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Golem");
                ui.separator();
                let (s_text, s_color) = status_label(&self.status);
                ui.label(egui::RichText::new(s_text).color(s_color));
                ui.separator();
                let (c_text, c_color) = conn_label(&self.conn);
                ui.label(egui::RichText::new(c_text).color(c_color));
                ui.separator();
                // Pick which profile (user-data-dir) "Launch Chrome" should use.
                let sel_label = profile_label(&self.chrome_profile, &self.profile_choices);
                egui::ComboBox::from_id_salt("chrome_profile")
                    .width(180.0)
                    .selected_text(sel_label)
                    .show_ui(ui, |ui| {
                        for (label, path) in &self.profile_choices {
                            ui.selectable_value(&mut self.chrome_profile, path.clone(), label);
                        }
                        if let Some(custom) = self.settings.chrome_user_data_dir.clone()
                            && !custom.trim().is_empty()
                            && !self.profile_choices.iter().any(|(_, p)| *p == custom)
                        {
                            let lbl = format!("Custom: {}", short_path(&custom));
                            ui.selectable_value(&mut self.chrome_profile, custom, lbl);
                        }
                    });
                if ui
                    .button("Launch Chrome")
                    .on_hover_text(format!(
                        "Launch Chrome/Chromium (debug port {}) with the selected profile, then \
                         connect.\nProfile: {}",
                        self.settings.chrome_port, self.chrome_profile
                    ))
                    .clicked()
                {
                    let udd = self.chrome_profile.trim().to_string();
                    self.send(UiCommand::LaunchChrome {
                        user_data_dir: (!udd.is_empty()).then_some(udd),
                    });
                }
                if ui.button("Connect").clicked() {
                    self.send(UiCommand::Connect);
                }
                if ui.button("Disconnect").clicked() {
                    self.send(UiCommand::Disconnect);
                }
                if ui.button("Stop").clicked() {
                    self.send(UiCommand::Stop);
                }
                if ui.button("Pause").clicked() {
                    self.send(UiCommand::Pause);
                }
                if ui.button("Resume").clicked() {
                    self.send(UiCommand::Resume);
                }
                ui.separator();
                ui.toggle_value(&mut self.pipeline_open, "Pipeline");
                if ui.toggle_value(&mut self.bundles_open, "Bundles").clicked() && self.bundles_open
                {
                    // Re-read the listing each time the window is opened.
                    self.bundles_files = None;
                    self.bundle_view = None;
                }
                ui.toggle_value(&mut self.settings_open, "Settings");
            });
                });
        });
    }

    fn draw_resume_banner(&mut self, ui: &mut egui::Ui) {
        let Some((run_id, workflow, step)) = self.resume.clone() else {
            return;
        };
        egui::Panel::top("resume_banner").show_inside(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!("Resume {workflow} @ {step}"))
                        .color(egui::Color32::ORANGE),
                );
                if ui.button("Resume").clicked() {
                    self.send(UiCommand::ResumeCheckpoint {
                        run_id: run_id.clone(),
                    });
                    self.resume = None;
                }
                if ui.button("Dismiss").clicked() {
                    self.resume = None;
                }
            });
        });
    }

    fn draw_workflows_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("workflows")
            .resizable(true)
            .default_size(260.0)
            .show_inside(ui, |ui| {
                ui.heading("Workflows");
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Clone the list so we can mutate buffers / send while iterating.
                        let workflows = self.workflows.clone();
                        for wf in &workflows {
                            self.draw_one_workflow(ui, wf);
                            ui.separator();
                        }
                    });
            });
    }

    fn draw_one_workflow(&mut self, ui: &mut egui::Ui, wf: &WorkflowInfo) {
        ui.label(egui::RichText::new(&wf.name).strong());
        if !wf.description.is_empty() {
            ui.small(&wf.description);
        }
        if !wf.dependencies.is_empty() {
            ui.small(format!("deps: {}", wf.dependencies.join(", ")));
        }
        if !wf.run_after.is_empty() {
            ui.small(format!("after: {}", wf.run_after.join(", ")));
        }

        let buffers = self.input_buffers.entry(wf.name.clone()).or_default();
        for spec in &wf.inputs {
            let buf = buffers
                .entry(spec.key.clone())
                .or_insert_with(|| spec.default.clone().unwrap_or_default());
            let label = if spec.required {
                format!("{} *", spec.label)
            } else {
                spec.label.clone()
            };
            // Label on its own line (wraps) + a width-filling field, so a long
            // label can't force the panel to stay wide — it shrinks freely.
            ui.small(&label);
            ui.add(egui::TextEdit::singleline(buf).desired_width(f32::INFINITY));
        }

        if ui.button("Run").clicked() {
            // Persist the entered inputs so they survive restarts.
            self.persist_inputs(&wf.name);
            let inputs = self
                .input_buffers
                .get(&wf.name)
                .cloned()
                .unwrap_or_default();
            self.send(UiCommand::Run {
                workflow: wf.name.clone(),
                inputs,
            });
        }
    }

    fn draw_central_panel(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            // Progress area.
            let (fraction, label) = self.progress.clone();
            ui.horizontal(|ui| {
                match fraction {
                    Some(f) => {
                        ui.add(
                            egui::ProgressBar::new(f.clamp(0.0, 1.0))
                                .desired_width(220.0)
                                .show_percentage(),
                        );
                    }
                    None => {
                        if !label.is_empty()
                            || matches!(self.status, EngineStatus::Running { .. })
                        {
                            ui.add(egui::Spinner::new());
                        }
                    }
                }
                if !label.is_empty() {
                    ui.label(&label);
                }
            });

            if let EngineStatus::Running { workflow, step } = &self.status {
                ui.label(
                    egui::RichText::new(format!("{workflow}  >  {step}"))
                        .color(egui::Color32::LIGHT_BLUE),
                );
            }

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
    }

    /// Render the whole log as ONE selectable, wrapping, colored text block
    /// (errors red / warnings yellow). A selectable Label lets the user
    /// drag-select and copy any part even while a task is streaming output (egui
    /// keys text selection on the Label's stable positional id, not on content,
    /// so a new log line does not reset an in-progress selection); wrapping keeps
    /// long lines fully visible. The LayoutJob is rebuilt each frame: egui's
    /// galley cache makes that free when the log is unchanged, and a re-layout of
    /// the capped (<=2000-line) buffer on each new line is cheap at the 80ms
    /// repaint cadence. stick_to_bottom only pins the view when already at the
    /// bottom, so scrolling up to read/select is kept. Shared by the normal
    /// central panel and the pipeline screen.
    fn render_log(&self, ui: &mut egui::Ui) {
        let font = egui::TextStyle::Monospace.resolve(ui.style());
        let normal = ui.visuals().text_color();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let mut job = egui::text::LayoutJob::default();
                job.wrap.max_width = ui.available_width();
                for line in &self.log {
                    let color = if is_error_line(line) {
                        egui::Color32::LIGHT_RED
                    } else if is_warn_line(line) {
                        egui::Color32::YELLOW
                    } else {
                        normal
                    };
                    job.append(
                        line,
                        0.0,
                        egui::TextFormat {
                            font_id: font.clone(),
                            color,
                            ..Default::default()
                        },
                    );
                    job.append(
                        "\n",
                        0.0,
                        egui::TextFormat {
                            font_id: font.clone(),
                            color: normal,
                            ..Default::default()
                        },
                    );
                }
                ui.add(egui::Label::new(job).selectable(true));
            });
    }

    fn draw_settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(true)
            .vscroll(true)
            .show(ctx, |ui| {
                self.settings_body(ui);
            });
        self.settings_open = open;
    }

    fn settings_body(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.settings;

        ui.heading("Chrome");
        ui.horizontal(|ui| {
            ui.label("host");
            ui.text_edit_singleline(&mut s.chrome_host);
        });
        ui.horizontal(|ui| {
            ui.label("port");
            ui.add(egui::DragValue::new(&mut s.chrome_port).range(1..=65535));
        });
        optional_path(ui, "custom chrome path", &mut s.chrome_path);
        optional_path(ui, "custom user-data-dir", &mut s.chrome_user_data_dir);
        ui.checkbox(&mut s.auto_relaunch_chrome, "auto-relaunch chrome");

        ui.separator();
        ui.heading("Filesystem & behaviour");
        ui.horizontal(|ui| {
            ui.label("output dir");
            let mut text = s.output_dir.display().to_string();
            if ui.text_edit_singleline(&mut text).changed() {
                s.output_dir = std::path::PathBuf::from(text);
            }
        });
        ui.checkbox(&mut s.auto_resume, "auto-resume from checkpoint");
        ui.add(egui::Slider::new(&mut s.zoom, 0.8..=2.5).text("UI zoom"));
        ui.checkbox(
            &mut s.overlay_enabled,
            "show movable overlay while running (separate always-on-top window; on Wayland it can steal focus or show 'not responding' when occluded)",
        );

        egui::ComboBox::from_label("input strategy")
            .selected_text(format!("{:?}", s.input_strategy))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut s.input_strategy, InputStrategy::Cdp, "Cdp");
                ui.selectable_value(&mut s.input_strategy, InputStrategy::Native, "Native");
                ui.selectable_value(&mut s.input_strategy, InputStrategy::Hybrid, "Hybrid");
            });

        ui.separator();
        ui.heading("Humanize");
        let h = &mut s.humanize;
        ui.add(egui::Slider::new(&mut h.speed, 0.25..=3.0).text("speed"));
        ui.add(egui::Slider::new(&mut h.curve, 0.0..=0.5).text("curve"));
        ui.add(egui::Slider::new(&mut h.jitter_px, 0.0..=5.0).text("jitter px"));
        ui.add(egui::Slider::new(&mut h.overshoot_chance, 0.0..=1.0).text("overshoot chance"));
        ui.add(egui::Slider::new(&mut h.step_ms, 2.0..=30.0).text("step ms"));
        ui.add(egui::Slider::new(&mut h.keypress_ms, 30.0..=300.0).text("keypress ms"));

        ui.separator();
        ui.heading("Timeouts (ms)");
        ui.horizontal(|ui| {
            ui.label("cdp call");
            ui.add(egui::DragValue::new(&mut s.cdp_call_timeout_ms).speed(50.0));
        });
        ui.horizontal(|ui| {
            ui.label("default wait");
            ui.add(egui::DragValue::new(&mut s.default_wait_timeout_ms).speed(50.0));
        });
        ui.horizontal(|ui| {
            ui.label("reconnect initial");
            ui.add(egui::DragValue::new(&mut s.reconnect_initial_ms).speed(10.0));
        });
        ui.horizontal(|ui| {
            ui.label("reconnect max");
            ui.add(egui::DragValue::new(&mut s.reconnect_max_ms).speed(50.0));
        });

        ui.separator();
        ui.heading("Solve (Docker + Claude Code)");
        ui.horizontal(|ui| {
            ui.label("claude path");
            ui.text_edit_singleline(&mut s.claude_path);
        });
        ui.horizontal(|ui| {
            ui.label("docker image");
            ui.text_edit_singleline(&mut s.docker_image);
        });
        ui.add(egui::Slider::new(&mut s.solve_max_iterations, 1..=10).text("max iterations"));
        ui.horizontal(|ui| {
            ui.label("claude idle timeout (s)")
                .on_hover_text(
                    "Max seconds claude may produce NO output before it's deemed hung. Not an \
                     absolute cap — a productive solve streams the whole time and never trips it.",
                );
            ui.add(egui::DragValue::new(&mut s.claude_timeout_secs).speed(10.0));
        });
        ui.horizontal(|ui| {
            ui.label("solve model");
            ui.add(
                egui::TextEdit::singleline(&mut s.solve_model)
                    .hint_text("opus or claude-opus-4-7"),
            );
        });
        ui.horizontal(|ui| {
            ui.label("solve effort");
            ui.add(
                egui::TextEdit::singleline(&mut s.solve_effort)
                    .hint_text("low/medium/high/xhigh/max"),
            );
        });

        ui.separator();
        if ui.button("Apply").clicked() {
            // Re-pair the launch profile to the (possibly changed) binary, and
            // refresh the dropdown so it reflects the new pairing.
            self.chrome_profile = resolve_launch_profile(&self.settings);
            self.profile_choices = discover_chrome_profiles(&self.settings);
            self.send(UiCommand::UpdateSettings(Box::new(self.settings.clone())));
            if let Err(e) = self.settings.save() {
                tracing::error!("failed to save settings: {e}");
                self.push_log(format!("[!] failed to save settings: {e}"));
            } else {
                self.push_log("settings applied");
            }
        }
    }

    fn draw_bundles_window(&mut self, ctx: &egui::Context) {
        let mut open = self.bundles_open;
        egui::Window::new("Downloaded bundles")
            .open(&mut open)
            .resizable(true)
            .default_width(620.0)
            .vscroll(true)
            .show(ctx, |ui| {
                let dir = self.settings.data_dir();
                // Cache the listing: reading the directory every frame is blocking
                // I/O on the UI thread and can stall the event loop (the compositor
                // then flags the window "not responding"). Refresh on open / button.
                if self.bundles_files.is_none() {
                    self.bundles_files = Some(read_bundle_listing(&dir));
                }
                let count = self.bundles_files.as_ref().map(Vec::len).unwrap_or(0);

                ui.horizontal(|ui| {
                    ui.label(format!("{count} bundle(s) in {}", dir.display()));
                    if ui.button("Refresh").clicked() {
                        self.bundles_files = None;
                        self.bundle_view = None;
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("bundle_list")
                    .max_height(150.0)
                    .show(ui, |ui| {
                        if let Some(files) = self.bundles_files.clone() {
                            for f in &files {
                                if let Some(name) = f.file_name().and_then(|s| s.to_str()) {
                                    let selected = self.bundle_selected.as_deref() == Some(name);
                                    if ui.selectable_label(selected, name).clicked() {
                                        self.bundle_selected = Some(name.to_string());
                                    }
                                }
                            }
                        }
                    });
                ui.separator();

                if let Some(sel) = self.bundle_selected.clone() {
                    // Parse the selected bundle once per selection (not per frame).
                    if self.bundle_view.as_ref().map(|b| b.name.as_str()) != Some(sel.as_str()) {
                        let view = load_bundle_view(&self.settings, &dir, &sel);
                        self.bundle_view = view;
                    }
                    // Clone the small parsed value out so the render below can call
                    // `&mut self` methods (show_images / send / …) without holding a
                    // borrow on `self.bundle_view`.
                    let loaded = self
                        .bundle_view
                        .as_ref()
                        .map(|b| (b.value.clone(), b.fallback_rubric.clone()));
                    match loaded {
                        Some((v, solve_rubric)) => {
                            if let Some(u) = v.get("task_url").and_then(|x| x.as_str()) {
                                ui.add(egui::Label::new(format!("task_url: {u}")).selectable(true));
                            }
                            let prompt = v.get("prompt").and_then(|x| x.as_str()).unwrap_or("");
                            ui.collapsing("prompt", |ui| {
                                ui.add(egui::Label::new(prompt).selectable(true).wrap());
                            });
                            ui.collapsing("rubric", |ui| {
                                let r = solve_rubric
                                    .as_ref()
                                    .or_else(|| v.get("rubric"))
                                    .unwrap_or(&serde_json::Value::Null);
                                render_rubric(ui, r);
                            });
                            let downloads = self.settings.download_dir();
                            for key in ["reference_files", "starting_state_files"] {
                                if let Some(arr) = v.get(key).and_then(|x| x.as_array())
                                    && !arr.is_empty()
                                {
                                    let names: Vec<String> = arr
                                        .iter()
                                        .filter_map(|it| {
                                            it.get("name").and_then(|x| x.as_str()).map(String::from)
                                        })
                                        .collect();
                                    let image_paths: Vec<std::path::PathBuf> = names
                                        .iter()
                                        .filter(|n| is_image_name(n))
                                        .map(|n| downloads.join(n))
                                        .collect();
                                    egui::CollapsingHeader::new(format!("{key} ({})", names.len()))
                                        .id_salt(key)
                                        .show(ui, |ui| {
                                            for n in &names {
                                                ui.label(n);
                                            }
                                            if !image_paths.is_empty()
                                                && ui.button("View images").clicked()
                                            {
                                                self.show_images(
                                                    format!("{key} (images)"),
                                                    image_paths.clone(),
                                                );
                                            }
                                        });
                                }
                            }
                            let ss = v.get("starting_state").and_then(|x| x.as_str()).unwrap_or("");
                            if !ss.trim().is_empty() {
                                ui.collapsing("starting state", |ui| {
                                    ui.add(egui::Label::new(ss).selectable(true).wrap());
                                });
                            }
                            let id = sel
                                .trim_start_matches("task-")
                                .trim_end_matches(".json")
                                .to_string();

                            // Show the solved result, if this task has been solved.
                            let final_dir =
                                self.settings.output_dir.join("solve").join(&id).join("final");
                            let netlist_path = final_dir.join("solution.cir");
                            ui.separator();
                            if netlist_path.exists() {
                                ui.colored_label(egui::Color32::LIGHT_GREEN, "[solved]");
                                ui.add(
                                    egui::Label::new(format!("netlist: {}", netlist_path.display()))
                                        .selectable(true),
                                );
                                ui.collapsing("solved netlist", |ui| {
                                    match std::fs::read_to_string(&netlist_path) {
                                        Ok(t) => {
                                            ui.add(
                                                egui::Label::new(egui::RichText::new(t).monospace())
                                                    .selectable(true),
                                            );
                                        }
                                        Err(e) => {
                                            ui.label(format!("(could not read: {e})"));
                                        }
                                    }
                                });
                                ui.horizontal(|ui| {
                                    if ui.button("Edit netlist").clicked() {
                                        match std::fs::read_to_string(&netlist_path) {
                                            Ok(t) => {
                                                self.editor_buf = t;
                                                self.editor_path = Some(netlist_path.clone());
                                                self.editor_status.clear();
                                                self.editor_open = true;
                                            }
                                            Err(e) => self.push_log(format!("[!] open netlist: {e}")),
                                        }
                                    }
                                    if ui.button("View plots").clicked() {
                                        let pdir = final_dir.join("plots");
                                        let mut paths: Vec<std::path::PathBuf> =
                                            std::fs::read_dir(&pdir)
                                                .map(|r| {
                                                    r.flatten()
                                                        .map(|e| e.path())
                                                        .filter(|p| is_image_path(p))
                                                        .collect()
                                                })
                                                .unwrap_or_default();
                                        paths.sort();
                                        self.show_images(format!("Plots: {}", pdir.display()), paths);
                                    }
                                });
                            } else {
                                ui.label("(not solved yet)");
                            }

                            ui.separator();
                            if ui.button("Solve this task").clicked() {
                                let wf = "Solve task (Claude + Docker)".to_string();
                                self.input_buffers
                                    .entry(wf.clone())
                                    .or_default()
                                    .insert("task_id".to_string(), id);
                                self.persist_inputs(&wf);
                                let inputs =
                                    self.input_buffers.get(&wf).cloned().unwrap_or_default();
                                self.send(UiCommand::Run { workflow: wf, inputs });
                            }
                        }
                        None => {
                            ui.colored_label(
                                egui::Color32::LIGHT_RED,
                                "could not read or parse this bundle",
                            );
                        }
                    }
                } else {
                    ui.label("Select a bundle above to view it.");
                }
            });
        self.bundles_open = open;
    }

    fn draw_editor_window(&mut self, ctx: &egui::Context) {
        if !self.editor_open {
            return;
        }
        let mut open = self.editor_open;
        // Keep the title short/fixed so it never pushes the close button
        // off-screen; show the (possibly long) path inside the window.
        egui::Window::new("Edit netlist")
            .open(&mut open)
            .resizable(true)
            .default_width(680.0)
            .default_height(520.0)
            .show(ctx, |ui| {
                if let Some(p) = self.editor_path.clone() {
                    ui.add(egui::Label::new(p.display().to_string()).truncate());
                }
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked()
                        && let Some(path) = self.editor_path.clone() {
                            self.editor_status = match std::fs::write(&path, self.editor_buf.as_bytes())
                            {
                                Ok(()) => format!("saved {}", path.display()),
                                Err(e) => format!("save failed: {e}"),
                            };
                        }
                    if ui.button("Reload").clicked()
                        && let Some(path) = self.editor_path.clone() {
                            match std::fs::read_to_string(&path) {
                                Ok(t) => {
                                    self.editor_buf = t;
                                    self.editor_status = "reloaded".to_string();
                                }
                                Err(e) => self.editor_status = format!("reload failed: {e}"),
                            }
                        }
                    ui.label(&self.editor_status);
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.editor_buf)
                                .code_editor()
                                .desired_width(f32::INFINITY)
                                .desired_rows(24),
                        );
                    });
            });
        self.editor_open = open;
    }

    fn draw_images_window(&mut self, ctx: &egui::Context) {
        if !self.images_open {
            return;
        }
        let mut open = self.images_open;
        // Short, fixed title so the close button is never pushed off-screen.
        egui::Window::new("Images")
            .open(&mut open)
            .resizable(true)
            .default_width(820.0)
            .default_height(640.0)
            .show(ctx, |ui| {
                if !self.images_title.is_empty() {
                    ui.add(egui::Label::new(&self.images_title).truncate());
                }
                ui.separator();
                if self.image_paths.is_empty() {
                    ui.label("(no images)");
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Uniform size for every image (source PNGs vary in pixel
                        // dimensions, so max_width alone renders inconsistently).
                        // fit_to_exact_size reserves the box even before the image
                        // finishes loading, so they all line up.
                        let w = ui.available_width().clamp(320.0, 760.0);
                        let h = w * 0.62;
                        for p in &self.image_paths {
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new(
                                    p.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                                )
                                .strong(),
                            );
                            if p.exists() {
                                let uri = format!("file://{}", p.display());
                                ui.add(
                                    egui::Image::new(uri)
                                        .fit_to_exact_size(egui::vec2(w, h))
                                        .show_loading_spinner(true),
                                );
                            } else {
                                ui.colored_label(
                                    egui::Color32::LIGHT_RED,
                                    format!("missing: {}", p.display()),
                                );
                            }
                            ui.separator();
                        }
                    });
            });
        self.images_open = open;
    }

    /// Open the image viewer with a titled set of image paths.
    fn show_images(&mut self, title: impl Into<String>, paths: Vec<std::path::PathBuf>) {
        self.images_title = title.into();
        self.image_paths = paths;
        self.images_open = true;
    }

    fn draw_prompt_modal(&mut self, ctx: &egui::Context) {
        let Some(p) = self.prompt.clone() else {
            return;
        };
        egui::Window::new("Golem needs you")
            .collapsible(false)
            .resizable(true)
            .default_width(440.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                // Selectable + scrollable so long messages (e.g. the prerequisite
                // list) can be read and copied.
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(&p.message).selectable(true).wrap());
                    });
                ui.add_space(8.0);
                match &p.kind {
                    PromptKind::Text { .. } => {
                        ui.text_edit_singleline(&mut self.prompt_input);
                        if ui.button("Submit").clicked() {
                            let v = self.prompt_input.clone();
                            self.answer_prompt(PromptResponse::Text(v));
                        }
                    }
                    PromptKind::Confirm => {
                        ui.horizontal(|ui| {
                            if ui.button("Yes").clicked() {
                                self.answer_prompt(PromptResponse::Bool(true));
                            }
                            if ui.button("No").clicked() {
                                self.answer_prompt(PromptResponse::Bool(false));
                            }
                        });
                    }
                    PromptKind::Info => {
                        if ui.button("OK").clicked() {
                            self.answer_prompt(PromptResponse::Dismiss);
                        }
                    }
                    PromptKind::Choice { options } => {
                        for (i, opt) in options.iter().enumerate() {
                            if ui.button(opt).clicked() {
                                self.answer_prompt(PromptResponse::Choice(i));
                            }
                        }
                    }
                }
            });
    }
}

impl eframe::App for GolemApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Non-`move` closure: it borrows `ui`/`self`/`frame` for the call, so
        // they're usable again in the error fallbacks below.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.draw(ui, &ctx, frame)
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!("gui draw error: {e}");
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("GUI error: {e}"));
                });
            }
            Err(_) => {
                tracing::error!("gui draw panicked (isolated; window kept alive)");
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    ui.colored_label(
                        egui::Color32::LIGHT_RED,
                        "GUI panic caught - see logs. The app is still running.",
                    );
                });
            }
        }
    }

    // The glow renderer hands the GL context to `on_exit` for cleanup; we don't
    // hold GL resources, so we just signal the engine to shut down.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.send(UiCommand::Shutdown);
    }
}

/// Whether a log line should be rendered in an error color.
fn is_error_line(line: &str) -> bool {
    line.starts_with("[!]") || line.starts_with("[panic]") || line.starts_with("[Error]")
}

/// Whether a log line is a warning (rendered yellow).
fn is_warn_line(line: &str) -> bool {
    line.starts_with("[Warn]")
}

/// Read the `task-*.json` bundle listing from `dir`, newest first. Done off the
/// per-frame render path (cached) so the UI thread never blocks on `read_dir`.
fn read_bundle_listing(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map(|r| {
            r.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|s| s.to_str())
                        .map(|n| n.starts_with("task-") && n.ends_with(".json"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files.reverse();
    files
}

/// Parse a single bundle file (plus its fallback rubric) for the viewer. Called
/// once when the selection changes — never per frame — to keep file I/O and JSON
/// parsing off the UI render path.
fn load_bundle_view(settings: &Settings, dir: &std::path::Path, sel: &str) -> Option<BundleView> {
    let value = std::fs::read_to_string(dir.join(sel))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())?;
    // The data JSON sometimes captured a null rubric; fall back to the solved
    // bundle's rubric.json when present.
    let fallback_rubric = if value.get("rubric").map(|r| !r.is_null()).unwrap_or(false) {
        None
    } else {
        sel.strip_prefix("task-")
            .and_then(|s| s.strip_suffix(".json"))
            .map(|id| settings.output_dir.join("solve").join(id).join("rubric.json"))
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    };
    Some(BundleView {
        name: sel.to_string(),
        value,
        fallback_rubric,
    })
}

/// The dedicated, never-conflicting Golem profile dir.
fn dedicated_profile(settings: &Settings) -> String {
    settings
        .output_dir
        .join("chrome-profile")
        .to_string_lossy()
        .into_owned()
}

/// Installed system browser profile dirs as `(family, user-data-dir)` where
/// family is "chromium" or "chrome". Only dirs that actually exist are returned.
fn system_profiles() -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if let Some(base) = directories::BaseDirs::new() {
        #[cfg(target_os = "linux")]
        let pairs: [(&str, std::path::PathBuf); 2] = {
            let c = base.config_dir();
            [("chromium", c.join("chromium")), ("chrome", c.join("google-chrome"))]
        };
        #[cfg(target_os = "macos")]
        let pairs: [(&str, std::path::PathBuf); 2] = {
            let c = base.config_dir();
            [("chromium", c.join("Chromium")), ("chrome", c.join("Google/Chrome"))]
        };
        #[cfg(target_os = "windows")]
        let pairs: [(&str, std::path::PathBuf); 2] = {
            let c = base.data_local_dir();
            [
                ("chromium", c.join("Chromium/User Data")),
                ("chrome", c.join("Google/Chrome/User Data")),
            ]
        };
        for (fam, p) in pairs {
            if p.is_dir() {
                out.push((fam, p.to_string_lossy().into_owned()));
            }
        }
    }
    out
}

/// True if the configured binary is Chromium (vs Chrome). An unset/empty path
/// means the default candidate order (Chrome first), so treat it as Chrome.
fn binary_is_chromium(settings: &Settings) -> bool {
    settings
        .chrome_path
        .as_deref()
        .map(|p| p.to_ascii_lowercase().contains("chromium"))
        .unwrap_or(false)
}

/// The user-data-dir to launch with, PAIRED to the chosen binary. Honors an
/// explicit choice (dedicated dir, a custom dir, or the same-family system
/// profile) but corrects a cross-family mismatch — e.g. a chromium binary
/// pointed at google-chrome's profile becomes chromium's profile (or the
/// dedicated one if chromium has no system profile yet).
fn resolve_launch_profile(settings: &Settings) -> String {
    let dedicated = dedicated_profile(settings);
    let want = if binary_is_chromium(settings) { "chromium" } else { "chrome" };
    let sys = system_profiles();
    let matching = sys.iter().find(|(f, _)| *f == want).map(|(_, d)| d.clone());
    let other = sys.iter().find(|(f, _)| *f != want).map(|(_, d)| d.clone());

    match settings
        .chrome_user_data_dir
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        // Persisted profile is the OTHER family's system dir -> mismatch -> pair it.
        Some(cur) if other.as_deref() == Some(cur) => matching.unwrap_or(dedicated),
        // Any other explicit choice (dedicated / custom / same-family) is honored.
        Some(cur) => cur.to_string(),
        // Nothing chosen yet -> the binary's system profile, else dedicated.
        None => matching.unwrap_or(dedicated),
    }
}

/// Selectable Chrome/Chromium profile directories for the launch dropdown: a
/// fresh dedicated Golem profile plus any existing system data dirs (so the user
/// can launch into a logged-in profile). The binary-matching profile is listed
/// first. Each entry is `(label, user-data-dir)`.
fn discover_chrome_profiles(settings: &Settings) -> Vec<(String, String)> {
    let mut out = vec![("Golem profile (fresh, dedicated)".to_string(), dedicated_profile(settings))];
    let want = if binary_is_chromium(settings) { "chromium" } else { "chrome" };
    let mut sys = system_profiles();
    // List the profile that matches the chosen binary first.
    sys.sort_by_key(|(f, _)| *f != want);
    for (fam, dir) in sys {
        let label = if fam == "chromium" { "System Chromium" } else { "System Chrome" };
        out.push((format!("{label} (your logged-in profile)"), dir));
    }
    out
}

/// The dropdown label for a profile path: its known label, else a short path.
fn profile_label(path: &str, choices: &[(String, String)]) -> String {
    choices
        .iter()
        .find(|(_, p)| p == path)
        .map(|(l, _)| l.clone())
        .unwrap_or_else(|| short_path(path))
}

/// Last one-or-two components of a path, for compact display.
fn short_path(path: &str) -> String {
    let p = std::path::Path::new(path);
    match (p.parent().and_then(|x| x.file_name()), p.file_name()) {
        (Some(parent), Some(name)) => {
            format!("{}/{}", parent.to_string_lossy(), name.to_string_lossy())
        }
        (_, Some(name)) => name.to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

/// Whether a filename looks like a viewable image.
fn is_image_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"]
        .iter()
        .any(|ext| n.ends_with(ext))
}

fn is_image_path(p: &std::path::Path) -> bool {
    p.file_name()
        .and_then(|s| s.to_str())
        .map(is_image_name)
        .unwrap_or(false)
}

/// Char-safe truncation with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let t: String = s.chars().take(max).collect();
        format!("{t}...")
    } else {
        s.to_string()
    }
}

/// Render a task rubric (an array of criterion objects, or `{items:[...]}`, or a
/// list of strings) in a structured, readable way.
fn render_rubric(ui: &mut egui::Ui, rubric: &serde_json::Value) {
    let items: Vec<&serde_json::Value> = match rubric {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::Array(a) => a.iter().collect(),
        serde_json::Value::Object(o) => match o.get("items") {
            Some(serde_json::Value::Array(a)) => a.iter().collect(),
            _ => vec![rubric],
        },
        // A bare string/number is a degenerate "rubric"; only show it if non-empty.
        serde_json::Value::String(s) if s.trim().is_empty() => Vec::new(),
        _ => vec![rubric],
    };
    if items.is_empty() {
        ui.label(egui::RichText::new("(no rubric)").weak());
        return;
    }
    let total: f64 = items
        .iter()
        .filter_map(|it| it.get("score").and_then(|s| s.as_f64()))
        .sum();
    let mut header = format!("{} criteria", items.len());
    if total > 0.0 {
        header.push_str(&format!(", {total:.0} points total"));
    }
    ui.label(egui::RichText::new(header).weak());

    for (i, it) in items.iter().enumerate() {
        let n = i + 1;
        match it {
            serde_json::Value::String(s) => {
                ui.add(egui::Label::new(format!("{n}. {s}")).selectable(true).wrap());
            }
            serde_json::Value::Object(_) => {
                let criterion = it.get("criterion").and_then(|x| x.as_str()).unwrap_or("");
                let score = it.get("score").and_then(|x| x.as_f64());
                let label = match score {
                    Some(s) => format!("{n}. [{s:.0}] {}", truncate(criterion, 90)),
                    None => format!("{n}. {}", truncate(criterion, 90)),
                };
                egui::CollapsingHeader::new(label)
                    .id_salt(n)
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(criterion).selectable(true).wrap());
                        if let Some(tags) = it.get("tags").and_then(|x| x.as_array()) {
                            let tags: Vec<&str> = tags.iter().filter_map(|t| t.as_str()).collect();
                            if !tags.is_empty() {
                                ui.label(
                                    egui::RichText::new(format!("tags: {}", tags.join(", "))).weak(),
                                );
                            }
                        }
                        if let Some(forms) = it.get("forms") {
                            for (key, label) in
                                [("type", "type"), ("assessment_process", "how"), ("explanation", "why")]
                            {
                                if let Some(val) = forms.get(key).and_then(|x| x.as_str())
                                    && !val.trim().is_empty()
                                {
                                    ui.add(
                                        egui::Label::new(format!("{label}: {val}"))
                                            .selectable(true)
                                            .wrap(),
                                    );
                                }
                            }
                        }
                    });
            }
            other => {
                ui.add(egui::Label::new(format!("{n}. {other}")).selectable(true));
            }
        }
    }
}


/// Short status label text (used by the overlay snapshot).
fn status_text(status: &EngineStatus) -> String {
    match status {
        EngineStatus::Idle => "Idle".to_string(),
        EngineStatus::Connecting => "Connecting".to_string(),
        EngineStatus::Connected => "Connected".to_string(),
        EngineStatus::Running { .. } => "Running".to_string(),
        EngineStatus::Paused => "Paused".to_string(),
        EngineStatus::Reconnecting { attempt } => format!("Reconnecting (#{attempt})"),
        EngineStatus::Stopped => "Stopped".to_string(),
        EngineStatus::Errored(e) => format!("Error: {e}"),
    }
}

fn status_label(status: &EngineStatus) -> (String, egui::Color32) {
    let color = match status {
        EngineStatus::Idle => egui::Color32::GRAY,
        EngineStatus::Connecting | EngineStatus::Reconnecting { .. } => egui::Color32::YELLOW,
        EngineStatus::Connected => egui::Color32::LIGHT_GREEN,
        EngineStatus::Running { .. } => egui::Color32::LIGHT_BLUE,
        EngineStatus::Paused => egui::Color32::ORANGE,
        EngineStatus::Stopped => egui::Color32::GRAY,
        EngineStatus::Errored(_) => egui::Color32::LIGHT_RED,
    };
    (status_text(status), color)
}

fn conn_label(conn: &ConnState) -> (String, egui::Color32) {
    match conn {
        ConnState::Disconnected => ("chrome: disconnected".to_string(), egui::Color32::GRAY),
        ConnState::Connecting => ("chrome: connecting".to_string(), egui::Color32::YELLOW),
        ConnState::Connected { target_url } => {
            let suffix = target_url
                .as_deref()
                .map(|u| format!(" ({u})"))
                .unwrap_or_default();
            (
                format!("chrome: connected{suffix}"),
                egui::Color32::LIGHT_GREEN,
            )
        }
        ConnState::Reconnecting { attempt } => (
            format!("chrome: reconnecting (#{attempt})"),
            egui::Color32::YELLOW,
        ),
        ConnState::Relaunching => ("chrome: relaunching".to_string(), egui::Color32::ORANGE),
    }
}

/// Edit an `Option<String>` as a checkbox + text field.
fn optional_path(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
    ui.horizontal(|ui| {
        let mut enabled = value.is_some();
        ui.checkbox(&mut enabled, label);
        if enabled {
            let mut text = value.clone().unwrap_or_default();
            ui.add(egui::TextEdit::singleline(&mut text).desired_width(180.0));
            *value = Some(text);
        } else {
            *value = None;
        }
    });
}
