use super::*;

#[test]
fn selection_repaint_cadence_keeps_one_deadline_and_flushes_when_input_stops() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let now = std::time::Instant::now();
    let ms = std::time::Duration::from_millis;
    state.last_composed_at = Some(now);
    for elapsed in [1, 4, 8, 12, 15] {
        assert!(!state.request_selection_drag_repaint(now + ms(elapsed)));
        assert_eq!(state.selection_repaint_deadline, Some(now + ms(16)));
    }
    assert_eq!(state.timer_delay(now + ms(8)), ms(8));
    assert!(!state.tick_selection_autoscroll(now + ms(15)).repaint);
    assert!(state.tick_selection_autoscroll(now + ms(16)).repaint);
    assert!(state.selection_repaint_deadline.is_none());
    assert!(!state.tick_selection_autoscroll(now + ms(17)).repaint);
}

#[test]
fn selection_repaint_cadence_allows_immediate_paint_when_due() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let now = std::time::Instant::now();
    assert!(state.request_selection_drag_repaint(now));
    state.last_composed_at = Some(now);
    assert!(state.request_selection_drag_repaint(now + std::time::Duration::from_millis(16)));
    assert!(state.selection_repaint_deadline.is_none());
}

#[test]
fn selection_repaint_cadence_does_not_leave_work_after_another_composition() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let now = std::time::Instant::now();
    state.selection_repaint_deadline = Some(now);
    state.compose(106, 20).expect("frame");
    assert!(state.selection_repaint_deadline.is_none());
    assert!(!state.tick_selection_autoscroll(now).repaint);
}

#[test]
fn selection_release_copies_latest_position_before_deferred_paint() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("pane frame");
    let pane = state.hits.panes[0].clone();
    let mut mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x,
        row: pane.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    };
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 2;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    state.selection_repaint_deadline = Some(std::time::Instant::now());
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    let release = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(release.repaint);
    assert!(matches!(
        &release.actions[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(&request.method,
                crate::api::schema::Method::PaneSelectionRead(params)
                    if params.cursor == crate::api::schema::PaneTextPoint { row: 0, col: 2 })
    ));
    state.compose(106, 20).expect("release frame");
    assert!(state.selection_repaint_deadline.is_none());
}

#[test]
fn ctrl_click_routes_link_activation_through_endpoint_then_client_host() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("pane frame");
    let pane = state.hits.panes[0].clone();
    let down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x + 2,
        row: pane.inner_rect.y + 1,
        modifiers: KeyModifiers::CONTROL,
    };
    let activate = state.handle_raw_events(vec![RawInputEvent::Mouse(down)]);
    let [ClientShellAction::Endpoint { request, .. }] = &activate.actions[..] else {
        panic!("expected link activation request");
    };
    let request_id = request.id.clone();
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneLinkActivate(params)
            if params.pane_id == "pane_1" && params.viewport_row == 1 && params.col == 2
    ));

    let up = MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        ..down
    };
    let held = state.handle_raw_events(vec![RawInputEvent::Mouse(up)]);
    assert!(held.requests.is_empty() && held.actions.is_empty());
    let (_, actions) = state.handle_endpoint_result(
        "boot-1",
        &request_id,
        Ok(crate::api::schema::ResponseResult::PaneLinkActivated {
            url: Some("https://example.test".to_owned()),
            handled: false,
        }),
    );
    assert!(matches!(
        &actions[..],
        [ClientShellAction::OpenSafeWebUrl(url)] if url == "https://example.test"
    ));
    assert!(!state.url_click_consumes_until_up);
}

#[test]
fn ctrl_click_without_a_link_replays_the_original_gesture() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("pane frame");
    let pane = state.hits.panes[0].clone();
    let down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x + 2,
        row: pane.inner_rect.y + 1,
        modifiers: KeyModifiers::CONTROL,
    };
    let activate = state.handle_raw_events(vec![RawInputEvent::Mouse(down)]);
    let request_id = match &activate.actions[..] {
        [ClientShellAction::Endpoint { request, .. }] => request.id.clone(),
        _ => panic!("expected link activation request"),
    };
    let (_, actions) = state.handle_endpoint_result(
        "boot-1",
        &request_id,
        Ok(crate::api::schema::ResponseResult::PaneLinkActivated {
            url: None,
            handled: false,
        }),
    );
    assert!(matches!(
        &actions[..],
        [ClientShellAction::ReplayMouse(events)] if events == &vec![down]
    ));
    let replay = match actions.into_iter().next().expect("replay action") {
        ClientShellAction::ReplayMouse(events) => state.replay_mouse_events(events),
        _ => unreachable!(),
    };
    assert!(matches!(
        &replay.actions[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(request.method, crate::api::schema::Method::PaneFocus(_))
    ));
    assert!(state.selection.is_some());
}

#[test]
fn pane_split_drag_uses_projected_handle_and_stable_tab_path() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.splits.push(PaneSurfaceSplit {
        direction: PaneSurfaceSplitDirection::Horizontal,
        pos: 40,
        area: SurfaceRect {
            x: 0,
            y: 0,
            width: 80,
            height: 19,
        },
        hit_rect: SurfaceRect {
            x: 40,
            y: 0,
            width: 1,
            height: 19,
        },
        path: vec![false, true],
    });
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("split pane surface");
    let split = state.hits.pane_splits[0].clone();

    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: split.hit_rect.x,
        row: split.hit_rect.y + 2,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        state.chrome_drag,
        Some(ClientChromeDrag::PaneSplit { .. })
    ));
    let mut replacement = snapshot();
    replacement.revision = 2;
    replacement
        .tab_bar_right
        .push(crate::protocol::ClientShellTabStatusSegment {
            text: "updated".into(),
            accent: false,
        });
    let mut replacement_surface = surface();
    replacement_surface.projection_revision = 2;
    replacement_surface.splits.push(PaneSurfaceSplit {
        direction: PaneSurfaceSplitDirection::Horizontal,
        pos: 40,
        area: SurfaceRect {
            x: 0,
            y: 0,
            width: 80,
            height: 19,
        },
        hit_rect: SurfaceRect {
            x: 40,
            y: 0,
            width: 1,
            height: 19,
        },
        path: vec![false, true],
    });
    state.set_snapshot(Box::new(replacement));
    state.set_pane_surface(replacement_surface);
    let drag = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: split.area.x + 48,
        row: split.hit_rect.y + 2,
        modifiers: KeyModifiers::empty(),
    })]);
    let [ClientShellAction::Endpoint { request, .. }] = &drag.actions[..] else {
        panic!("pane split drag should use endpoint API");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::LayoutSetSplitRatio(params)
            if params.tab_id.as_deref() == Some("tab_1")
                && params.path == vec![false, true]
                && (params.ratio - 0.6).abs() < f32::EPSILON
    ));
    let release =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: split.area.x + 48,
            row: split.hit_rect.y + 2,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(release.actions.is_empty());
    assert!(state.chrome_drag.is_none());
}

