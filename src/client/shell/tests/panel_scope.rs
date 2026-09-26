use super::*;

fn agent(
    pane_id: &str,
    workspace_id: &str,
    tab_id: &str,
    name: &str,
    status: AgentStatus,
    sequence: u64,
) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: workspace_id.into(),
        tab_id: tab_id.into(),
        name: Some(name.into()),
        display_agent: Some(name.into()),
        agent: Some("pi".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: sequence,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: pane_id == "pane_1",
    }
}

fn snapshot_with_agents_in_two_workspaces() -> ClientShellSnapshot {
    let mut projected = snapshot();
    projected.workspaces.push(ClientShellWorkspace {
        workspace_id: "ws_2".into(),
        active_tab_id: "tab_2".into(),
        new_workspace_cwd: "/other".into(),
        number: 2,
        label: "other-space".into(),
        custom_label: true,
        branch: Some("other".into()),
        git_ahead_behind: None,
        tokens: Vec::new(),
        worktree: None,
        focused: false,
        agent_count: 1,
        agent_status: AgentStatus::Working,
    });
    projected.tabs.push(ClientShellTab {
        tab_id: "tab_2".into(),
        workspace_id: "ws_2".into(),
        number: 1,
        label: "1".into(),
        custom_label: false,
        zoomed: false,
        focused: false,
        agent_status: AgentStatus::Working,
    });
    projected.panes.push(ClientShellPane {
        pane_id: "pane_2".into(),
        workspace_id: "ws_2".into(),
        tab_id: "tab_2".into(),
        label: None,
        cwd: Some("/other".into()),
        foreground_cwd: Some("/other".into()),
        focused: false,
        right_click_passthrough: false,
    });
    projected.agents = vec![
        agent(
            "pane_1",
            "ws_1",
            "tab_1",
            "active-agent",
            AgentStatus::Blocked,
            1,
        ),
        agent(
            "pane_2",
            "ws_2",
            "tab_2",
            "background-agent",
            AgentStatus::Working,
            2,
        ),
    ];
    projected.workspaces[0].agent_count = 1;
    projected.workspaces[0].agent_status = AgentStatus::Blocked;
    projected
}

#[test]
fn current_scope_agent_panel_tracks_only_the_focused_workspace() {
    let mut projected = snapshot_with_agents_in_two_workspaces();
    projected.agents[0].agent_status = AgentStatus::Working;
    projected.agents.push(agent(
        "pane_3",
        "ws_1",
        "tab_1",
        "urgent-active-agent",
        AgentStatus::Blocked,
        3,
    ));

    let all = agent_sidebar::ordered_agent_pane_ids(
        &projected,
        crate::config::AgentPanelSortConfig::Spaces,
        crate::config::AgentPanelScopeConfig::All,
        None,
    );
    assert_eq!(all, ["pane_1", "pane_2", "pane_3"]);

    let current = agent_sidebar::ordered_agent_pane_ids(
        &projected,
        crate::config::AgentPanelSortConfig::Spaces,
        crate::config::AgentPanelScopeConfig::Current,
        None,
    );
    assert_eq!(current, ["pane_3", "pane_1"]);

    projected.agent_view_label = Some("custom".into());
    projected.agent_order = vec!["pane_2".into(), "pane_1".into(), "pane_3".into()];
    let no_sort_view = crate::api::schema::AgentViewSetParams {
        source: "test".into(),
        label: Some("custom".into()),
        filter: None,
        sort: Vec::new(),
    };
    let viewed_current = agent_sidebar::ordered_agent_pane_ids(
        &projected,
        crate::config::AgentPanelSortConfig::Spaces,
        crate::config::AgentPanelScopeConfig::Current,
        Some(&no_sort_view),
    );
    assert_eq!(viewed_current, ["pane_3", "pane_1"]);

    let mut config = Config::default();
    config.ui.agent_panel_sort = crate::config::AgentPanelSortConfig::Spaces;
    config.ui.agent_panel_scope = crate::config::AgentPanelScopeConfig::Current;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(projected.clone()));
    state.set_test_endpoint_agent_view(&ClientEndpointId::Local, Some(no_sort_view.clone()));
    state.set_pane_surface(surface());
    state.compose(106, 30).expect("current-space Agent view");
    let rendered = state
        .hits
        .agents
        .iter()
        .map(|(_, pane_id)| pane_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(rendered, ["pane_3", "pane_1"]);
    let aggregate = aggregate_navigation::aggregate_agent_rows(
        &state.endpoints,
        &state.active_endpoint_id,
        state.config.agent_panel_sort,
        state.config.agent_panel_scope,
    )
    .into_iter()
    .map(|row| row.agent.pane_id.as_str())
    .collect::<Vec<_>>();
    assert_eq!(aggregate, rendered);
    assert!(matches!(
        state.endpoint_method_for_action(crate::input::KeybindAction::FocusAgent(0), false),
        Some(crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget {
            pane_id
        })) if pane_id == "pane_3"
    ));

    let mut explicit_sort_view = no_sort_view.clone();
    explicit_sort_view.sort = vec![crate::api::schema::AgentViewSort {
        field: crate::api::schema::AgentViewSortField::Builtin(
            crate::api::schema::AgentViewBuiltinSortField::StateChangeSeq,
        ),
        order: crate::api::schema::AgentViewSortOrder::Asc,
    }];
    let explicitly_sorted = agent_sidebar::ordered_agent_pane_ids(
        &projected,
        crate::config::AgentPanelSortConfig::Spaces,
        crate::config::AgentPanelScopeConfig::Current,
        Some(&explicit_sort_view),
    );
    assert_eq!(explicitly_sorted, ["pane_1", "pane_3"]);

    projected.focused_workspace_id = Some("ws_2".into());
    let switched = agent_sidebar::ordered_agent_pane_ids(
        &projected,
        crate::config::AgentPanelSortConfig::Spaces,
        crate::config::AgentPanelScopeConfig::Current,
        Some(&no_sort_view),
    );
    assert_eq!(switched, ["pane_2"]);
}

