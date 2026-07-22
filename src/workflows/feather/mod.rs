//! Feather (feather.openai.com) task-platform workflows, per
//! `docs/WORKFLOWS_20260618.md`.

pub mod util;

pub mod claim_task;
pub mod get_task_data;
pub mod navigate_home;
pub mod navigate_to_task;
pub mod navigate_verify;
pub mod open_vm;
pub mod stop_vagon;
pub mod submit_fill;
pub mod vagon_log;

pub use claim_task::ClaimTask;
pub use get_task_data::GetTaskData;
pub use navigate_home::NavigateHome;
pub use navigate_to_task::NavigateToTask;
pub use navigate_verify::NavigateVerify;
pub use open_vm::OpenVmTerminal;
pub use stop_vagon::StopVagon;
pub use submit_fill::SubmitFill;
pub use vagon_log::CreateVagonLog;