#[test]
fn disabled_mouse_chrome_keeps_tab_wheel_but_removes_split_drag_hits() {
    let mut config = Config::default();
    config.ui.mouse_capture = false;
    let mut projected = snapshot();
    let mut second_tab = projected.tabs[0].clone();
    second_tab.tab_id = "tab_2".into();
    second_tab.number = 2;
    second_tab.label = "2".into();
    second_tab.focused = false;
    projected.tabs.push(second_tab);
    let mut pane_surface = surface();
    pane_surface.splits.push(PaneSurfaceSplit {
        direction: PaneSurfaceSplitDirection::Horizontal,
        pos: 40,
        area: SurfaceRect {
            x: 0,
            y: 0,
            width: 80,
            height: 19,
        },
        hit_rect: SurfaceRect {
            x: 40,
            y: 0,
            width: 1,
            height: 19,
        },
        path: Vec::new(),
    });
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("mouse-disabled shell");
    assert!(state.hits.pane_splits.is_empty());
    let first_tab = state.hits.tabs[0].0;
    let wheel = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: first_tab.x,
        row: first_tab.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        &wheel.actions[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(
                &request.method,
                crate::api::schema::Method::TabFocus(target) if target.tab_id == "tab_2"
            )
    ));
}

#[test]
fn client_double_click_selects_word_and_copies_only_after_release() {
    for (copy_on_select, release_before_response) in
        [(false, true), (false, false), (true, true), (true, false)]
    {
        let mut state = word_drag_state(copy_on_select);
        let initial = start_word_drag(&mut state);
        let release = MouseEventKind::Up(MouseButton::Left);
        if release_before_response {
            assert!(word_drag_mouse(&mut state, release, 0, 8)
                .actions
                .is_empty());
        }
        let mut actions = word_row_reply(&mut state, &initial, "alpha bravo charlie");
        if !release_before_response {
            assert!(actions.is_empty(), "holding the second press must not copy");
            assert!(state.selection.as_ref().unwrap().is_in_progress());
            state.tick_copy_feedback(std::time::Instant::now() + std::time::Duration::from_secs(1));
            assert!(state.selection.as_ref().unwrap().is_visible());
            assert!(state.copy_feedback.is_none());
            actions = word_drag_mouse(&mut state, release, 0, 8).actions;
        }
        assert!(state.selection.as_ref().unwrap().is_finalized());
        assert_eq!(
            state.selection.as_ref().unwrap().ordered_cells(),
            ((0, 6), (0, 10))
        );
        assert!(
            word_drag_mouse(&mut state, release, 0, 8)
                .actions
                .is_empty(),
            "copy only once"
        );
        if copy_on_select {
            // The token comes from the row read that drew the highlight. A second
            // read could be refused by unrelated output, or return other text.
            assert!(
                matches!(&actions[..], [ClientShellAction::ClipboardWrite(bytes)] if bytes == b"bravo"),
                "plain double-click copies its highlighted token directly: {actions:?}"
            );
            assert!(state.copy_feedback.is_some(), "copy feedback is shown");
            assert!(state.tick_copy_feedback(state.selection_highlight_clear_deadline.unwrap()));
            assert!(state.selection.is_none());
        } else {
            assert!(actions.is_empty(), "manual selection must not auto-copy");
            assert!(state.selection_highlight_clear_deadline.is_none());
            state.tick_copy_feedback(std::time::Instant::now() + std::time::Duration::from_secs(1));
            assert!(
                state.selection.is_some(),
                "manual selection must not expire"
            );
        }
    }
}

fn word_drag_state(copy_on_select: bool) -> ClientShellState {
    let mut config = Config::default();
    config.ui.copy_on_select = copy_on_select;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    let buffer = Buffer::with_lines([
        "alpha bravo charlie",
        "delta echo foxtrot ",
        "golf hotel india   ",
    ]);
    pane_surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
    pane_surface.panes[0].rect.width = 19;
    pane_surface.panes[0].rect.height = 3;
    pane_surface.panes[0].inner_rect = pane_surface.panes[0].rect;
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    state
}

fn word_drag_mouse(
    state: &mut ClientShellState,
    kind: MouseEventKind,
    row: u16,
    col: u16,
) -> ClientShellInput {
    let pane = state.hits.panes[0].clone();
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind,
        column: pane.inner_rect.x + col,
        row: pane.inner_rect.y + row,
        modifiers: KeyModifiers::empty(),
    })])
}

fn word_read_id(actions: &[ClientShellAction]) -> String {
    actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::Endpoint { request, .. }
                if matches!(
                    request.method,
                    crate::api::schema::Method::PaneSelectionRead(_)
                ) =>
            {
                Some(request.id.clone())
            }
            _ => None,
        })
        .expect("selection read")
}

fn word_row_reply(state: &mut ClientShellState, id: &str, text: &str) -> Vec<ClientShellAction> {
    state
        .handle_endpoint_result(
            "boot-1",
            id,
            Ok(crate::api::schema::ResponseResult::PaneSelection {
                pane_id: "pane_1".into(),
                text: text.into(),
            }),
        )
        .1
}

fn start_word_drag(state: &mut ClientShellState) -> String {
    word_drag_mouse(state, MouseEventKind::Down(MouseButton::Left), 0, 8);
    word_drag_mouse(state, MouseEventKind::Up(MouseButton::Left), 0, 8);
    assert!(state.selection.is_none(), "plain clicks must not select");
    let second = word_drag_mouse(state, MouseEventKind::Down(MouseButton::Left), 0, 8);
    assert!(second.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(&request.method, crate::api::schema::Method::PaneSelectionRead(params)
            if params.anchor.col == 0 && params.cursor.col == state.hits.panes[0].inner_rect.width - 1))));
    word_read_id(&second.actions)
}

#[test]
fn double_click_drag_selects_whole_words_in_both_directions() {
    let mut state = word_drag_state(false);
    let initial = start_word_drag(&mut state);
    word_row_reply(&mut state, &initial, "alpha bravo charlie");
    for (col, expected) in [
        (14, ((0, 6), (0, 18))),
        (2, ((0, 0), (0, 10))),
        (8, ((0, 6), (0, 10))),
        (11, ((0, 6), (0, 11))),
        (16, ((0, 6), (0, 18))),
    ] {
        let motion = word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, col);
        assert!(
            motion.actions.is_empty(),
            "reuse the row while dragging within it"
        );
        assert_eq!(state.selection.as_ref().unwrap().ordered_cells(), expected);
    }
    assert!(
        word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 16)
            .actions
            .is_empty()
    );
    assert!(state.selection.as_ref().unwrap().is_finalized());
}

