use super::*;
use crate::protocol::ClientShellWorktree;

fn grouped_snapshot(parent_count: usize, child_count: usize) -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    snapshot.workspaces[0].label = "repo".into();
    snapshot.workspaces[0].agent_count = parent_count;
    snapshot.workspaces[0].worktree = Some(ClientShellWorktree {
        key: "repo".into(),
        label: "repo".into(),
        is_linked_worktree: false,
    });
    snapshot.workspaces.push(ClientShellWorkspace {
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
        agent_count: child_count,
        agent_status: AgentStatus::Idle,
    });
    snapshot
}

fn sidebar_text(frame: &FrameData) -> String {
    frame
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<String>()
}

/// Every agent-count badge on screen, as numbers.
///
/// Read cell by cell rather than by substring: the badge glyph is double width, so the frame grid
/// holds a blank continuation cell between it and the digits and a plain `contains("🤖4")` reports
/// absence for a badge that is plainly on screen.
fn agent_badges(frame: &FrameData) -> Vec<usize> {
    let symbols = frame
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<Vec<_>>();
    let mut badges = Vec::new();
    for (index, symbol) in symbols.iter().enumerate() {
        if *symbol != "🤖" {
            continue;
        }
        let digits = symbols
            .iter()
            .skip(index + 1)
            .skip_while(|symbol| symbol.trim().is_empty())
            .take_while(|symbol| symbol.chars().all(|character| character.is_ascii_digit()))
            .copied()
            .collect::<String>();
        if let Ok(count) = digits.parse::<usize>() {
            badges.push(count);
        }
    }
    badges
}

/// The count has to survive the whole client-shell path -- wire field, token resolution, span
/// rendering -- and land on screen. Asserting the wire field alone would pass against a build that
/// never renders it, which is exactly the dead-config-key failure this feature had to avoid.
#[test]
fn space_rows_render_the_agent_count_from_the_wire() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut projected = snapshot();
    projected.workspaces[0].agent_count = 4;
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());

    let frame = state.compose(106, 20).expect("composed frame");
    assert_eq!(
        agent_badges(&frame),
        vec![4],
        "the space row shows its agent count: {:?}",
        sidebar_text(&frame)
    );

    // Zero is rendered rather than omitted, which is what keeps a row's height independent of how
    // many agents happen to be running.
    let mut none_running = state.snapshot.as_deref().expect("snapshot").clone();
    none_running.revision = 2;
    none_running.workspaces[0].agent_count = 0;
    let mut next_surface = surface();
    next_surface.projection_revision = 2;
    state.set_snapshot(Box::new(none_running));
    state.set_pane_surface(next_surface);
    let frame = state.compose(106, 20).expect("composed frame");
    assert_eq!(
        agent_badges(&frame),
        vec![0],
        "zero is shown, not hidden: {:?}",
        sidebar_text(&frame)
    );
}

/// A collapsed group parent stands in for its whole group, so its row must total the group rather
/// than report only its own panes -- otherwise collapsing a group appears to lose agents.
#[test]
fn a_collapsed_group_parent_sums_the_agent_counts_of_its_members() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(grouped_snapshot(2, 3)));
    state.set_pane_surface(surface());

    // Expanded: the parent shows its own count and the child shows its own.
    let expanded = state.compose(106, 20).expect("expanded group");
    assert_eq!(
        agent_badges(&expanded),
        vec![2, 3],
        "expanded rows each show their own count, and nothing is summed: {:?}",
        sidebar_text(&expanded)
    );

    let mut collapsed_snapshot = grouped_snapshot(2, 3);
    collapsed_snapshot.revision = 2;
    let mut collapsed_surface = surface();
    collapsed_surface.projection_revision = 2;
    state.collapsed_groups.insert("repo".into());
    state.set_snapshot(Box::new(collapsed_snapshot));
    state.set_pane_surface(collapsed_surface);

    let collapsed = state.compose(106, 20).expect("collapsed group");
    assert_eq!(
        agent_badges(&collapsed),
        vec![5],
        "the collapsed parent totals the group and the child row is hidden: {:?}",
        sidebar_text(&collapsed)
    );
}

/// The aggregate is scoped to one group. A space in a different repo must not be added in.
#[test]
fn a_collapsed_group_sums_only_its_own_members() {
    let mut projected = grouped_snapshot(2, 3);
    projected.workspaces.push(ClientShellWorkspace {
        workspace_id: "ws_3".into(),
        active_tab_id: "tab_ws3".into(),
        new_workspace_cwd: "/other".into(),
        number: 3,
        label: "other".into(),
        custom_label: false,
        branch: None,
        git_ahead_behind: None,
        tokens: Vec::new(),
        worktree: Some(ClientShellWorktree {
            key: "other-repo".into(),
            label: "other".into(),
            is_linked_worktree: false,
        }),
        focused: false,
        agent_count: 9,
        agent_status: AgentStatus::Idle,
    });

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.collapsed_groups.insert("repo".into());
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());

    let frame = state.compose(106, 24).expect("two groups");
    assert_eq!(
        agent_badges(&frame),
        vec![5, 9],
        "the repo group totals 2 + 3 and the unrelated space keeps its own 9, \
         with nothing folded across groups: {:?}",
        sidebar_text(&frame)
    );
}

/// The token is opt-out through config like any other, so a layout without it renders no count.
#[test]
fn a_space_layout_without_the_token_renders_no_count() {
    let mut config = Config::default();
    config.ui.sidebar.spaces.rows = vec![vec![
        crate::config::SpaceSidebarToken::StateIcon,
        crate::config::SpaceSidebarToken::Workspace,
    ]];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    let mut projected = snapshot();
    projected.workspaces[0].agent_count = 4;
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());

    let frame = state.compose(106, 20).expect("composed frame");
    assert!(
        agent_badges(&frame).is_empty(),
        "no count token configured: {:?}",
        sidebar_text(&frame)
    );
}
