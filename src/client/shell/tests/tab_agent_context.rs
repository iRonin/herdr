use super::*;
use crate::client::shell::render::display_width;

fn tab_context_config(enabled: bool) -> ClientShellConfig {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_agent_context = enabled;
    config
}

fn snapshot_with_reporting_agent(display_agent: &str) -> ClientShellSnapshot {
    let mut projected = snapshot();
    projected.tabs[0].label = "Pane".into();
    projected.agents.push(ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some("cli".into()),
        display_agent: Some(display_agent.to_owned()),
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        // Blocked survives the client's acknowledged-seen projection unchanged,
        // so the composed frame carries what this fixture set.
        agent_status: AgentStatus::Blocked,
        state_change_seq: 0,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
    });
    projected
}

fn composed_tab_row(state: &ClientShellState, frame: &FrameData) -> (Rect, Vec<String>) {
    let (rect, tab_id) = state.hits.tabs.first().expect("one tab");
    assert_eq!(tab_id, "tab_1");
    let symbols = (rect.x..rect.x + rect.width)
        .map(|x| {
            frame.cells[usize::from(rect.y) * usize::from(frame.width) + usize::from(x)]
                .symbol
                .clone()
        })
        .collect();
    (*rect, symbols)
}

fn composed_state(config: ClientShellConfig, display_agent: &str) -> (ClientShellState, FrameData) {
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot_with_reporting_agent(display_agent)));
    state.set_pane_surface(surface());
    let frame = state.compose(106, 20).expect("tab bar frame");
    (state, frame)
}

/// The decorations render on the tab AND the tab widens by exactly their display
/// width: a wide marker glyph that only added its character count would corrupt
/// the label geometry, which is the failure the width assertion exists to catch.
#[test]
fn context_and_marker_compose_on_the_tab_with_exact_width() {
    let display_agent = "🥷✅ ~42%·4242 $1.23";

    let (enabled, frame) = composed_state(tab_context_config(true), display_agent);
    let (rect, row) = composed_tab_row(&enabled, &frame);

    let joined = row.join("");
    assert!(joined.contains("✅"), "the marker must render: {joined:?}");
    assert!(
        joined.contains("~42%"),
        "the reading must render: {joined:?}"
    );
    assert!(
        joined.contains("Pane"),
        "the pane name must render: {joined:?}"
    );
    // The exact width relationship, in display cells: the decorations add the
    // marker (2 cells), its separator, the reading's separator and its text —
    // on top of the stock padding, never out of it.
    assert_eq!(
        rect.width,
        display_width("✅") + 1 + display_width("Pane") + 1 + display_width("~42%") + 4,
        "the tab must account for every decoration cell"
    );

    // Control: the key off must render the stock label byte-identically.
    let (disabled, stock_frame) = composed_state(tab_context_config(false), display_agent);
    let (stock_rect, stock_row) = composed_tab_row(&disabled, &stock_frame);
    let stock_joined = stock_row.join("");
    assert!(!stock_joined.contains("~42%"), "no reading when off");
    assert!(!stock_joined.contains("✅"), "no marker when off");
    assert_eq!(
        stock_rect.width,
        display_width("Pane") + 4,
        "stock width is the pane name plus the stock padding"
    );
}

/// A marker is separated from the pane name by ONE real space cell. The marker
/// may be double-width, so the assertion steps over its full character extent
/// before reading the separator and the name.
#[test]
fn tab_lifecycle_marker_has_a_separator_before_the_pane_name() {
    for (display_agent, marker) in [("🥷✅ 52%·4242", "✅"), ("🥷❓3 52%·4242", "❓3")] {
        let (state, frame) = composed_state(tab_context_config(true), display_agent);
        let (rect, row) = composed_tab_row(&state, &frame);

        let marker_head = marker.chars().next().unwrap().to_string();
        let marker_x = row
            .iter()
            .position(|symbol| *symbol == marker_head)
            .unwrap_or_else(|| panic!("marker {marker:?} must render in {row:?}"));
        // A wide glyph's continuation cell reads as a space in extracted text but
        // is not the separator — step over the marker's full DISPLAY extent.
        let marker_extent = usize::from(display_width(marker));
        assert_eq!(
            row[marker_x + marker_extent],
            " ",
            "{marker:?} must be followed by a real separator cell, not its continuation cell"
        );
        assert_eq!(
            row[marker_x + marker_extent + 1],
            "P",
            "the pane name must begin immediately after the separator"
        );
        // The marker and its separator also count toward the tab geometry.
        assert_eq!(
            rect.width,
            display_width(marker) + 1 + display_width("Pane") + 1 + display_width("52%") + 4,
            "the separator must also count toward tab geometry for {marker:?}"
        );
    }
}

/// `ui.tab_agent_status` and `ui.tab_agent_context` compose in one label: the
/// status mark and its separator, then the marker, the name, the reading.
/// Both keys are independent — either alone must work.
#[test]
fn status_mark_and_context_compose_on_one_tab() {
    let mut config = tab_context_config(true);
    config.tab_agent_status = true;
    let (state, frame) = composed_state(config, "🥷✅ ~42%·4242 $1.23");
    let (rect, row) = composed_tab_row(&state, &frame);

    let mark = row
        .iter()
        .position(|symbol| symbol != " ")
        .expect("the tab draws something");
    assert_eq!(
        row[mark], "●",
        "the blocked status mark leads the label: {row:?}"
    );
    assert_eq!(row[mark + 1], " ", "one separator after the mark: {row:?}");
    assert_eq!(row[mark + 2], "✅", "then the lifecycle marker: {row:?}");
    let joined = row.join("");
    assert!(joined.contains("✅"), "marker present: {joined:?}");
    assert!(joined.contains("Pane"), "name present: {joined:?}");
    assert!(joined.contains("~42%"), "reading present: {joined:?}");
    // The two features' widths add: the mark's two cells on top of the
    // decorated label.
    assert_eq!(
        rect.width,
        2 + display_width("✅") + 1 + display_width("Pane") + 1 + display_width("~42%") + 4
    );
}

/// An agent that reports no context segment still carries its lifecycle marker:
/// absence means absence for the reading only, never for the whole label.
#[test]
fn marker_without_a_reading_still_renders() {
    let (state, frame) = composed_state(tab_context_config(true), "🥷✅ working");
    let (rect, row) = composed_tab_row(&state, &frame);
    let joined = row.join("");
    assert!(
        !joined.contains('%'),
        "no reading without a report: {joined:?}"
    );
    assert!(
        joined.contains("✅"),
        "the lifecycle marker still rides the label: {joined:?}"
    );
    assert_eq!(
        rect.width,
        display_width("✅") + 1 + display_width("Pane") + 4,
        "only the marker's cells widen the tab"
    );
}