#[test]
fn double_click_drag_waits_for_latest_row_before_copying() {
    for release_before_anchor in [false, true] {
        let mut state = word_drag_state(true);
        let initial = start_word_drag(&mut state);
        if !release_before_anchor {
            assert!(word_row_reply(&mut state, &initial, "alpha bravo charlie").is_empty());
        }
        let first_motion =
            word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 1, 8);
        for col in [1, 3, 7] {
            assert!(
                word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 2, col)
                    .actions
                    .is_empty()
            );
        }
        assert!(
            word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 2, 7)
                .actions
                .is_empty()
        );
        let final_read = if release_before_anchor {
            word_row_reply(&mut state, &initial, "alpha bravo charlie")
        } else {
            word_row_reply(
                &mut state,
                &word_read_id(&first_motion.actions),
                "delta echo foxtrot",
            )
        };
        assert!(
            matches!(&final_read[..], [ClientShellAction::Endpoint { request, .. }]
            if matches!(&request.method, crate::api::schema::Method::PaneSelectionRead(params)
                if params.anchor.row == 2 && params.cursor.row == 2))
        );
        let copy = word_row_reply(&mut state, &word_read_id(&final_read), "golf hotel india");
        assert!(
            matches!(&copy[..], [ClientShellAction::Endpoint { request, .. }]
            if matches!(&request.method, crate::api::schema::Method::PaneSelectionRead(params)
                if params.anchor == crate::api::schema::PaneTextPoint { row: 0, col: 6 }
                    && params.cursor == crate::api::schema::PaneTextPoint { row: 2, col: 9 }))
        );
        let copied = word_row_reply(
            &mut state,
            &word_read_id(&copy),
            "bravo charlie\ndelta echo foxtrot\ngolf hotel",
        );
        assert!(
            matches!(&copied[..], [ClientShellAction::ClipboardWrite(bytes)]
            if bytes == b"bravo charlie\ndelta echo foxtrot\ngolf hotel")
        );
    }
}

#[test]
fn double_click_drag_ignores_row_reply_after_typing_or_new_click() {
    for typing in [false, true] {
        let mut state = word_drag_state(false);
        let initial = start_word_drag(&mut state);
        word_row_reply(&mut state, &initial, "alpha bravo charlie");
        let drag = word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 1, 8);
        let row_id = word_read_id(&drag.actions);
        if typing {
            state.handle_input_bytes(b"x");
        } else {
            word_drag_mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 0, 0);
            word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 0);
        }
        assert!(word_row_reply(&mut state, &row_id, "delta echo foxtrot").is_empty());
        assert!(state.selection.is_none());
    }
}

#[test]
fn double_click_drag_survives_focus_lag_after_anchor_reply() {
    let mut state = word_drag_state(true);
    let initial = start_word_drag(&mut state);
    word_row_reply(&mut state, &initial, "alpha bravo charlie");
    let mut lagging = snapshot();
    lagging.focused_pane_id = None;
    lagging.panes[0].focused = false;
    state.set_snapshot(Box::new(lagging));
    assert!(state.selection.is_some());
    word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, 14);
    assert_eq!(
        state.selection.as_ref().unwrap().ordered_cells(),
        ((0, 6), (0, 18))
    );
    let released = word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 14);
    assert_eq!(released.actions.len(), 1);
}

#[test]
fn double_click_drag_invalidates_cached_boundaries_outside_selected_cells() {
    for copy_on_select in [false, true] {
        let mut state = word_drag_state(copy_on_select);
        let initial = start_word_drag(&mut state);
        word_row_reply(&mut state, &initial, "alpha bravo charlie");
        let mut changed = state.pane_surface.as_ref().unwrap().clone();
        changed.surface_revision += 1;
        changed.panes[0].content_revision += 2;
        changed.frame.cells[14].symbol = " ".into();
        state.set_pane_surface(changed);
        assert!(
            state.selection.is_none(),
            "unchanged selected cells do not validate cached boundaries outside the selection"
        );
        assert!(
            word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, 14)
                .actions
                .is_empty()
        );
        assert!(
            word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 14)
                .actions
                .is_empty()
        );
        assert!(state.selection.is_none());
    }
}

#[test]
fn reconnect_word_selection_tracks_content_changes() {
    for content_changed in [false, true] {
        let mut state = word_drag_state(true);
        let initial = start_word_drag(&mut state);
        word_row_reply(&mut state, &initial, "alpha bravo charlie");
        let mut next_surface = state.pane_surface.as_ref().unwrap().clone();
        if content_changed {
            next_surface.panes[0].content_revision += 2;
            next_surface.frame.cells[14].symbol = " ".into();
        }
        let endpoint_id = state.active_endpoint_id.clone();
        let snapshot = state.snapshot.as_ref().unwrap().clone();
        state.mark_endpoint_disconnected(&endpoint_id);
        state.cache_endpoint_snapshot_inactive_for_generation(&endpoint_id, 1, snapshot);
        state.set_endpoint_status(
            &endpoint_id,
            crate::client::endpoint::ClientEndpointStatus::Online,
        );
        assert!(state.activate_endpoint_projection(&endpoint_id));
        state.set_pane_surface(next_surface);

        assert_eq!(state.selection.is_some(), !content_changed);
        assert_eq!(state.word_selection_gesture.is_some(), !content_changed);
    }
}

#[test]
fn double_click_release_ignores_reply_after_focus_or_content_changes() {
    for focus_changed in [false, true] {
        let mut state = word_drag_state(true);
        let initial = start_word_drag(&mut state);
        word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 8);
        if focus_changed {
            let mut lagging = snapshot();
            lagging.focused_pane_id = None;
            lagging.panes[0].focused = false;
            state.set_snapshot(Box::new(lagging));
            let mut unfocused = snapshot();
            unfocused.focused_pane_id = Some("pane_2".into());
            unfocused.panes[0].focused = false;
            let mut other = unfocused.panes[0].clone();
            other.pane_id = "pane_2".into();
            other.focused = true;
            unfocused.panes.push(other);
            state.set_snapshot(Box::new(unfocused));
        } else {
            let mut changed = state.pane_surface.as_ref().unwrap().clone();
            changed.surface_revision += 1;
            changed.panes[0].content_revision += 2;
            state.set_pane_surface(changed);
        }
        assert!(
            word_row_reply(&mut state, &initial, "alpha bravo charlie").is_empty(),
            "a stale released gesture must not copy"
        );
        assert!(state.selection.is_none());
    }
}

#[test]
fn double_click_drag_resize_cancels_pending_word_lookup() {
    for anchor_ready in [false, true] {
        let mut state = word_drag_state(true);
        let initial = start_word_drag(&mut state);
        let pending = if anchor_ready {
            word_row_reply(&mut state, &initial, "alpha bravo charlie");
            let motion = word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 1, 8);
            word_read_id(&motion.actions)
        } else {
            initial
        };
        word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 1, 8);
        let mut resized = state.pane_surface.as_ref().unwrap().clone();
        resized.surface_revision += 1;
        resized.panes[0].rect.width += 5;
        resized.panes[0].inner_rect.width += 5;
        state.set_pane_surface(resized);
        assert!(word_row_reply(&mut state, &pending, "alpha bravo charlie extra").is_empty());
        assert!(
            state.selection.is_none(),
            "a late reply must not restore a resized selection"
        );
        assert!(state.selection_autoscroll.is_none());
    }
}

#[test]
fn double_click_drag_autoscroll_keeps_absolute_word_anchor() {
    let mut state = word_drag_state(false);
    state.hits.panes[0].scroll = Some(crate::pane::ScrollMetrics {
        max_offset_from_bottom: 10,
        offset_from_bottom: 5,
        viewport_rows: 3,
    });
    let initial = start_word_drag(&mut state);
    word_row_reply(&mut state, &initial, "alpha bravo charlie");
    word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, 14);
    let tick = state.tick_selection_autoscroll(state.selection_autoscroll_deadline.unwrap());
    word_row_reply(
        &mut state,
        &word_read_id(&tick.actions),
        "delta echo foxtrot",
    );
    assert_eq!(
        state.selection.as_ref().unwrap().ordered_cells(),
        ((4, 11), (5, 10))
    );
    word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 14);
    assert!(state.selection.as_ref().unwrap().is_finalized());
    assert!(state.selection_autoscroll.is_none());
}

