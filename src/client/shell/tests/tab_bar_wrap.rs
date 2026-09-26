use super::*;

fn wrapping_snapshot(tabs: usize) -> ClientShellSnapshot {
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
    snapshot
}

fn wrapping_state(tabs: usize, wrap: bool) -> ClientShellState {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_bar_wrap = wrap;
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(wrapping_snapshot(tabs)));
    state.set_pane_surface(surface());
    state
}

fn tab_rows(state: &ClientShellState) -> std::collections::BTreeSet<u16> {
    let mut rows = state
        .hits
        .tabs
        .iter()
        .map(|(rect, _)| rect.y)
        .collect::<std::collections::BTreeSet<_>>();
    if !state.hits.new_tab.is_empty() {
        rows.insert(state.hits.new_tab.y);
    }
    rows
}

fn frame_row(frame: &FrameData, row: u16) -> String {
    let width = usize::from(frame.width);
    let start = usize::from(row) * width;
    frame.cells[start..start + width]
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect()
}

/// The wrapped bar is only useful if the rows it needs are actually reserved: the panes below have
/// to shrink by exactly the rows the tabs occupy, or the tab bar would overdraw live pane content.
/// The `wrap = false` half is the control — it must reproduce upstream's single-row geometry
/// unchanged for the same snapshot, which is what makes this feature default-preserving.
#[test]
fn wrapped_tab_bar_reserves_its_rows_and_shrinks_the_pane_surface() {
    let unwrapped = wrapping_state(9, false).layout(80, 20);
    assert_eq!(unwrapped.tab_bar.height, 1, "control: no wrap, one row");
    assert_eq!(unwrapped.pane_surface.y, 1);
    assert_eq!(unwrapped.pane_surface.height, 19);

    let wrapped = wrapping_state(9, true).layout(80, 20);
    assert!(
        wrapped.tab_bar.height > 1,
        "nine tabs at 80 columns need more than one row, got {}",
        wrapped.tab_bar.height
    );
    assert_eq!(wrapped.tab_bar.y, 0);
    assert_eq!(wrapped.pane_surface.y, wrapped.tab_bar.height);
    assert_eq!(
        wrapped.pane_surface.height,
        20 - wrapped.tab_bar.height,
        "panes give up exactly the wrapped rows"
    );
    assert_eq!(wrapped.tab_bar.width, unwrapped.tab_bar.width);
    assert_eq!(wrapped.pane_surface.x, unwrapped.pane_surface.x);
}

/// The reserved height and the rows the renderer lays out are computed by two different call paths
/// (`layout` before rendering, the flow inside the renderer). If they disagree the bar either
/// overdraws a pane row or leaves a dead stripe, and no other assertion in the suite would notice.
#[test]
fn wrapped_tab_bar_height_matches_the_rows_it_lays_out() {
    // Above `DEFAULT_MOBILE_WIDTH_THRESHOLD`; narrower widths take the mobile layout, which has no
    // tab bar at all.
    for columns in [70u16, 80, 106, 140, 200] {
        let mut state = wrapping_state(11, true);
        state.compose(columns, 24).expect("wrapped tab bar");
        let layout = state.layout(columns, 24);
        let rows = tab_rows(&state);
        assert!(!rows.is_empty(), "columns={columns}: nothing was laid out");
        assert_eq!(
            layout.tab_bar.height,
            rows.iter().copied().max().expect("row") + 1,
            "columns={columns}: reserved {} rows, laid out {rows:?}",
            layout.tab_bar.height
        );
        assert!(
            rows.iter().all(|row| *row < layout.tab_bar.bottom()),
            "columns={columns}: a row escaped the bar: {rows:?}"
        );
    }
}

/// Wrapping exists so no tab is hidden. Every tab must be present and clickable, and the scroll
/// affordances that only make sense on a single row must not be drawn.
#[test]
fn wrapped_tab_bar_shows_every_tab_across_rows_without_scroll_chrome() {
    let mut state = wrapping_state(9, true);
    let frame = state.compose(80, 20).expect("wrapped tab bar");
    let layout = state.layout(80, 20);

    assert_eq!(state.hits.tabs.len(), 9, "every tab is hit-testable");
    assert!(
        tab_rows(&state).len() > 1,
        "the tabs actually wrapped: {:?}",
        tab_rows(&state)
    );
    assert!(state.hits.tab_scroll_left.is_empty());
    assert!(state.hits.tab_scroll_right.is_empty());
    assert!(!state.hits.new_tab.is_empty(), "new-tab control is placed");
    assert_eq!(state.tab_scroll, 0);

    let bar = (0..layout.tab_bar.height)
        .map(|row| frame_row(&frame, layout.tab_bar.y + row))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(bar.contains('+'), "new-tab control rendered: {bar:?}");
    for number in 1..=9 {
        assert!(
            bar.contains(&number.to_string()),
            "tab {number} rendered: {bar:?}"
        );
    }
    assert!(!bar.contains('…'), "no overflow ellipsis: {bar:?}");
    assert!(!bar.contains('<'), "no scroll-left control: {bar:?}");
    assert!(!bar.contains('>'), "no scroll-right control: {bar:?}");
}

