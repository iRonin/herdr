use super::*;

/// A snapshot whose active workspace (`ws_1`) holds `tabs` tabs, plus a second
/// workspace `ws_2` the drag can target.
fn two_workspace_snapshot(tabs: usize) -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    snapshot
        .tabs
        .extend((2..=tabs).map(|number| ClientShellTab {
            tab_id: format!("tab_{number}"),
            workspace_id: "ws_1".into(),
            number,
            label: number.to_string(),
            custom_label: false,
            zoomed: false,
            focused: false,
            agent_status: AgentStatus::Idle,
        }));
    snapshot.workspaces.push(ClientShellWorkspace {
        workspace_id: "ws_2".into(),
        active_tab_id: "ws_2:1".into(),
        new_workspace_cwd: "/repo".into(),
        number: 2,
        label: "target".into(),
        custom_label: false,
        branch: Some("main".into()),
        git_ahead_behind: None,
        tokens: Vec::new(),
        worktree: None,
        focused: false,
        agent_count: 0,
        agent_status: AgentStatus::Idle,
    });
    snapshot
}

fn drag_state(tabs: usize, enabled: bool, wrap: bool) -> ClientShellState {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_drag_move_workspace = enabled;
    config.tab_bar_wrap = wrap;
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(two_workspace_snapshot(tabs)));
    state.set_pane_surface(surface());
    state
        .compose(106, 20)
        .expect("tab bar and two workspace entries");
    state
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> RawInputEvent {
    RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

fn drag_tab_onto_entry(
    state: &mut ClientShellState,
    source: Rect,
    entry: Rect,
) -> ClientShellInput {
    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        source.x + 1,
        source.y,
    )]);
    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        entry.x + 1,
        entry.y,
    )]);
    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        entry.x + 1,
        entry.y,
    )])
}

fn workspace_entry(state: &ClientShellState, workspace_id: &str) -> Rect {
    state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id.is_local() && hit.workspace_id == workspace_id)
        .map(|hit| hit.rect)
        .unwrap_or_else(|| panic!("workspace entry for {workspace_id}"))
}

fn moved_tab_target(outcome: &ClientShellInput) -> Option<(String, String)> {
    outcome.actions.iter().find_map(|action| match action {
        ClientShellAction::Endpoint { request, .. } => match &request.method {
            crate::api::schema::Method::TabMoveToWorkspace(params) => {
                Some((params.tab_id.clone(), params.workspace_id.clone()))
            }
            _ => None,
        },
        _ => None,
    })
}

/// The whole feature: press a tab, drag it onto another workspace's sidebar
/// entry, release — the tab moves to that workspace without taking focus.
/// Asserts the mid-drag state too, because the drag must arm a *move*, not a
/// reorder that happens to look like one on release.
#[test]
fn dragging_a_tab_onto_a_workspace_entry_moves_it_when_enabled() {
    let mut state = drag_state(3, true, false);
    let source = state.hits.tabs[0].0;
    let entry = workspace_entry(&state, "ws_2");

    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        source.x + 1,
        source.y,
    )]);
    let drag = state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        entry.x + 1,
        entry.y,
    )]);
    assert!(drag.repaint, "hovering a drop target must repaint");
    assert!(matches!(
        &state.chrome_drag,
        Some(ClientChromeDrag::Tab {
            tab_id,
            workspace_id,
            insert_index: None,
            move_target: Some(target),
        }) if tab_id == "tab_1"
            && workspace_id == "ws_1"
            && target == "ws_2"
    ));

    let release = state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        entry.x + 1,
        entry.y,
    )]);
    assert_eq!(
        moved_tab_target(&release),
        Some(("tab_1".to_string(), "ws_2".to_string())),
        "the release must send tab.move_to_workspace for the dragged tab"
    );
    // Dropping on an entry appends without switching the user's view.
    let request = release.actions.iter().find_map(|action| match action {
        ClientShellAction::Endpoint { request, .. } => match &request.method {
            crate::api::schema::Method::TabMoveToWorkspace(params) => Some(params.clone()),
            _ => None,
        },
        _ => None,
    });
    let params = request.expect("move request");
    assert_eq!(params.insert_index, None);
    assert!(!params.focus);
}