const WORD_ROWS: [&str; 3] = [
    "alpha BRAVOCOPY charlie",
    "delta echo foxtrot",
    "golf hotel india",
];

/// Fake endpoint answering `pane.selection.read` the way the real server does
/// (`src/app/api/panes.rs`, `selection_read`): a read pinned to a content
/// revision that no longer matches is refused with `stale_content`; any other
/// read returns the text the pane holds NOW in the requested columns.
struct WordEndpoint {
    rows: Vec<String>,
    revision: u64,
    reads: Vec<crate::api::schema::PaneSelectionReadParams>,
}

impl WordEndpoint {
    fn new(rows: &[&str]) -> Self {
        Self {
            rows: rows.iter().map(|row| (*row).to_owned()).collect(),
            revision: 0,
            reads: Vec::new(),
        }
    }

    /// Pane output reaching the server: the revision moves, and optionally one row changes.
    fn output(&mut self, row: usize, text: &str) {
        self.revision += 1;
        self.rows[row] = text.to_owned();
    }

    fn read(
        &self,
        params: &crate::api::schema::PaneSelectionReadParams,
    ) -> Result<crate::api::schema::ResponseResult, ClientShellEndpointError> {
        if params
            .content_revision
            .is_some_and(|revision| revision != self.revision)
        {
            return Err(ClientShellEndpointError {
                code: Some("stale_content".into()),
                message: "pane content changed".into(),
            });
        }
        assert_eq!(
            params.anchor.row, params.cursor.row,
            "fixture serves single-row reads"
        );
        // Display columns, by the unicode-width tables rather than the code under test.
        let mut col = 0u16;
        let mut text = String::new();
        for ch in self.rows[params.anchor.row as usize].chars() {
            let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
            let start = if width == 0 {
                col.saturating_sub(1)
            } else {
                col
            };
            col += width;
            if start >= params.anchor.col && start <= params.cursor.col {
                text.push(ch);
            }
        }
        Ok(crate::api::schema::ResponseResult::PaneSelection {
            pane_id: params.pane_id.clone(),
            text,
        })
    }

    /// Answer the selection reads in `actions` once; return what they lead to.
    fn answer(
        &mut self,
        state: &mut ClientShellState,
        actions: Vec<ClientShellAction>,
    ) -> (Vec<ClientShellAction>, Vec<Vec<u8>>) {
        let mut next = Vec::new();
        let mut copied = Vec::new();
        for action in actions {
            match action {
                ClientShellAction::Endpoint { request, .. } => {
                    if let crate::api::schema::Method::PaneSelectionRead(params) = &request.method {
                        self.reads.push(params.clone());
                        let result = self.read(params);
                        next.extend(
                            state
                                .handle_endpoint_result("boot-1", &request.id, result)
                                .1,
                        );
                    }
                }
                ClientShellAction::ClipboardWrite(bytes) => copied.push(bytes),
                _ => {}
            }
        }
        (next, copied)
    }

    /// Answer every selection read until none is left; return all clipboard payloads.
    fn serve(
        &mut self,
        state: &mut ClientShellState,
        mut actions: Vec<ClientShellAction>,
    ) -> Vec<Vec<u8>> {
        let mut copied = Vec::new();
        while !actions.is_empty() {
            let (next, written) = self.answer(state, actions);
            copied.extend(written);
            actions = next;
        }
        copied
    }
}

/// Every surface in these tests is 40 columns wide, so a content change is
/// never also a resize (a resize clears selections through an older guard).
fn word_surface(rows: &[&str], surface_revision: u64, content_revision: u64) -> PaneSurfaceFrame {
    let mut pane_surface = surface();
    let buffer = Buffer::with_lines(rows.iter().map(|row| {
        format!(
            "{row}{}",
            " ".repeat(40 - unicode_width::UnicodeWidthStr::width(*row))
        )
    }));
    pane_surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
    pane_surface.surface_revision = surface_revision;
    let pane = &mut pane_surface.panes[0];
    pane.content_revision = content_revision;
    pane.rect.width = buffer.area.width;
    pane.rect.height = buffer.area.height;
    pane.inner_rect = pane.rect;
    pane_surface
}

fn word_state(copy_on_select: bool, rows: &[&str]) -> ClientShellState {
    let mut config = Config::default();
    config.ui.copy_on_select = copy_on_select;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(word_surface(rows, 1, 0));
    state.compose(106, 20).expect("composed frame");
    state
}

/// Click, release, press again: returns the actions of the held second press.
fn press_twice(state: &mut ClientShellState, row: u16, col: u16) -> Vec<ClientShellAction> {
    word_drag_mouse(state, MouseEventKind::Down(MouseButton::Left), row, col);
    word_drag_mouse(state, MouseEventKind::Up(MouseButton::Left), row, col);
    let second = word_drag_mouse(state, MouseEventKind::Down(MouseButton::Left), row, col);
    assert!(
        second
            .actions
            .iter()
            .any(|action| matches!(action, ClientShellAction::Endpoint { .. })),
        "the second press reads the clicked row"
    );
    second.actions
}

/// A plain double-click on BRAVOCOPY with nothing in between; returns its state.
fn copied_bravocopy() -> ClientShellState {
    let mut state = word_state(true, &WORD_ROWS);
    let mut endpoint = WordEndpoint::new(&WORD_ROWS);
    let held = press_twice(&mut state, 0, 8);
    assert!(endpoint.serve(&mut state, held).is_empty());
    let release = word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 8);
    assert_eq!(
        endpoint.serve(&mut state, release.actions),
        vec![b"BRAVOCOPY".to_vec()]
    );
    assert_eq!(
        state.selection.as_ref().unwrap().ordered_cells(),
        ((0, 6), (0, 14))
    );
    assert!(state.selection_highlight_clear_deadline.is_some());
    state
}

