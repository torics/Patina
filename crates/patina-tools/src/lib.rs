//! Built-in tools, path confinement and git operations.
pub mod command;
pub mod confine;
pub mod files;
pub mod git;
pub mod mcp;
pub mod process;
pub mod sandbox;
pub mod setup;

pub use confine::Workspace;

/// Every tool name an agent's `tools` list may contain.
pub fn tool_names() -> Vec<&'static str> {
    let mut names = files::TOOL_NAMES.to_vec();
    names.push(command::RUN_COMMAND);
    names
}
