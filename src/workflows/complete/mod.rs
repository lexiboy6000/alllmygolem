//! "Complete task" — type a solved netlist into a browser-hosted neovim editor
//! at a human pace using direct input. See `workflow.rs` for the flow and
//! `typing.rs` for the human-typing schedule model.

pub mod demo;
pub mod execute_vm;
pub mod nvim;
pub mod typing;
pub mod workflow;

pub use execute_vm::ExecuteOnVm;
pub use workflow::CompleteTask;
