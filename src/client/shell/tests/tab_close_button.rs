use super::*;

fn close_button_state(tabs: usize, enabled: bool) -> ClientShellState {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_close_button = enabled;
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
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(surface());
    state
}

fn cell(frame: &FrameData, x: u16, y: u16) -> String {
    frame.cells[usize::from(y) * usize::from(frame.width) + usize::from(x)]
        .symbol
        .to_string()
}

fn click_at(column: u16, row: u16, modifiers: KeyModifiers) -> Vec<RawInputEvent> {
    let press = crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers,
    };
    vec![
        RawInputEvent::Mouse(press),
        RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..press
        }),
    ]
}

fn closed_tab_id(outcome: &ClientShellInput) -> Option<String> {
    outcome.actions.iter().find_map(|action| match action {
        ClientShellAction::Endpoint { request, .. } => match &request.method {
            crate::api::schema::Method::TabClose(target) => Some(target.tab_id.clone()),
            _ => None,
        },
        _ => None,
    })
}

fn focused_tab_id(outcome: &ClientShellInput) -> Option<String> {
    outcome.actions.iter().find_map(|action| match action {
        ClientShellAction::Endpoint { request, .. } => match &request.method {
            crate::api::schema::Method::TabFocus(target) => Some(target.tab_id.clone()),
            _ => None,
        },
        _ => None,
    })
}

/// The marker has to land on the cell the click handler reads, and it has to be absent by default:
/// the feature is opt-in, so with it off the last cell must stay part of the label background.
#[test]
fn tab_close_marker_occupies_the_last_label_cell_only_when_enabled() {
    let mut enabled = close_button_state(3, true);
    let frame = enabled
        .compose(106, 20)
        .expect("tab bar with close markers");
    assert_eq!(enabled.hits.tabs.len(), 3);
    for (rect, tab_id) in &enabled.hits.tabs {
        assert_eq!(
            cell(&frame, rect.right() - 1, rect.y),
            "x",
            "close marker in the last cell of {tab_id}"
        );
    }
    let first = enabled.hits.tabs[0].0;
    assert_eq!(
        cell(&frame, first.x, first.y),
        " ",
        "the label keeps its leading padding"
    );

    let mut disabled = close_button_state(3, false);
    let frame = disabled.compose(106, 20).expect("control: plain tab bar");
    assert_eq!(disabled.hits.tabs.len(), 3);
    for (rect, tab_id) in &disabled.hits.tabs {
        assert_eq!(
            cell(&frame, rect.right() - 1, rect.y),
            " ",
            "control: no marker on {tab_id} when the setting is off"
        );
    }
}

/// The whole point of the gesture: Alt-click the marker of a tab you are not on and it closes,
/// without first being focused. Asserting "a close was requested" is not enough — it has to be the
/// tab that was clicked, and no focus change may ride along.
#[test]
fn alt_click_on_the_marker_closes_that_tab_without_focusing_it() {
    let mut state = close_button_state(3, true);
    state.compose(106, 20).expect("tab bar with close markers");
    let (rect, tab_id) = state
        .hits
        .tabs
        .iter()
        .find(|(_, tab_id)| tab_id == "tab_3")
        .map(|(rect, tab_id)| (*rect, tab_id.clone()))
        .expect("third tab");
    assert_ne!(
        state.snapshot.as_deref().expect("snapshot").focused_tab_id,
        Some(tab_id.clone()),
        "the tab under test must be a background tab"
    );

    let outcome = state.handle_raw_events(click_at(rect.right() - 1, rect.y, KeyModifiers::ALT));
    assert_eq!(closed_tab_id(&outcome).as_deref(), Some("tab_3"));
    assert_eq!(
        focused_tab_id(&outcome),
        None,
        "closing a background tab must not focus it first"
    );
}

/// Three ways the gesture must NOT fire. Each is a real click a user makes constantly, and any of
/// them closing a tab would be data loss, so they are asserted separately rather than as one case.
#[test]
fn the_close_gesture_needs_the_marker_cell_the_modifier_and_the_setting() {
    let mut state = close_button_state(3, true);
    state.compose(106, 20).expect("tab bar with close markers");
    let rect = state
        .hits
        .tabs
        .iter()
        .find(|(_, tab_id)| tab_id == "tab_3")
        .map(|(rect, _)| *rect)
        .expect("third tab");

    let plain = state.handle_raw_events(click_at(rect.right() - 1, rect.y, KeyModifiers::empty()));
    assert_eq!(closed_tab_id(&plain), None, "a plain click must not close");
    assert_eq!(
        focused_tab_id(&plain).as_deref(),
        Some("tab_3"),
        "a plain click on the marker still activates the tab"
    );

    let body = state.handle_raw_events(click_at(rect.x + 1, rect.y, KeyModifiers::ALT));
    assert_eq!(
        closed_tab_id(&body),
        None,
        "Alt-clicking the label body must not close"
    );
    assert_eq!(
        focused_tab_id(&body).as_deref(),
        Some("tab_3"),
        "Alt-clicking the label body keeps the normal press behaviour"
    );

    let mut off = close_button_state(3, false);
    off.compose(106, 20).expect("control: plain tab bar");
    let rect = off
        .hits
        .tabs
        .iter()
        .find(|(_, tab_id)| tab_id == "tab_3")
        .map(|(rect, _)| *rect)
        .expect("third tab");
    let outcome = off.handle_raw_events(click_at(rect.right() - 1, rect.y, KeyModifiers::ALT));
    assert_eq!(
        closed_tab_id(&outcome),
        None,
        "with the setting off the marker cell is an ordinary label cell"
    );
    assert_eq!(focused_tab_id(&outcome).as_deref(), Some("tab_3"));
}