/// Control for the gate: with `ui.tab_drag_move_workspace` off, the identical
/// gesture must not move anything. The drag cannot even start off the tab row,
/// so the release resolves to the plain click-through focus, exactly as it did
/// before the feature existed.
#[test]
fn dragging_a_tab_onto_a_workspace_entry_is_ignored_when_disabled() {
    let mut state = drag_state(3, false, false);
    let source = state.hits.tabs[0].0;
    let entry = workspace_entry(&state, "ws_2");

    let release = drag_tab_onto_entry(&mut state, source, entry);

    assert_eq!(
        moved_tab_target(&release),
        None,
        "no move may be sent when the setting is off"
    );
    assert!(
        !matches!(
            &state.chrome_drag,
            Some(ClientChromeDrag::Tab {
                move_target: Some(_),
                ..
            })
        ),
        "the drag must never arm a cross-workspace move while disabled"
    );
    // Click-through behaviour is unchanged: an off-bar release focuses the tab.
    let focused = release.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(&request.method, crate::api::schema::Method::TabFocus(target) if target.tab_id == "tab_1")
    ));
    assert!(focused);
}

/// The drag has to be able to START off the tab row (the workspace entries are
/// below it), but only when the feature is on — otherwise this would change
/// the plain click behaviour for everyone.
#[test]
fn the_drag_can_start_off_the_tab_row_only_when_enabled() {
    let mut enabled = drag_state(3, true, false);
    let source = enabled.hits.tabs[0].0;
    let off_row = source.y + 4;
    enabled.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        source.x + 1,
        source.y,
    )]);
    enabled.handle_raw_events(vec![mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        source.x + 1,
        off_row,
    )]);
    assert!(matches!(
        &enabled.chrome_drag,
        Some(ClientChromeDrag::Tab {
            insert_index: None,
            move_target: None,
            ..
        })
    ));
    let release = enabled.handle_raw_events(vec![mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        source.x + 1,
        off_row,
    )]);
    assert!(
        release.actions.is_empty(),
        "no entry under the pointer: no move"
    );

    let mut disabled = drag_state(3, false, false);
    let source = disabled.hits.tabs[0].0;
    let off_row = source.y + 4;
    disabled.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        source.x + 1,
        source.y,
    )]);
    disabled.handle_raw_events(vec![mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        source.x + 1,
        off_row,
    )]);
    assert!(
        disabled.chrome_drag.is_none(),
        "control: with the gate off, a drag off the tab row never starts"
    );
}

/// The tab's own workspace is never a move target — dropping there must not
/// silently reorder the tab to the end.
#[test]
fn dropping_on_the_own_workspace_entry_is_a_noop() {
    let mut state = drag_state(3, true, false);
    let source = state.hits.tabs[0].0;
    let own_entry = workspace_entry(&state, "ws_1");

    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        source.x + 1,
        source.y,
    )]);
    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        own_entry.x + 1,
        own_entry.y,
    )]);
    assert!(matches!(
        &state.chrome_drag,
        Some(ClientChromeDrag::Tab {
            move_target: None,
            ..
        })
    ));
    let release = state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        own_entry.x + 1,
        own_entry.y,
    )]);
    assert!(
        release.actions.is_empty(),
        "own-workspace drop: no move and no reorder may be sent"
    );
}

/// ui.tab_bar_wrap composition: a tab on a wrapped lower row is as draggable
/// as one on the single-row bar.
#[test]
fn a_tab_from_a_wrapped_lower_row_moves_to_the_workspace() {
    let mut state = drag_state(9, true, true);
    state
        .compose(80, 20)
        .expect("wrapped tab bar with workspace entries");
    let top_row = state
        .hits
        .tabs
        .first()
        .map(|(rect, _)| rect.y)
        .expect("at least one tab");
    let (source, source_id) = state
        .hits
        .tabs
        .iter()
        .find(|(rect, _)| rect.y > top_row)
        .map(|(rect, id)| (*rect, id.clone()))
        .expect("a tab on a wrapped lower row");
    let entry = workspace_entry(&state, "ws_2");

    let release = drag_tab_onto_entry(&mut state, source, entry);

    assert_eq!(
        moved_tab_target(&release),
        Some((source_id, "ws_2".into())),
        "the wrapped-row tab must move to the hovered workspace"
    );
}

