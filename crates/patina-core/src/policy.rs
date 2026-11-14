//! The policy engine: for each tool call, allow it, ask the owner, or deny it.
//! Approvals are a convenience layer. The sandbox (M3) is the security boundary for commands.
use crate::tool::ToolKind;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// Ask before every edit and every command.
    ConfirmAll,
    /// Edits are automatic; commands ask unless they match the project's allowlist.
    #[default]
    AutoEdit,
    /// Everything is automatic. Offered only when a sandbox is active.
    Autopilot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
}

/// `command_allowed` says whether a command matched the project's allowlist.
/// `sandboxed` says whether commands run contained; without containment every action is confirmed.
pub fn decide(mode: PolicyMode, kind: ToolKind, command_allowed: bool, sandboxed: bool) -> Decision {
    let mode = if sandboxed { mode } else { PolicyMode::ConfirmAll };
    match (kind, mode) {
        (ToolKind::Read, _) => Decision::Allow,
        (_, PolicyMode::ConfirmAll) => Decision::Ask,
        (ToolKind::Edit, _) => Decision::Allow,
        (ToolKind::Command, PolicyMode::Autopilot) => Decision::Allow,
        (ToolKind::Command, PolicyMode::AutoEdit) if command_allowed => Decision::Allow,
        (ToolKind::Command, PolicyMode::AutoEdit) => Decision::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Decision::*;
    use PolicyMode::*;
    use ToolKind::*;

    #[test]
    fn policy_table_matches_the_brief() {
        for (mode, kind, listed, want) in [
            (ConfirmAll, Read, false, Allow),
            (ConfirmAll, Edit, false, Ask),
            (ConfirmAll, Command, true, Ask),
            (AutoEdit, Edit, false, Allow),
            (AutoEdit, Command, false, Ask),
            (AutoEdit, Command, true, Allow),
            (Autopilot, Edit, false, Allow),
            (Autopilot, Command, false, Allow),
        ] {
            assert_eq!(decide(mode, kind, listed, true), want, "{mode:?} {kind:?} listed={listed}");
        }
    }

    #[test]
    fn without_a_sandbox_every_mode_behaves_as_confirm_all() {
        for mode in [ConfirmAll, AutoEdit, Autopilot] {
            assert_eq!(decide(mode, Edit, true, false), Ask);
            assert_eq!(decide(mode, Command, true, false), Ask);
            assert_eq!(decide(mode, Read, false, false), Allow);
        }
    }

    #[test]
    fn default_mode_is_auto_edit() {
        assert_eq!(PolicyMode::default(), AutoEdit);
    }
}