/// A marker on a row the mode bar has drawn over is not on screen, so clicking where it used to be
/// must not close anything. This rides on the same hit map the mode bar clears, so it is asserted
/// rather than assumed.
#[test]
fn a_marker_swallowed_by_the_mode_bar_does_not_close_its_tab() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_close_button = true;
    config.tab_bar_position = crate::config::TabBarPositionConfig::Bottom;
    let mut state = ClientShellState::new(config);
    let mut projected = snapshot();
    projected.tabs.extend((2..=3).map(|number| ClientShellTab {
        tab_id: format!("tab_{number}"),
        workspace_id: "ws_1".into(),
        number,
        label: number.to_string(),
        custom_label: false,
        zoomed: false,
        focused: false,
        agent_status: AgentStatus::Idle,
    }));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());

    state.compose(106, 20).expect("terminal-mode bottom bar");
    let rect = state
        .hits
        .tabs
        .iter()
        .find(|(_, tab_id)| tab_id == "tab_3")
        .map(|(rect, _)| *rect)
        .expect("third tab");
    let marker = (rect.right() - 1, rect.y);

    state.mode = ClientShellMode::Navigate;
    state.compose(106, 20).expect("navigate-mode bottom bar");
    let swallowed = state.handle_raw_events(click_at(marker.0, marker.1, KeyModifiers::ALT));
    assert_eq!(
        closed_tab_id(&swallowed),
        None,
        "the mode bar covers the marker, so the click must not close"
    );

    // Non-vacuity: the identical click closes once the mode bar is gone.
    state.mode = ClientShellMode::Terminal;
    state
        .compose(106, 20)
        .expect("terminal-mode bottom bar again");
    let live = state.handle_raw_events(click_at(marker.0, marker.1, KeyModifiers::ALT));
    assert_eq!(closed_tab_id(&live).as_deref(), Some("tab_3"));
}

/// Wrapped rows reuse the same marker placement, so a tab that flowed onto a lower row must be
/// closable at its own row's last cell.
#[test]
fn the_close_marker_works_on_a_wrapped_lower_row() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_close_button = true;
    config.tab_bar_wrap = true;
    let mut projected = snapshot();
    projected.tabs.extend((2..=9).map(|number| ClientShellTab {
        tab_id: format!("tab_{number}"),
        workspace_id: "ws_1".into(),
        number,
        label: number.to_string(),
        custom_label: false,
        zoomed: false,
        focused: false,
        agent_status: AgentStatus::Idle,
    }));
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());

    let frame = state.compose(80, 20).expect("wrapped tab bar");
    let bar_y = state.layout(80, 20).tab_bar.y;
    let (rect, tab_id) = state
        .hits
        .tabs
        .iter()
        .find(|(rect, _)| rect.y > bar_y)
        .map(|(rect, tab_id)| (*rect, tab_id.clone()))
        .expect("a tab wrapped onto a lower row");
    assert_eq!(
        cell(&frame, rect.right() - 1, rect.y),
        "x",
        "the wrapped row draws the marker too"
    );

    let outcome = state.handle_raw_events(click_at(rect.right() - 1, rect.y, KeyModifiers::ALT));
    assert_eq!(closed_tab_id(&outcome).as_deref(), Some(tab_id.as_str()));
}

/// The modifier is configurable, and configuring it must move the gesture rather than add to it:
/// the new modifier has to fire and the old one has to stop firing, or a user who chose ctrl still
/// closes tabs by accident with Alt.
#[test]
fn the_configured_modifier_arms_the_marker_and_the_default_stops_firing() {
    let mut state = close_button_state(3, true);
    state.config.tab_close_button_modifier = KeyModifiers::CONTROL;
    state.compose(106, 20).expect("tab bar with close markers");
    let rect = state
        .hits
        .tabs
        .iter()
        .find(|(_, tab_id)| tab_id == "tab_3")
        .map(|(rect, _)| *rect)
        .expect("third tab");
    let marker = (rect.right() - 1, rect.y);

    let with_alt = state.handle_raw_events(click_at(marker.0, marker.1, KeyModifiers::ALT));
    assert_eq!(
        closed_tab_id(&with_alt),
        None,
        "Alt must no longer close once ctrl is configured"
    );
    assert_eq!(
        focused_tab_id(&with_alt).as_deref(),
        Some("tab_3"),
        "it falls through to the ordinary press instead"
    );

    let with_ctrl = state.handle_raw_events(click_at(marker.0, marker.1, KeyModifiers::CONTROL));
    assert_eq!(closed_tab_id(&with_ctrl).as_deref(), Some("tab_3"));
}

/// A combination must require every modifier in it, not any one of them.
#[test]
fn a_combination_modifier_requires_all_of_its_parts() {
    let mut state = close_button_state(3, true);
    state.config.tab_close_button_modifier = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    state.compose(106, 20).expect("tab bar with close markers");
    let rect = state
        .hits
        .tabs
        .iter()
        .find(|(_, tab_id)| tab_id == "tab_3")
        .map(|(rect, _)| *rect)
        .expect("third tab");
    let marker = (rect.right() - 1, rect.y);

    let partial = state.handle_raw_events(click_at(marker.0, marker.1, KeyModifiers::CONTROL));
    assert_eq!(
        closed_tab_id(&partial),
        None,
        "ctrl alone is not ctrl+shift"
    );

    let both = state.handle_raw_events(click_at(
        marker.0,
        marker.1,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert_eq!(closed_tab_id(&both).as_deref(), Some("tab_3"));
}