#[test]
fn mobile_header_counts_all_workspaces_while_current_scope_limits_panel_targets() {
    let mut config = Config::default();
    config.ui.agent_panel_scope = crate::config::AgentPanelScopeConfig::Current;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot_with_agents_in_two_workspaces()));
    state.set_pane_surface(surface());

    let header = state.compose(44, 20).expect("mobile header");
    let header_rows = frame_rows(&header);
    assert!(
        header_rows[1].contains("1 blocked"),
        "active-workspace agent must be counted: {:?}",
        header_rows[1]
    );
    assert!(
        header_rows[1].contains("1 working"),
        "background-workspace agent must stay in the global count: {:?}",
        header_rows[1]
    );

    state.mode = ClientShellMode::Navigate;
    state.compose(44, 20).expect("mobile switcher");
    let agent_targets = state
        .hits
        .mobile_targets
        .iter()
        .filter_map(|(_, target)| match target {
            ClientMobileTarget::Agent { pane_id, .. } => Some(pane_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(agent_targets, ["pane_1"]);
}

#[test]
fn configured_priority_space_cycle_enters_and_leaves_current_scope() {
    let path = std::env::temp_dir().join(format!(
        "herdr-panel-mode-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    let mut config = Config::default();
    config.ui.agent_panel_sort = crate::config::AgentPanelSortConfig::Priority;
    config.ui.agent_panel_scope = crate::config::AgentPanelScopeConfig::All;
    config.ui.agent_panel_modes = vec![
        crate::config::AgentPanelModeConfig::Priority,
        crate::config::AgentPanelModeConfig::Space,
    ];
    let shell_config = ClientShellConfig::from_config(&config).with_preferences_path(path.clone());
    let mut state = ClientShellState::new(shell_config);
    state.set_snapshot(Box::new(snapshot_with_agents_in_two_workspaces()));
    state.set_pane_surface(surface());

    state.compose(106, 30).expect("priority panel");
    let first_toggle = state.hits.agent_sort_toggle;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: first_toggle.x,
        row: first_toggle.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert_eq!(
        state.config.agent_panel_scope,
        crate::config::AgentPanelScopeConfig::Current
    );
    assert_eq!(
        state.config.agent_panel_sort,
        crate::config::AgentPanelSortConfig::Priority
    );

    let reloaded = ClientShellState::new(
        ClientShellConfig::from_config(&config).with_preferences_path(path.clone()),
    );
    assert_eq!(
        reloaded.config.agent_panel_scope,
        crate::config::AgentPanelScopeConfig::Current
    );
    assert!(reloaded.agent_panel_scope_manual);

    let space_frame = state.compose(106, 30).expect("space panel");
    assert!(
        frame_rows(&space_frame)
            .iter()
            .any(|row| row.contains("space")),
        "space mode label must be visible"
    );
    let second_toggle = state.hits.agent_sort_toggle;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: second_toggle.x,
        row: second_toggle.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert_eq!(
        state.config.agent_panel_scope,
        crate::config::AgentPanelScopeConfig::All
    );
    assert_eq!(
        state.config.agent_panel_sort,
        crate::config::AgentPanelSortConfig::Priority
    );
    let priority_frame = state.compose(106, 30).expect("priority panel restored");
    assert!(
        frame_rows(&priority_frame)
            .iter()
            .any(|row| row.contains("priority")),
        "priority mode label must be restored"
    );
    std::fs::remove_file(path).expect("remove panel mode preferences");
}