/// The owner's report: the word is highlighted but the clipboard never changes.
/// Output reaches the server after it answered the row read that drew the
/// highlight. Unrelated output, or the clicked row itself being rewritten, must
/// not stop the copy, and the copy must be the token that was highlighted.
#[test]
fn plain_double_click_copies_highlighted_token_despite_later_output() {
    for (output_row, output_text) in [(2, "OUTPUT LINE"), (0, "alpha WRONGTEXT charlie")] {
        for release_before_response in [false, true] {
            let case =
                format!("output on row {output_row}, release first {release_before_response}");
            let mut state = word_state(true, &WORD_ROWS);
            let mut endpoint = WordEndpoint::new(&WORD_ROWS);
            let before = std::time::Instant::now();
            let held = press_twice(&mut state, 0, 8);
            let mut pending = Vec::new();
            if release_before_response {
                pending = word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 8)
                    .actions;
                assert!(
                    pending.is_empty(),
                    "{case}: nothing to copy before the row arrives"
                );
            }
            // The server answers the row read against the unchanged pane...
            let (after_row, copied_with_row) = endpoint.answer(&mut state, held);
            pending.extend(after_row);
            // ...then output reaches it before the client hears of it.
            endpoint.output(output_row, output_text);
            if !release_before_response {
                assert!(
                    pending.is_empty() && copied_with_row.is_empty(),
                    "{case}: held press"
                );
                pending = word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 8)
                    .actions;
            }
            let mut copied = copied_with_row;
            copied.extend(endpoint.serve(&mut state, pending));
            assert_eq!(
                copied
                    .iter()
                    .map(|bytes| String::from_utf8_lossy(bytes))
                    .collect::<Vec<_>>(),
                vec!["BRAVOCOPY"],
                "{case}: copies the highlighted token exactly once"
            );
            assert_eq!(
                endpoint.reads.len(),
                1,
                "{case}: no second read after the highlight: {:?}",
                endpoint.reads
            );
            assert_eq!(
                state.selection.as_ref().unwrap().ordered_cells(),
                ((0, 6), (0, 14)),
                "{case}"
            );
            assert!(state.selection.as_ref().unwrap().is_finalized(), "{case}");
            assert!(state.copy_feedback.is_some(), "{case}: copy feedback");
            let deadline = state.selection_highlight_clear_deadline.unwrap();
            let half_second = std::time::Duration::from_millis(500);
            assert!(
                deadline >= before + half_second
                    && deadline <= std::time::Instant::now() + half_second,
                "{case}: the highlight stays for 500 ms"
            );
        }
    }
}

fn patch_to(
    state: &ClientShellState,
    next: &PaneSurfaceFrame,
) -> crate::protocol::PaneSurfacePatch {
    let current = state.pane_surface.as_ref().unwrap();
    let width = usize::from(next.frame.width);
    crate::protocol::PaneSurfacePatch {
        boot_id: current.boot_id.clone(),
        projection_revision: current.projection_revision,
        base_surface_revision: current.surface_revision,
        surface_revision: current.surface_revision + 1,
        rows: next
            .frame
            .cells
            .chunks(width)
            .enumerate()
            .filter(|(y, cells)| current.frame.cells[y * width..(y + 1) * width] != **cells)
            .map(|(y, cells)| crate::protocol::PaneSurfacePatchRow {
                x: 0,
                y: y as u16,
                cells: cells.to_vec(),
            })
            .collect(),
        panes: next.panes.clone(),
        cursor: next.frame.cursor.clone(),
    }
}

/// After a snapshot copy the highlight must not mark text that was not copied,
/// and must not vanish early because of output elsewhere: content_revision is
/// pane-wide, so a busy agent pane would wipe a pane-wide guard at once.
#[test]
fn copied_word_highlight_clears_only_when_its_own_cells_change() {
    type Change = Box<dyn Fn(&mut PaneSurfaceFrame)>;
    fn with_rows(rows: [&'static str; 3]) -> Change {
        Box::new(move |next: &mut PaneSurfaceFrame| *next = word_surface(&rows, 2, 1))
    }
    // (case, change to the next surface, highlight kept, also deliverable as a patch)
    let cases: Vec<(&str, Change, bool, bool)> = vec![
        (
            "output on another row",
            with_rows([WORD_ROWS[0], WORD_ROWS[1], "OUTPUT LINE HERE"]),
            true,
            true,
        ),
        (
            "output beside the token",
            with_rows(["ALPHA BRAVOCOPY CHARLIE", WORD_ROWS[1], WORD_ROWS[2]]),
            true,
            true,
        ),
        (
            "token row scrolled out of view",
            Box::new(|next: &mut PaneSurfaceFrame| {
                *next = word_surface(&["new", "rows", "here"], 2, 1);
                next.panes[0].scroll = Some(crate::protocol::PaneSurfaceScrollMetrics {
                    offset_from_bottom: 0,
                    max_offset_from_bottom: 5,
                    viewport_rows: 3,
                });
            }),
            true,
            true,
        ),
        (
            "pane not in the surface",
            Box::new(|next: &mut PaneSurfaceFrame| {
                *next = word_surface(&["new", "rows", "here"], 2, 1);
                next.panes[0].pane_id = "pane_9".into();
            }),
            true,
            false,
        ),
        (
            // Malformed, but nothing validates a full surface's frame size: cells it
            // does not carry are not evidence that the token changed.
            "frame carries no cells",
            Box::new(|next: &mut PaneSurfaceFrame| {
                next.frame.cells.clear();
                next.frame.height = 0;
            }),
            true,
            false,
        ),
        (
            "token rewritten",
            with_rows(["alpha WRONGTEXT charlie", WORD_ROWS[1], WORD_ROWS[2]]),
            false,
            true,
        ),
        (
            "one token cell rewritten",
            with_rows(["alpha BRAVOCOPX charlie", WORD_ROWS[1], WORD_ROWS[2]]),
            false,
            true,
        ),
        (
            "token row cleared",
            with_rows(["", WORD_ROWS[1], WORD_ROWS[2]]),
            false,
            true,
        ),
        (
            // A continuation cell (empty symbol) where a token cell was: must
            // compare, not stall on a zero-width symbol.
            "a token cell with an empty symbol",
            Box::new(|next: &mut PaneSurfaceFrame| {
                *next = word_surface(&WORD_ROWS, 2, 1);
                next.frame.cells[10].symbol = String::new();
            }),
            false,
            true,
        ),
    ];
    for via_patch in [false, true] {
        for (name, change, kept, patchable) in &cases {
            if via_patch && !patchable {
                continue;
            }
            let case = format!("{name}, via patch {via_patch}");
            let mut state = copied_bravocopy();
            let deadline = state.selection_highlight_clear_deadline.unwrap();
            let mut next = word_surface(&WORD_ROWS, 2, 1);
            change(&mut next);
            if via_patch {
                let patch = patch_to(&state, &next);
                assert!(
                    matches!(
                        state.apply_pane_surface_patch(patch),
                        super::super::surface_patch::ClientPaneSurfacePatchOutcome::Applied(_)
                    ),
                    "{case}: patch applies"
                );
            } else {
                state.set_pane_surface(next);
            }
            assert_eq!(
                state.pane_surface.as_ref().unwrap().panes[0].content_revision,
                1,
                "{case}: the update was taken"
            );
            assert_eq!(state.selection.is_some(), *kept, "{case}: highlight");
            if *kept {
                assert_eq!(
                    state.selection_highlight_clear_deadline,
                    Some(deadline),
                    "{case}: left to its deadline"
                );
                state.tick_copy_feedback(deadline - std::time::Duration::from_millis(1));
                assert!(state.selection.is_some(), "{case}: kept until the deadline");
                state.tick_copy_feedback(deadline);
                assert!(state.selection.is_none(), "{case}: cleared at the deadline");
            } else {
                assert!(state.selection_highlight_clear_deadline.is_none(), "{case}");
            }
        }
    }
}

/// The guard reads the token's own cells inside its pane: a pane offset in the
/// surface and scrolled into scrollback, with other content around and below it.
#[test]
fn copied_word_guard_reads_only_its_own_pane_cells() {
    // 44x10 surface: a border row, the pane's three rows framed by border cells,
    // then six rows that belong to something else.
    let framed = |border: &str,
                  rows: [&str; 3],
                  below: &str,
                  offset_from_bottom: u64,
                  revision: u64| {
        let mut lines = vec![border.repeat(44)];
        lines.extend(
            rows.iter()
                .map(|row| format!("{border}{border}{row:<40}{border}{border}")),
        );
        lines.extend((0..6).map(|_| format!("{below:<44}")));
        let mut next = surface();
        next.frame =
            FrameData::from_ratatui_buffer_with_hyperlinks(&Buffer::with_lines(lines), None, &[]);
        next.surface_revision = revision + 1;
        let pane = &mut next.panes[0];
        pane.content_revision = revision;
        pane.rect = SurfaceRect {
            x: 0,
            y: 0,
            width: 44,
            height: 10,
        };
        pane.inner_rect = SurfaceRect {
            x: 2,
            y: 1,
            width: 40,
            height: 3,
        };
        pane.scroll = Some(crate::protocol::PaneSurfaceScrollMetrics {
            offset_from_bottom,
            max_offset_from_bottom: 10,
            viewport_rows: 3,
        });
        next
    };
    for (case, border, shown, below, offset, kept) in [
        (
            "border cells on the token's screen row change",
            "%",
            [WORD_ROWS[0], "", ""],
            "below",
            0,
            true,
        ),
        (
            "token scrolled below the pane, over other rows",
            "#",
            ["older", "rows", "here"],
            "ZZZZ",
            5,
            true,
        ),
        (
            "token rewritten",
            "#",
            ["alpha WRONGTEXT charlie", "", ""],
            "below",
            0,
            false,
        ),
    ] {
        let mut config = Config::default();
        config.ui.copy_on_select = true;
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
        state.set_snapshot(Box::new(snapshot()));
        state.set_pane_surface(framed("#", [WORD_ROWS[0], "", ""], "below", 0, 0));
        state.compose(106, 20).expect("composed frame");
        // Absolute row 10 is the top of the viewport.
        let mut server_rows = vec![""; 10];
        server_rows.push(WORD_ROWS[0]);
        let mut endpoint = WordEndpoint::new(&server_rows);
        let held = press_twice(&mut state, 0, 8);
        assert!(endpoint.serve(&mut state, held).is_empty());
        let release = word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 8);
        assert_eq!(
            endpoint.serve(&mut state, release.actions),
            vec![b"BRAVOCOPY".to_vec()],
            "{case}"
        );
        assert_eq!(
            state.selection.as_ref().unwrap().ordered_cells(),
            ((10, 6), (10, 14)),
            "{case}"
        );
        state.set_pane_surface(framed(border, shown, below, offset, 1));
        assert_eq!(state.selection.is_some(), kept, "{case}");
    }
}

