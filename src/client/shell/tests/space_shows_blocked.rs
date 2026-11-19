//! A space (and a collapsed worktree group) shows blocked when ANY agent in it is blocked, whether
//! or not the user has already read that block, and whatever else the space holds. The agent list
//! is a different surface: it still sinks a read block below an unread finish.
//!
//! Every case here reads what the client actually draws (or the shared status function the row is
//! drawn from), because the failure it guards is a space row that looks calm while an agent in it
//! is stuck waiting for the user.

use super::*;
use crate::protocol::ClientShellWorktree;
use std::collections::HashSet;

/// `(status, read)`: a blocked agent is "read" when the user has already looked at it.
type AgentSpec = (AgentStatus, bool);

const READ_BLOCKED: AgentSpec = (AgentStatus::Blocked, true);
const UNREAD_BLOCKED: AgentSpec = (AgentStatus::Blocked, false);
const UNREAD_DONE: AgentSpec = (AgentStatus::Done, false);
const WORKING: AgentSpec = (AgentStatus::Working, false);
const READ_IDLE: AgentSpec = (AgentStatus::Idle, false);
const UNKNOWN: AgentSpec = (AgentStatus::Unknown, false);

fn agents_in(workspace_id: &str, first_pane: usize, specs: &[AgentSpec]) -> Vec<ClientShellAgent> {
    specs
        .iter()
        .enumerate()
        .map(|(index, (status, read))| {
            let tokens = if *read {
                vec![(
                    crate::protocol::CLIENT_SHELL_BLOCKED_READ_TOKEN.into(),
                    "1".into(),
                )]
            } else {
                Vec::new()
            };
            let mut agent = test_agent(*status, tokens);
            agent.workspace_id = workspace_id.into();
            agent.pane_id = format!("pane_{}", first_pane + index);
            agent.name = Some(format!("agent-{}", first_pane + index));
            agent.focused = first_pane + index == 1;
            agent
        })
        .collect()
}

/// The `ws_1` space with one agent per spec, each in its own pane of tab `tab_1`.
fn space_snapshot(specs: &[AgentSpec]) -> ClientShellSnapshot {
    let mut projected = snapshot();
    // The wire aggregate is deliberately a calm value no case expects: the row must follow the
    // agents it holds, not a stale aggregate.
    projected.workspaces[0].agent_status = AgentStatus::Unknown;
    projected.agents = agents_in("ws_1", 1, specs);
    for index in 2..=specs.len() {
        let mut pane = projected.panes[0].clone();
        pane.pane_id = format!("pane_{index}");
        pane.focused = false;
        projected.panes.push(pane);
    }
    projected
}

fn space_config() -> Config {
    let mut config = Config::default();
    config.ui.status_indicators = crate::config::StatusIndicatorStyle::Dots;
    config.ui.sidebar.spaces.rows = vec![
        vec![
            crate::config::SpaceSidebarToken::StateIcon,
            crate::config::SpaceSidebarToken::Workspace,
        ],
        vec![crate::config::SpaceSidebarToken::StateText],
    ];
    config
}

struct SpaceRow {
    mark: String,
    colour: ratatui::style::Color,
    state_text: String,
}

/// What the space row for `ws_1` draws in a composed frame: its mark, the mark's colour and its
/// state text.
fn space_row(state: &mut ClientShellState) -> SpaceRow {
    let frame = state.compose(106, 30).expect("sidebar frame");
    let rect = state.hits.workspaces.first().expect("a space row").rect;
    let buffer = frame.to_ratatui_buffer().expect("frame buffer");
    // The first row of an un-indented space starts one cell in, with the state icon.
    let cell = &buffer[(rect.x + 1, rect.y)];
    let rows = frame_rows(&frame);
    let state_text = rows[usize::from(rect.y) + 1]
        .chars()
        .skip(usize::from(rect.x))
        .take(usize::from(rect.width))
        .collect::<String>()
        .trim()
        .to_owned();
    SpaceRow {
        mark: cell.symbol().to_owned(),
        colour: cell.fg,
        state_text,
    }
}

