use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::common::AgentStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct TabListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabRenameParams {
    pub tab_id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabMoveParams {
    pub tab_id: String,
    pub insert_index: usize,
}

/// Move a whole tab — panes, split layout, label, zoom, and running terminals
/// — to another workspace as a unit. This is a separate method from `tab.move`
/// (in-workspace reorder) so older servers reject it as an unknown method
/// instead of silently ignoring a `workspace_id` field and reordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabMoveToWorkspaceParams {
    pub tab_id: String,
    /// Destination workspace. May name the tab's own workspace, which reorders
    /// the tab within it.
    pub workspace_id: String,
    /// Position within the target workspace. Omitted appends to the end of the
    /// target workspace's tab bar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_index: Option<usize>,
    /// Focus the tab (and its workspace) after the move.
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabMovePaneRemap {
    pub previous_pane_id: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabMoveToWorkspaceResult {
    pub changed: bool,
    pub previous_tab_id: String,
    pub previous_workspace_id: String,
    pub tab: TabInfo,
    /// Final position of the tab within the target workspace.
    pub insert_index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_workspace_id: Option<String>,
    /// Public pane id reassignment for every pane that moved with the tab.
    pub pane_id_map: Vec<TabMovePaneRemap>,
    pub target_layout: Box<super::panes::PaneLayoutSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    pub focused: bool,
    pub pane_count: usize,
    pub agent_status: AgentStatus,
}
