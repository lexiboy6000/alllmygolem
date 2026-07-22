//! "Solve task" workflows — take the downloaded SPICE task and produce a final
//! ngspice netlist using Claude Code in a Docker sandbox, gated by review agents.
//! See `docs/SOLVE_WORKFLOW_PLAN.md`.

pub mod build_image;
pub mod checkpoints;
pub mod format_review;
pub mod orchestrator;
pub mod preflight;
pub mod util;

pub use build_image::BuildImage;
pub use checkpoints::SolveCheckpoints;
pub use format_review::SolveFormatReview;
pub use orchestrator::SolveTask;
pub use preflight::SolvePreflight;
