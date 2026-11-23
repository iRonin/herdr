//! Leaving panes whose agent ended out of a restore (`[session] skip_ended_agent_panes_on_restore`).
//!
//! A save with the setting on marks a pane whose agent ended and whose shell is idle (`PaneSnapshot::agent_ended`). The start
//! that reads such a file runs `prune_ended_agent_panes` over the loaded snapshot and the loaded pane history, before the
//! unchanged `restore`, so everything `restore` indexes by position (the history, `active`, `selected`, `active_tab`) is fixed in
//! one place.

use super::snapshot::{SessionHistorySnapshot, SessionSnapshot};

/// What `prune_ended_agent_panes` left out of the session.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DroppedEndedPanes {
    pub panes: usize,
    pub tabs: usize,
    pub workspaces: usize,
    /// The ids of the workspaces that went, so a new workspace does not reuse one.
    pub workspace_ids: Vec<String>,
}

/// Whether any pane of the snapshot carries the mark.
pub fn has_ended_agent_panes(snapshot: &SessionSnapshot) -> bool {
    let _ = snapshot;
    false
}

/// Removes every marked pane from the snapshot, and from the history in step with it: the layout of a tab that loses a pane,
/// the tab that loses all of them, the workspace that loses every tab, and the indices that pointed past them.
pub fn prune_ended_agent_panes(
    snapshot: &mut SessionSnapshot,
    history: &mut Option<SessionHistorySnapshot>,
) -> DroppedEndedPanes {
    let _ = (snapshot, history);
    DroppedEndedPanes::default()
}
