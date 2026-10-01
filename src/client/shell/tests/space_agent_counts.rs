use super::*;
use crate::protocol::ClientShellWorktree;

/// The count token is not part of the default layout; these tests opt in with the rows the
/// feature documents, so they keep exercising the rendering path rather than the default.
fn space_count_config() -> Config {
    let mut config = Config::default();
    config.ui.sidebar.spaces.rows = vec![
        vec![
            crate::config::SpaceSidebarToken::StateIcon,
            crate::config::SpaceSidebarToken::Workspace,
        ],
        vec![
            crate::config::SpaceSidebarToken::AgentCount,
            crate::config::SpaceSidebarToken::Branch,
            crate::config::SpaceSidebarToken::GitStatus,
        ],
    ];
    config
}

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
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&space_count_config()));
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
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&space_count_config()));
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

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&space_count_config()));
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

/// The machines sidebar (drawn once a saved SSH machine exists) aggregates a collapsed group's
/// status over its hidden members as of v0.9.1 (#3781). The count has to aggregate the same
/// members, or a collapsed row can read "blocked" beside a count that ignores the blocked member.
#[test]
fn a_collapsed_group_on_a_saved_machine_sums_the_agent_counts_of_its_members() {
    use crate::client::endpoint::{
        ClientEndpointId, ClientEndpointStatus, ProfileId, SavedSshEndpoint,
    };

    let profile = SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Build".into(),
        target: "dev@build.example".into(),
        session: "agents".into(),
        enabled: true,
    };
    let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&space_count_config()));
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
    // Local carries a count no remote row can produce, so every badge on screen is attributable.
    let mut local = snapshot();
    local.workspaces[0].agent_count = 7;
    state.set_snapshot(Box::new(local));
    state.set_pane_surface(surface());
    let mut remote = grouped_snapshot(2, 3);
    remote.boot_id = "remote-boot".into();
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote));

    let expanded = state.compose(106, 28).expect("expanded remote group");
    let badges = agent_badges(&expanded);
    assert!(
        badges.contains(&2) && badges.contains(&3) && !badges.contains(&5),
        "expanded remote rows each show their own count: {badges:?} {:?}",
        sidebar_text(&expanded)
    );

    state
        .remote_collapsed_groups
        .entry(endpoint_id.clone())
        .or_default()
        .insert("repo".into());
    let mut collapsed_remote = grouped_snapshot(2, 3);
    collapsed_remote.boot_id = "remote-boot".into();
    collapsed_remote.revision = 2;
    state.set_endpoint_snapshot(&endpoint_id, Box::new(collapsed_remote));

    let collapsed = state.compose(106, 28).expect("collapsed remote group");
    let badges = agent_badges(&collapsed);
    assert!(
        badges.contains(&5),
        "the collapsed remote parent totals its group (2 + 3): {badges:?} {:?}",
        sidebar_text(&collapsed)
    );
    assert!(
        !badges.contains(&2) && !badges.contains(&3),
        "neither the parent's own count nor the hidden member's shows once collapsed: {badges:?}"
    );
}