/// A client showing exactly these agents. The snapshot is installed after the surface so that it
/// reaches the renderer as given: the client's own first-snapshot baseline and the "this pane is
/// on screen, so it is read" rule would otherwise turn an unread finish or an unread block into a
/// read one before it is drawn, and that rule is not what these cases are about.
fn state_for(specs: &[AgentSpec], config: Config) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(space_snapshot(specs)));
    state.set_pane_surface(surface());
    state.snapshot = Some(Box::new(space_snapshot(specs)));
    state
}

#[test]
fn a_space_row_shows_blocked_whatever_else_the_space_holds() {
    // (agents in the space, mark, state text). Each blocked case is also tried with its agents in
    // the opposite order, because the winner of a tie must not depend on which agent comes last.
    let cases: [(&[AgentSpec], &str, &str); 8] = [
        (&[READ_BLOCKED, UNREAD_DONE], "○", "blocked"),
        (&[READ_BLOCKED, WORKING], "○", "blocked"),
        (&[READ_BLOCKED, READ_IDLE], "○", "blocked"),
        (&[READ_BLOCKED, UNKNOWN], "○", "blocked"),
        (&[READ_BLOCKED, UNREAD_DONE, WORKING], "○", "blocked"),
        // An unread block anywhere in the space makes the whole space draw the unread mark.
        (&[UNREAD_BLOCKED, READ_BLOCKED], "●", "blocked"),
        (&[READ_BLOCKED, UNREAD_BLOCKED], "●", "blocked"),
        (&[READ_BLOCKED, UNREAD_DONE, UNREAD_BLOCKED], "●", "blocked"),
    ];
    for (specs, mark, state_text) in cases {
        let mirrored = specs.iter().rev().copied().collect::<Vec<_>>();
        for specs in [specs, mirrored.as_slice()] {
            let mut state = state_for(specs, space_config());
            let row = space_row(&mut state);
            let label = format!("{specs:?}");
            assert_eq!(row.mark, mark, "mark for {label}");
            assert_eq!(
                row.colour, state.config.palette.red,
                "a blocked space is red whatever its mark: {label}"
            );
            assert_eq!(row.state_text, state_text, "state text for {label}");
        }
    }
}

/// Controls: with no blocked agent the old ranking holds (an unread finish above work, work
/// above read idle), so the blocked cases above cannot pass by painting every space red.
#[test]
fn a_space_without_a_blocked_agent_keeps_its_own_ranking() {
    let cases: [(&[AgentSpec], AgentStatus, &str); 3] = [
        (&[WORKING, UNREAD_DONE], AgentStatus::Done, "done"),
        (&[READ_IDLE, WORKING], AgentStatus::Working, "working"),
        (&[READ_IDLE, UNKNOWN], AgentStatus::Idle, "idle"),
    ];
    for (specs, status, state_text) in cases {
        let mut state = state_for(specs, space_config());
        let row = space_row(&mut state);
        assert_eq!(
            row.mark,
            status_icon(status, crate::config::StatusIndicatorStyle::Dots),
            "mark for {specs:?}"
        );
        assert_eq!(
            row.colour,
            status_color(status, &state.config.palette),
            "{specs:?}"
        );
        assert_eq!(row.state_text, state_text, "{specs:?}");
    }
}

/// The space row follows its agents, but the agent list is its own surface: a read block still
/// sinks below an unread finish there, in the same frame where the space is blocked.
#[test]
fn the_agent_list_still_sinks_a_read_block_while_its_space_shows_blocked() {
    let mut config = space_config();
    config.ui.agent_panel_sort = crate::config::AgentPanelSortConfig::Priority;
    config.ui.sidebar.agents.rows = vec![vec![
        crate::config::AgentSidebarToken::StateIcon,
        crate::config::AgentSidebarToken::Agent,
    ]];
    let mut state = state_for(&[READ_BLOCKED, UNREAD_DONE], config);

    let row = space_row(&mut state);
    assert_eq!(
        (row.mark.as_str(), row.colour),
        ("○", state.config.palette.red)
    );
    assert_eq!(row.state_text, "blocked");

    // pane_1 is the read-blocked agent and pane_2 the unread finish.
    assert_eq!(
        state
            .hits
            .agents
            .first()
            .map(|(_, pane_id)| pane_id.as_str()),
        Some("pane_2"),
        "the unread finish must still be listed above the read block"
    );
    assert_eq!(
        state
            .hits
            .agents
            .get(1)
            .map(|(_, pane_id)| pane_id.as_str()),
        Some("pane_1")
    );
}