/// Once the copied highlight is gone, a newer selection is the user's own and
/// the old token's guard must leave it alone.
#[test]
fn copied_word_guard_leaves_a_later_selection_alone() {
    let mut state = copied_bravocopy();
    word_drag_mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 0);
    word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 1, 4);
    assert_eq!(
        state.selection.as_ref().unwrap().ordered_cells(),
        ((1, 0), (1, 4))
    );
    state.set_pane_surface(word_surface(
        &["alpha WRONGTEXT charlie", WORD_ROWS[1], WORD_ROWS[2]],
        2,
        1,
    ));
    assert_eq!(
        state
            .selection
            .as_ref()
            .map(|selection| selection.ordered_cells()),
        Some(((1, 0), (1, 4))),
        "a drag selection survives output on the earlier token"
    );
}

/// A double-click that is dragged into more words copies the dragged range
/// with a live read, like a plain drag (upstream 963f78f4, #4193). Output must
/// not refuse it. Pinned: a rewritten row copies the NEW text, as plain drag does.
#[test]
fn extended_word_drag_copies_live_range_after_output() {
    for (output_row, output_text, expected) in [
        (2, "OUTPUT LINE", "BRAVOCOPY charlie"),
        (0, "alpha WRONGTEXT charlie", "WRONGTEXT charlie"),
    ] {
        let mut state = word_state(true, &WORD_ROWS);
        let mut endpoint = WordEndpoint::new(&WORD_ROWS);
        let held = press_twice(&mut state, 0, 8);
        assert!(endpoint.serve(&mut state, held).is_empty());
        let drag = word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, 18);
        assert!(drag.actions.is_empty());
        assert_eq!(
            state.selection.as_ref().unwrap().ordered_cells(),
            ((0, 6), (0, 22))
        );
        endpoint.output(output_row, output_text);
        let release = word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 18);
        let copied = endpoint.serve(&mut state, release.actions);
        let last = endpoint.reads.last().unwrap();
        assert_eq!(last.content_revision, None, "live read, like a plain drag");
        assert_eq!(
            (last.anchor, last.cursor),
            (
                crate::api::schema::PaneTextPoint { row: 0, col: 6 },
                crate::api::schema::PaneTextPoint { row: 0, col: 22 }
            )
        );
        assert_eq!(
            copied
                .iter()
                .map(|bytes| String::from_utf8_lossy(bytes))
                .collect::<Vec<_>>(),
            vec![expected]
        );
        assert!(state.selection.is_none());
    }
}

#[test]
fn copy_on_select_live_reload_switches_double_click_copy_both_ways() {
    for (configured, reloaded) in [(true, false), (false, true)] {
        let mut state = word_state(configured, &WORD_ROWS);
        let mut config = Config::default();
        config.ui.copy_on_select = reloaded;
        state.config.apply_live_config(&config, &[], &[]);
        assert_eq!(state.config.copy_on_select, reloaded);
        let mut endpoint = WordEndpoint::new(&WORD_ROWS);
        let held = press_twice(&mut state, 0, 8);
        assert!(endpoint.serve(&mut state, held).is_empty());
        let release = word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 8);
        let copied = endpoint.serve(&mut state, release.actions);
        if reloaded {
            assert_eq!(copied, vec![b"BRAVOCOPY".to_vec()]);
            assert!(state.selection_highlight_clear_deadline.is_some());
        } else {
            assert!(copied.is_empty(), "copy_on_select = false never copies");
            assert_eq!(endpoint.reads.len(), 1, "and reads nothing more");
            assert!(state.selection_highlight_clear_deadline.is_none());
            state.tick_copy_feedback(std::time::Instant::now() + std::time::Duration::from_secs(1));
            assert!(
                state.selection.as_ref().unwrap().is_finalized(),
                "selection stays"
            );
        }
    }
}