/// Machines: a tab lives on the server that rendered it. Dropping it onto
/// ANOTHER machine's workspace entry must be ignored — never routed to that
/// machine's server.
#[test]
fn dropping_on_another_machines_workspace_is_ignored() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_drag_move_workspace = true;
    let mut state = ClientShellState::new(config);
    let profile = crate::client::endpoint::SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Build".into(),
        target: "dev@build.example".into(),
        session: "agents".into(),
        enabled: true,
    };
    let remote_id = crate::client::endpoint::ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(
        &remote_id,
        crate::client::endpoint::ClientEndpointStatus::Online,
    );
    state.set_snapshot(Box::new(two_workspace_snapshot(3)));
    state.set_pane_surface(surface());
    let mut remote = two_workspace_snapshot(2);
    remote.boot_id = "remote-boot".into();
    remote.workspaces[0].label = "remote-workspace".into();
    state.set_endpoint_snapshot(&remote_id, Box::new(remote));
    state
        .compose(106, 24)
        .expect("machines sidebar with both workspaces");

    let source = state
        .hits
        .tabs
        .first()
        .map(|(rect, _)| *rect)
        .expect("local tab hit");
    let remote_entry = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id == remote_id)
        .map(|hit| hit.rect)
        .expect("remote machine workspace entry");
    assert!(
        remote_entry.y > source.y,
        "entry must sit below the tab row"
    );

    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        source.x + 1,
        source.y,
    )]);
    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        remote_entry.x + 1,
        remote_entry.y,
    )]);
    assert!(
        !matches!(
            &state.chrome_drag,
            Some(ClientChromeDrag::Tab {
                move_target: Some(_),
                ..
            })
        ),
        "another machine's entry must never arm a move target"
    );
    let release = state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        remote_entry.x + 1,
        remote_entry.y,
    )]);
    assert!(
        release.actions.is_empty(),
        "the drop must be ignored, not sent to the remote server"
    );
}

/// The cross-workspace drag has exactly one entry point: the left mouse-down
/// on the tab row. On a bottom tab bar in a mode-bar mode the covered row's
/// tab hits are removed entirely, so the gesture cannot even start — asserted
/// here rather than assumed, because that guarantee lives in the composer,
/// far from the drag handler it protects.
#[test]
fn bottom_mode_bar_prevents_the_move_drag_from_starting() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_drag_move_workspace = true;
    config.tab_bar_position = crate::config::TabBarPositionConfig::Bottom;
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(two_workspace_snapshot(3)));
    state.set_pane_surface(surface());
    state.mode = ClientShellMode::Prefix;
    state
        .compose(106, 20)
        .expect("bottom tab bar under the mode bar");
    let entry = workspace_entry(&state, "ws_2");
    let covered_row = state.layout(106, 20).tab_bar.y;
    assert!(
        state
            .hits
            .tabs
            .iter()
            .all(|(rect, _)| rect.y != covered_row),
        "control: the mode bar must have swallowed the covered tab row's hits"
    );

    // A press where a tab used to be must not arm anything, and a drag from
    // that press must not invent a move.
    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        3,
        covered_row,
    )]);
    assert!(state.tab_press.is_none());
    let release = state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        entry.x + 1,
        entry.y,
    )]);
    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        entry.x + 1,
        entry.y,
    )]);
    assert!(release.actions.is_empty());
    assert!(state.chrome_drag.is_none());
}

