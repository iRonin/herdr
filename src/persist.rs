//! Session persistence — save/restore workspaces, layouts, and working directories.
//!
//! Stored at `~/.config/herdr/session.json`.
//! Optional pane screen history is stored separately at `session-history.json`.
//! Installed plugins are persisted separately at `plugins.json`.

mod ended_panes;
mod io;
pub mod plugin_registry;
mod restore;
mod snapshot;
mod writer;

pub use self::ended_panes::{has_droppable_ended_agent_panes, prune_ended_agent_panes};
pub use self::io::{clear_history, load, load_history};
pub use self::restore::restore;
pub(crate) use self::restore::restored_worktree_space_membership;
#[cfg(unix)]
pub use self::restore::{handoff_pane_aliases, restore_handoff};
pub use self::snapshot::{
    annotate_ended_agent_panes, capture, capture_history, DirectionSnapshot, LayoutSnapshot,
    SessionHistorySnapshot, SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
};
pub(crate) use self::writer::SessionWriter;