/// Exact bytes and display columns for tokens whose bytes, chars and columns differ.
#[test]
fn plain_double_click_copies_unicode_tokens_exactly() {
    for (row, click, token, columns) in [
        (
            "see https://example.test/a?b=1 ok",
            10,
            "https://example.test/a?b=1",
            (4, 29),
        ),
        (
            "cat \"my dir/file a.txt\" ok",
            9,
            "my dir/file a.txt",
            (5, 21),
        ),
        ("x 日本語 y", 5, "日本語", (2, 7)),
        ("a cafe\u{301} b", 4, "cafe\u{301}", (2, 5)),
        ("日本 wide-汉字x end", 9, "wide-汉字x", (5, 14)),
    ] {
        let rows = [row, "second row", "third row"];
        let mut state = word_state(true, &rows);
        let mut endpoint = WordEndpoint::new(&rows);
        let held = press_twice(&mut state, 0, click);
        assert!(endpoint.serve(&mut state, held).is_empty());
        let release = word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, click);
        assert_eq!(
            endpoint.serve(&mut state, release.actions),
            vec![token.as_bytes().to_vec()],
            "{row:?}"
        );
        assert_eq!(
            state.selection.as_ref().unwrap().ordered_cells(),
            ((0, columns.0), (0, columns.1)),
            "{row:?}"
        );
        // The row-scoped guard reads these cells back from the surface: output
        // elsewhere must leave a wide or combining token highlighted...
        state.set_pane_surface(word_surface(&[row, "second row", "OUTPUT ROW"], 2, 1));
        assert!(
            state.selection.is_some(),
            "{row:?}: unchanged token keeps its highlight"
        );
        // ...and its own row changing must clear it.
        state.set_pane_surface(word_surface(&["", "second row", "OUTPUT ROW"], 3, 2));
        assert!(state.selection.is_none(), "{row:?}: rewritten token clears");
    }
}

#[test]
fn double_click_copies_at_most_once_across_duplicate_or_obsolete_replies() {
    // A duplicate of the row reply, and a repeated release, copy nothing more.
    let mut state = word_state(true, &WORD_ROWS);
    let mut endpoint = WordEndpoint::new(&WORD_ROWS);
    let held = press_twice(&mut state, 0, 8);
    let id = word_read_id(&held);
    assert!(
        word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 8)
            .actions
            .is_empty()
    );
    assert_eq!(
        endpoint.serve(&mut state, held),
        vec![b"BRAVOCOPY".to_vec()]
    );
    let duplicate = word_row_reply(&mut state, &id, WORD_ROWS[0]);
    assert!(
        endpoint.serve(&mut state, duplicate).is_empty(),
        "duplicate reply"
    );
    let again = word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 8);
    assert!(
        endpoint.serve(&mut state, again.actions).is_empty(),
        "repeated release"
    );

    // A reply that arrives after a new click belongs to an obsolete gesture.
    let mut state = word_state(true, &WORD_ROWS);
    let held = press_twice(&mut state, 0, 8);
    word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 8);
    word_drag_mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 0);
    word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 1, 0);
    assert!(
        endpoint.serve(&mut state, held).is_empty(),
        "obsolete gesture"
    );

    // A reply for another pane copies nothing.
    let mut state = word_state(true, &WORD_ROWS);
    let held = press_twice(&mut state, 0, 8);
    word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 8);
    let (_, result) = state.handle_endpoint_result(
        "boot-1",
        &word_read_id(&held),
        Ok(crate::api::schema::ResponseResult::PaneSelection {
            pane_id: "pane_2".into(),
            text: WORD_ROWS[0].into(),
        }),
    );
    assert!(endpoint.serve(&mut state, result).is_empty(), "wrong pane");
    assert!(state.selection.is_none());
}

#[test]
fn pane_content_updates_preserve_live_ranges_until_geometry_or_screen_changes() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let surface_at = |surface_revision, content_revision, alternate_screen_active| {
        let mut pane_surface = surface();
        pane_surface.surface_revision = surface_revision;
        pane_surface.panes[0].content_revision = content_revision;
        pane_surface.panes[0].scroll = Some(crate::protocol::PaneSurfaceScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 11,
            viewport_rows: 2,
        });
        pane_surface.panes[0].alternate_screen_active = alternate_screen_active;
        pane_surface
    };
    state.set_pane_surface(surface_at(1, 0, true));
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();
    let mouse = |kind, column, row| {
        RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        })
    };

    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        pane.inner_rect.x,
        pane.inner_rect.y + 1,
    )]);
    let mut updated_surface = surface_at(2, 2, true);
    updated_surface.frame.cells[0].symbol = "W".into();
    state.set_pane_surface(updated_surface);
    state.compose(106, 20).expect("updated frame");

    let drag = state.handle_raw_events(vec![mouse(
        MouseEventKind::Drag(MouseButton::Left),
        pane.inner_rect.x + 1,
        pane.inner_rect.y + 1,
    )]);

    assert!(drag.repaint || state.selection_repaint_deadline.is_some());
    let selection = state.selection.as_ref().expect("visible selection");
    assert!(selection.is_visible());
    assert_eq!(selection.ordered_cells(), ((12, 0), (12, 1)));

    let mut replaced_surface = surface_at(3, 4, true);
    replaced_surface.frame.cells[4].symbol = "X".into();
    state.set_pane_surface(replaced_surface);
    assert_eq!(
        state.selection.as_ref().unwrap().ordered_cells(),
        ((12, 0), (12, 1))
    );

    // The selected row can leave the viewport during a drag. A later patch,
    // including an in-flight content revision, must keep that absolute range.
    let mut scrolled = surface_at(4, 5, true);
    scrolled.panes[0]
        .scroll
        .as_mut()
        .unwrap()
        .offset_from_bottom = 2;
    assert!(matches!(
        state.apply_pane_surface_patch(crate::protocol::PaneSurfacePatch {
            boot_id: scrolled.boot_id,
            projection_revision: scrolled.projection_revision,
            base_surface_revision: 3,
            surface_revision: 4,
            panes: scrolled.panes,
            rows: vec![],
            cursor: scrolled.frame.cursor,
        }),
        super::super::surface_patch::ClientPaneSurfacePatchOutcome::Applied(_)
    ));
    assert!(state.selection.as_ref().unwrap().is_in_progress());
    assert_eq!(
        state.selection.as_ref().unwrap().ordered_cells(),
        ((12, 0), (12, 1))
    );

    for (surface_revision, content_revision, width, alternate_screen_active) in
        [(5, 6, 4, false), (6, 8, 3, false)]
    {
        state.selection = Some(crate::selection::Selection::absolute_anchor(
            "pane_1".to_owned(),
            (12, 0),
        ));
        let mut changed_surface =
            surface_at(surface_revision, content_revision, alternate_screen_active);
        changed_surface.panes[0].inner_rect.width = width;
        changed_surface.panes[0].alternate_screen_active = alternate_screen_active;
        state.set_pane_surface(changed_surface);
        assert!(state.selection.is_none());
    }
}

#[test]
fn pane_mouse_input_keeps_stable_target_and_endpoint_encoding() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();

    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x + 2,
        row: pane.inner_rect.y + 1,
        modifiers: KeyModifiers::ALT,
    })]);
    let [ClientMessage::ClientShellPaneInput { pane_id, events }] = &click.requests[..] else {
        panic!("pane application click should use targeted canonical input");
    };
    assert_eq!(pane_id, "pane_1");
    assert!(matches!(
        &events[..],
        [ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Down(
                crate::protocol::ClientMouseButton::Left
            ),
            position: ClientMousePosition::Cell { column: 2, row: 1 },
            modifiers,
            ..
        }] if *modifiers == KeyModifiers::ALT.bits()
    ));
    let moved = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Moved,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::ALT,
    })]);
    assert!(moved.requests.is_empty());
    assert!(state.pane_mouse_gesture.is_some());
    state.hits.panes.clear();
    let release =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::ALT,
        })]);
    assert!(matches!(
        &release.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, events }]
            if pane_id == "pane_1"
                && matches!(
                    &events[..],
                    [ClientPaneInputEvent::Mouse {
                        kind: crate::protocol::ClientMouseKind::Up(
                            crate::protocol::ClientMouseButton::Left
                        ),
                        ..
                    }]
                )
    ));
    assert!(state.pane_mouse_gesture.is_none());
}