/// The drag-update path never arms both outcomes at once, but that invariant
/// lives in a different function from the dispatch. A drag carrying BOTH must
/// dispatch the cross-workspace move — downgrading it to an in-bar reorder
/// would relocate nothing while silently reordering a live layout.
#[test]
fn a_drag_carrying_both_outcomes_dispatches_the_move() {
    let mut state = drag_state(3, true, false);
    state.chrome_drag = Some(ClientChromeDrag::Tab {
        tab_id: "tab_1".into(),
        workspace_id: "ws_1".into(),
        insert_index: Some(2),
        move_target: Some("ws_2".into()),
    });

    let release = state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        1,
        1,
    )]);

    assert_eq!(
        moved_tab_target(&release),
        Some(("tab_1".to_string(), "ws_2".to_string())),
        "the move must win over the in-bar reorder"
    );
    let reordered = release.actions.iter().any(|action| {
        matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(&request.method, crate::api::schema::Method::TabMove(_))
        )
    });
    assert!(!reordered, "no reorder may ride along");
}

/// A server that does not advertise `tab.move_to_workspace` (a stock v0.9.1
/// peer) must be refused visibly — an unsupported notice — and the request
/// must not be sent.
#[test]
fn an_unsupporting_server_is_refused_visibly() {
    let mut state = drag_state(3, true, false);
    state.set_endpoint_methods(Some(vec![
        "tab.move".into(),
        "tab.focus".into(),
        "tab.close".into(),
    ]));
    let source = state.hits.tabs[0].0;
    let entry = workspace_entry(&state, "ws_2");

    let release = drag_tab_onto_entry(&mut state, source, entry);

    assert!(
        release.actions.is_empty(),
        "the move must not be sent to a server that cannot execute it"
    );
    let notice = state
        .visible_endpoint_notice
        .as_ref()
        .expect("an unsupported-action notice");
    assert_eq!(notice.key.kind, ClientEndpointNoticeKind::Unsupported);
    assert_eq!(notice.key.code, "tab.move_to_workspace");
}

/// The hovered entry highlights as the drop target while the drag is armed —
/// without feedback the user is dropping blind.
#[test]
fn the_hovered_entry_highlights_as_the_drop_target() {
    let mut state = drag_state(3, true, false);
    let source = state.hits.tabs[0].0;
    let entry = workspace_entry(&state, "ws_2");
    let plain_frame = state.compose(106, 20).expect("plain sidebar");
    let plain_bg = plain_frame.cells
        [usize::from(entry.y) * usize::from(plain_frame.width) + usize::from(entry.x)]
    .bg;

    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        source.x + 1,
        source.y,
    )]);
    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        entry.x + 1,
        entry.y,
    )]);
    let hovered_frame = state.compose(106, 20).expect("hovered sidebar");
    let hovered_bg = hovered_frame.cells
        [usize::from(entry.y) * usize::from(hovered_frame.width) + usize::from(entry.x)]
    .bg;
    assert_ne!(
        plain_bg, hovered_bg,
        "the hovered entry must render with a distinct background"
    );
    // And the neighbouring (non-hovered) entry keeps its plain background.
    let other = workspace_entry(&state, "ws_1");
    let other_plain = plain_frame.cells
        [usize::from(other.y) * usize::from(plain_frame.width) + usize::from(other.x)]
    .bg;
    let other_hovered = hovered_frame.cells
        [usize::from(other.y) * usize::from(hovered_frame.width) + usize::from(other.x)]
    .bg;
    assert_eq!(other_plain, other_hovered);
}

/// Composition with `ui.tab_close_button`: the close marker is modifier-armed
/// on mouse-down, the move is an unmodified drag — so with both features on, a
/// plain drag from a tab that HAS a marker still moves, and never closes.
#[test]
fn the_drag_composes_with_the_close_button() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_drag_move_workspace = true;
    config.tab_close_button = true;
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(two_workspace_snapshot(3)));
    state.set_pane_surface(surface());
    state
        .compose(106, 20)
        .expect("tab bar with close markers and workspace entries");
    let source = state.hits.tabs[0].0;
    let entry = workspace_entry(&state, "ws_2");

    let release = drag_tab_onto_entry(&mut state, source, entry);

    assert_eq!(
        moved_tab_target(&release),
        Some(("tab_1".to_string(), "ws_2".to_string()))
    );
    let closed = release.actions.iter().any(|action| {
        matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(&request.method, crate::api::schema::Method::TabClose(_))
        )
    });
    assert!(!closed, "an unmodified drag must never close the tab");
}
