//! The movable, always-on-top overlay viewport.
//!
//! Shown only while a workflow is `Running`/`Paused` or a prompt is active, it
//! gives the user a compact always-visible control surface (stop / pause /
//! resume) and lets them answer prompts without bringing the main window
//! forward.
//!
//! The viewport callback stored by egui must be `Fn + Send + Sync + 'static`,
//! so it cannot borrow `GolemApp`. Instead the main window publishes a small
//! [`OverlayState`] snapshot into a shared `Arc<Mutex<_>>` every frame, and the
//! callback reads/writes that plus a cloned [`CommandTx`] to send commands.

use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::messages::{CommandTx, PromptKind, PromptRequest, UiCommand};

/// Snapshot of everything the overlay needs to render, written by the main
/// window once per frame and read inside the (move) viewport callback.
#[derive(Clone, Debug, Default)]
pub struct OverlayState {
    /// Whether the overlay should be displayed at all this frame.
    pub visible: bool,
    /// Human-readable engine status (e.g. "Running").
    pub status_text: String,
    /// Current workflow name (empty if none).
    pub workflow: String,
    /// Current step label (empty if none).
    pub step: String,
    /// Determinate progress, if known.
    pub progress_fraction: Option<f32>,
    /// Progress / activity label.
    pub progress_label: String,
    /// `true` while the engine is paused (controls Pause/Resume affordance).
    pub paused: bool,

    /// Active prompt, mirrored from the main window.
    pub prompt: Option<PromptRequest>,
    /// The prompt id whose default has already been loaded into `prompt_input`.
    pub prompt_loaded_id: Option<Uuid>,
    /// Editable text answer for a `Text` prompt.
    pub prompt_input: String,
    /// Set by the overlay when the user answers a prompt here, so the main
    /// window can clear its mirror of it on the next frame.
    pub answered_prompt: Option<Uuid>,
}

impl OverlayState {
    /// Sync the active prompt into the overlay, pre-filling the input buffer
    /// once per new prompt id.
    pub fn sync_prompt(&mut self, prompt: &Option<PromptRequest>) {
        match prompt {
            Some(p) => {
                if self.prompt_loaded_id != Some(p.id) {
                    if let PromptKind::Text { default } = &p.kind {
                        self.prompt_input = default.clone();
                    } else {
                        self.prompt_input.clear();
                    }
                    self.prompt_loaded_id = Some(p.id);
                }
                self.prompt = Some(p.clone());
            }
            None => {
                self.prompt = None;
                self.prompt_loaded_id = None;
            }
        }
    }
}

/// Spawn / refresh the overlay viewport. Safe to call every frame; egui keeps a
/// single deferred viewport keyed by id and re-runs the callback as needed.
pub fn show(ctx: &egui::Context, state: &Arc<Mutex<OverlayState>>, commands: &CommandTx) {
    let state = Arc::clone(state);
    let commands = commands.clone();

    let builder = egui::ViewportBuilder::default()
        .with_title("Golem")
        .with_always_on_top()
        .with_decorations(false)
        .with_resizable(false)
        // Bias toward a top-left corner so, when egui has to embed the overlay
        // inside the main window (platforms without child OS windows), it does
        // not sit over the centered prompt modal.
        .with_position([24.0, 80.0])
        .with_inner_size([280.0, 200.0]);

    ctx.show_viewport_deferred(
        egui::ViewportId::from_hash_of("golem_overlay"),
        builder,
        move |ui: &mut egui::Ui, class: egui::ViewportClass| {
            // Draw directly into the provided Ui. egui gives us either the root
            // Ui of a real separate window (`Deferred`) or a Ui already inside an
            // embedded egui window (`EmbeddedWindow`, used when the platform
            // can't create a child OS window — e.g. some Wayland setups). We must
            // NOT open a CentralPanel on the shared context here: that paints
            // over the main window and covers the "Golem needs you" prompt modal.
            let separate_window = matches!(class, egui::ViewportClass::Deferred);
            draw_overlay(ui, separate_window, &state, &commands);
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(80));
        },
    );
}

fn draw_overlay(
    ui: &mut egui::Ui,
    can_drag: bool,
    state: &Arc<Mutex<OverlayState>>,
    commands: &CommandTx,
) {
    let st = state.lock();

    // Draggable header (a real overlay window has no decorations). When the
    // overlay is embedded in the main window, dragging the OS window would move
    // the MAIN window, so only issue StartDrag for a real separate window.
    let header = ui.add(
        egui::Label::new(egui::RichText::new("Golem  :::  (drag)").strong())
            .sense(egui::Sense::click_and_drag()),
    );
    if can_drag && header.dragged() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    ui.separator();

    ui.label(egui::RichText::new(&st.status_text).color(egui::Color32::LIGHT_BLUE));
    if !st.workflow.is_empty() {
        ui.label(egui::RichText::new(&st.workflow).strong());
    }
    if !st.step.is_empty() {
        ui.small(&st.step);
    }

    match st.progress_fraction {
        Some(f) => {
            ui.add(egui::ProgressBar::new(f.clamp(0.0, 1.0)).show_percentage());
        }
        None => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                if !st.progress_label.is_empty() {
                    ui.small(&st.progress_label);
                }
            });
        }
    }

    ui.add_space(4.0);

    ui.horizontal(|ui| {
        let stop = ui.add(
            egui::Button::new(egui::RichText::new("STOP").strong().color(egui::Color32::WHITE))
                .fill(egui::Color32::from_rgb(180, 30, 30)),
        );
        if stop.clicked() {
            let _ = commands.send(UiCommand::Stop);
        }
        if st.paused {
            if ui.button("Resume").clicked() {
                let _ = commands.send(UiCommand::Resume);
            }
        } else if ui.button("Pause").clicked() {
            let _ = commands.send(UiCommand::Pause);
        }
    });

    // Prompts are answered in the main window's "Golem needs you" dialog (which
    // is drawn on top). The overlay intentionally does NOT mirror the prompt: a
    // duplicate in this small, undecorated window rendered cut off and could
    // wedge layout. If a prompt is waiting, point the user at the dialog.
    if st.prompt.is_some() {
        ui.separator();
        ui.label(
            egui::RichText::new("Waiting for your response in the\n\"Golem needs you\" dialog.")
                .color(egui::Color32::YELLOW),
        );
    }
}