#[test]
fn pane_pixel_mouse_preserves_pane_relative_pixel_coordinates() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    pane_surface.panes[0].sgr_pixel_mouse = true;
    pane_surface.panes[0].pixel_width = 39;
    pane_surface.panes[0].pixel_height = 38;
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();
    let geometry =
        crate::input::mouse::HostGeometry::new(106, 20, 1060, 400).expect("host geometry");
    let x = u32::from(pane.inner_rect.x) * 10 + 21;
    let y = u32::from(pane.inner_rect.y) * 20 + 21;
    let report = format!("\x1b[<0;{x};{y}M");

    let outcome = state.handle_pixel_mouse(report.as_bytes(), geometry);
    assert!(matches!(
        &outcome.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, events }]
            if pane_id == "pane_1"
                && matches!(
                    &events[..],
                    [ClientPaneInputEvent::Mouse {
                        kind: crate::protocol::ClientMouseKind::Down(
                            crate::protocol::ClientMouseButton::Left
                        ),
                        position: ClientMousePosition::Pixels { x: 20, y: 20, .. },
                        ..
                    }]
                )
    ));

    let lost = state.handle_raw_events(vec![RawInputEvent::OuterFocusLost]);
    assert!(matches!(
        &lost.requests[..],
        [
            ClientMessage::ClientShellPaneInput { pane_id, events },
            ClientMessage::ClientShellFocus { focused: false }
        ] if pane_id == "pane_1" && matches!(
            &events[..],
            [ClientPaneInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Up(
                    crate::protocol::ClientMouseButton::Left
                ),
                position: ClientMousePosition::Pixels { x: 20, y: 20, .. },
                ..
            }]
        )
    ));
}

#[test]
fn pane_owned_right_click_forwards_the_complete_gesture() {
    let mut snapshot = snapshot();
    snapshot.panes[0].right_click_passthrough = true;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();

    let down = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: pane.inner_rect.x + 1,
        row: pane.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        &down.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, .. }] if pane_id == "pane_1"
    ));
    assert!(state.overlay.is_none());
    assert!(state.pane_mouse_gesture.is_some());

    let up = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Right),
        column: 0,
        row: 0,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        &up.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, events }]
            if pane_id == "pane_1"
                && matches!(
                    &events[..],
                    [ClientPaneInputEvent::Mouse {
                        kind: crate::protocol::ClientMouseKind::Up(
                            crate::protocol::ClientMouseButton::Right
                        ),
                        ..
                    }]
                )
    ));
    assert!(state.pane_mouse_gesture.is_none());
}

#[test]
fn tab_click_waits_for_release_and_drag_reorders_by_stable_id() {
    let mut projected = snapshot();
    for index in 2..=3 {
        let mut tab = projected.tabs[0].clone();
        tab.tab_id = format!("tab_{index}");
        tab.number = index;
        tab.label = index.to_string();
        tab.focused = false;
        projected.tabs.push(tab);
    }
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("three tabs");
    let first = state.hits.tabs[0].0;
    let third = state.hits.tabs[2].0;

    let down = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: first.x + 1,
        row: first.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(down.actions.is_empty());
    let drag = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: third.right().saturating_sub(1),
        row: third.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(drag.repaint);
    assert!(matches!(
        state.chrome_drag,
        Some(ClientChromeDrag::Tab {
            ref tab_id,
            insert_index: Some(3),
            ..
        }) if tab_id == "tab_1"
    ));
    let frame = state.compose(106, 20).expect("tab drop indicator");
    assert!(frame
        .cells
        .iter()
        .take(frame.width as usize)
        .any(|cell| cell.symbol == "│"));

    let release =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: third.right().saturating_sub(1),
            row: third.y,
            modifiers: KeyModifiers::empty(),
        })]);
    let [ClientShellAction::Endpoint { request, .. }] = &release.actions[..] else {
        panic!("tab drag should use endpoint API");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::TabMove(params)
            if params.tab_id == "tab_1" && params.insert_index == 3
    ));

    state.compose(106, 20).expect("tabs after drag");
    let second = state.hits.tabs[1].0;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: second.x + 1,
        row: second.y,
        modifiers: KeyModifiers::empty(),
    })]);
    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: second.x + 1,
        row: second.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        &click.actions[0],
        ClientShellAction::Endpoint { request, .. }
            if matches!(&request.method, crate::api::schema::Method::TabFocus(target) if target.tab_id == "tab_2")
    ));
}

#[test]
fn tab_drag_clears_its_drop_target_after_leaving_the_tab_row() {
    let mut projected = snapshot();
    for index in 2..=3 {
        let mut tab = projected.tabs[0].clone();
        tab.tab_id = format!("tab_{index}");
        tab.number = index;
        tab.label = index.to_string();
        tab.focused = false;
        projected.tabs.push(tab);
    }
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("three tabs");
    let first = state.hits.tabs[0].0;
    let third = state.hits.tabs[2].0;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: first.x + 1,
        row: first.y,
        modifiers: KeyModifiers::empty(),
    })]);
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: third.x,
        row: third.y,
        modifiers: KeyModifiers::empty(),
    })]);
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: third.x,
        row: third.y + 1,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        state.chrome_drag,
        Some(ClientChromeDrag::Tab {
            insert_index: None,
            ..
        })
    ));
    let release =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: third.x,
            row: third.y + 1,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(release.actions.is_empty());
}

#[test]
fn tab_wheel_switches_tabs_without_changing_overflow_scroll() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("tab bar");
    let tab = state.hits.tabs[0].0;

    let outcome =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: tab.x,
            row: tab.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(matches!(
        &outcome.actions[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(
                &request.method,
                crate::api::schema::Method::TabFocus(target) if target.tab_id == "tab_1"
            )
    ));
    assert_eq!(state.tab_scroll, 0);
    state.compose(106, 20).expect("tab bar after wheel");
    assert!(state.hits.tabs.iter().any(|(_, tab_id)| tab_id == "tab_1"));
}

#[test]
fn context_menu_keyboard_and_outside_click_are_client_owned() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("composed frame");
    let tab = state.hits.tabs[0].0;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: tab.x + 1,
        row: tab.y,
        modifiers: KeyModifiers::empty(),
    })]);
    state.compose(106, 20).expect("tab context menu");
    let moved = state.handle_input_bytes(b"\x1b[B");
    assert!(moved.repaint);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            highlighted: 1,
            ..
        }))
    ));
    let text = state.handle_raw_events(vec![RawInputEvent::Text(crate::input::TextCommit::new(
        "not pane input",
    ))]);
    assert!(text.requests.is_empty());
    let paste = state.handle_raw_events(vec![RawInputEvent::Paste("not pane input".into())]);
    assert!(paste.requests.is_empty());
    let outside =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 105,
            row: 19,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(outside.repaint);
    assert!(state.overlay.is_none());
}