/// A tab that wrapped onto a lower row is only reachable if hit testing is row-aware end to end.
/// Asserting the rect exists is not enough — the click has to resolve to that tab's id.
#[test]
fn wrapped_tab_bar_click_on_a_lower_row_focuses_that_tab() {
    let mut state = wrapping_state(9, true);
    state.compose(80, 20).expect("wrapped tab bar");
    let bar_y = state.layout(80, 20).tab_bar.y;
    let (rect, tab_id) = state
        .hits
        .tabs
        .iter()
        .find(|(rect, _)| rect.y > bar_y)
        .map(|(rect, tab_id)| (*rect, tab_id.clone()))
        .expect("a tab wrapped onto a lower row");

    let press = crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x + rect.width / 2,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    };
    let outcome = state.handle_raw_events(vec![
        RawInputEvent::Mouse(press),
        RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..press
        }),
    ]);
    assert!(
        outcome.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(
                    &request.method,
                    crate::api::schema::Method::TabFocus(target) if target.tab_id == tab_id
                )
        )),
        "clicking {tab_id} on row {} focuses it: {:?}",
        rect.y,
        outcome.actions
    );
}

/// Upstream lets a mode bar take over the tab row when the bar sits at the bottom, and clears the
/// tab hits so an invisible tab cannot be clicked. A wrapped bar is taller than the mode bar, so
/// that swallow must be scoped to the covered row: the rows still on screen have to stay clickable.
/// Asserting only "some tabs survive" would pass if the swallow stopped working altogether, so this
/// also asserts that the covered row's tabs really were removed.
#[test]
fn bottom_wrapped_tab_bar_mode_bar_swallows_only_the_row_it_covers() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_bar_wrap = true;
    config.tab_bar_position = crate::config::TabBarPositionConfig::Bottom;
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(wrapping_snapshot(9)));
    state.set_pane_surface(surface());

    state.compose(80, 20).expect("terminal-mode wrapped bar");
    let layout = state.layout(80, 20);
    let covered_row = layout.tab_bar.bottom() - 1;
    let unswallowed = state.hits.tabs.len();
    let on_covered_row = state
        .hits
        .tabs
        .iter()
        .filter(|(rect, _)| rect.y == covered_row)
        .count();
    assert!(
        on_covered_row > 0,
        "the control needs tabs on the row the mode bar will cover"
    );

    state.mode = ClientShellMode::Navigate;
    state.compose(80, 20).expect("navigate-mode wrapped bar");
    assert_eq!(
        state.layout(80, 20).tab_bar,
        layout.tab_bar,
        "the mode bar must not change the bar's geometry"
    );
    assert!(
        state
            .hits
            .tabs
            .iter()
            .all(|(rect, _)| rect.y != covered_row),
        "tabs under the mode bar are not clickable: {:?}",
        state.hits.tabs
    );
    assert_eq!(
        state.hits.tabs.len(),
        unswallowed - on_covered_row,
        "only the covered row was swallowed"
    );
    assert!(
        !state.hits.tabs.is_empty(),
        "rows above the mode bar stay clickable"
    );
}

/// The single-row bar at the bottom is the case upstream ships: there the mode bar is the whole
/// bar, so every tab hit must still go. This is the reduction the row-scoped guard has to preserve.
#[test]
fn bottom_single_row_tab_bar_mode_bar_still_swallows_the_whole_bar() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_bar_position = crate::config::TabBarPositionConfig::Bottom;
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(wrapping_snapshot(9)));
    state.set_pane_surface(surface());

    state.compose(80, 20).expect("terminal-mode single row");
    assert_eq!(state.layout(80, 20).tab_bar.height, 1);
    assert!(!state.hits.tabs.is_empty());

    state.mode = ClientShellMode::Navigate;
    state.compose(80, 20).expect("navigate-mode single row");
    assert!(state.hits.tabs.is_empty());
    assert!(state.hits.new_tab.is_empty());
    assert!(state.hits.tab_scroll_left.is_empty());
    assert!(state.hits.tab_scroll_right.is_empty());
}

/// A wrapped bar may still need more rows than it is allowed to take. It must then scroll its rows
/// rather than drop the focused tab off-screen, which is the only reason the clamp is survivable.
#[test]
fn wrapped_tab_bar_keeps_the_focused_tab_visible_when_rows_are_clamped() {
    let mut snapshot = wrapping_snapshot(40);
    snapshot.focused_tab_id = Some("tab_40".into());
    for tab in &mut snapshot.tabs {
        tab.focused = tab.tab_id == "tab_40";
    }
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_bar_wrap = true;
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(surface());

    state.compose(70, 12).expect("clamped wrapped bar");
    let layout = state.layout(70, 12);
    assert_eq!(
        layout.tab_bar.height, 6,
        "the bar is clamped to half the height"
    );
    assert!(
        state.hits.tabs.len() < 40,
        "the clamp really did push rows off-window"
    );
    assert!(
        state.hits.tabs.iter().any(|(_, tab_id)| tab_id == "tab_40"),
        "the focused tab stays reachable: {:?}",
        state.hits.tabs
    );
}