/// A parent worktree space and one linked worktree space of the same repository, with the agents
/// of each. The parent row is the group row once the group is collapsed.
fn grouped_snapshot(parent: &[AgentSpec], child: &[AgentSpec]) -> ClientShellSnapshot {
    let mut projected = snapshot();
    projected.workspaces[0].label = "repo".into();
    projected.workspaces[0].agent_status = AgentStatus::Unknown;
    projected.workspaces[0].worktree = Some(ClientShellWorktree {
        key: "repo".into(),
        label: "repo".into(),
        is_linked_worktree: false,
    });
    projected.workspaces.push(ClientShellWorkspace {
        workspace_id: "ws_2".into(),
        active_tab_id: "tab_ws2".into(),
        new_workspace_cwd: "/repo/feature".into(),
        number: 2,
        label: "repo-feature".into(),
        custom_label: false,
        branch: Some("worktree/feature".into()),
        git_ahead_behind: None,
        tokens: Vec::new(),
        worktree: Some(ClientShellWorktree {
            key: "repo".into(),
            label: "repo".into(),
            is_linked_worktree: true,
        }),
        focused: false,
        agent_count: child.len(),
        agent_status: AgentStatus::Unknown,
    });
    projected.agents = agents_in("ws_1", 1, parent);
    projected
        .agents
        .extend(agents_in("ws_2", 1 + parent.len(), child));
    projected
}

#[test]
fn a_collapsed_worktree_group_row_shows_blocked_when_any_member_agent_is_blocked() {
    use crate::config::StatusIndicatorStyle::Dots;
    let collapsed = HashSet::from(["repo".to_string()]);
    // (parent agents, child agents, mark). The mirrored case swaps which space holds the block.
    let cases: [(&[AgentSpec], &[AgentSpec], &str); 5] = [
        (&[UNREAD_DONE], &[READ_BLOCKED], "○"),
        (&[WORKING], &[READ_BLOCKED], "○"),
        (&[READ_BLOCKED, UNREAD_DONE], &[WORKING], "○"),
        (&[UNREAD_BLOCKED], &[READ_BLOCKED], "●"),
        (&[READ_BLOCKED], &[UNREAD_BLOCKED], "●"),
    ];
    for (parent, child, mark) in cases {
        for (parent, child) in [(parent, child), (child, parent)] {
            let projected = grouped_snapshot(parent, child);
            let status = super::super::sidebar::displayed_workspace_status(
                &projected,
                &projected.workspaces[0],
                &collapsed,
            );
            let label = format!("parent {parent:?}, child {child:?}");
            assert_eq!(status.status(), AgentStatus::Blocked, "{label}");
            assert_eq!(status.icon(Dots), mark, "{label}");
        }
    }

    // Controls: expanded, the parent row speaks for its own space only; collapsed, a group with
    // no blocked agent still ranks as before.
    let projected = grouped_snapshot(&[UNREAD_DONE], &[READ_BLOCKED]);
    let expanded = super::super::sidebar::displayed_workspace_status(
        &projected,
        &projected.workspaces[0],
        &HashSet::new(),
    );
    assert_eq!(expanded.status(), AgentStatus::Done);
    let projected = grouped_snapshot(&[WORKING], &[UNREAD_DONE]);
    let calm = super::super::sidebar::displayed_workspace_status(
        &projected,
        &projected.workspaces[0],
        &collapsed,
    );
    assert_eq!(calm.status(), AgentStatus::Done);
}
