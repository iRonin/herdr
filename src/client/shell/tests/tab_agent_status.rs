use super::super::agent_sidebar::render_agent_panel;
use super::super::render::render_tab_bar;
use super::*;

fn tab_status_config(enabled: bool) -> ClientShellConfig {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.tab_agent_status = enabled;
    config
}

/// One fixture for BOTH surfaces, so the two renders below are always compared
/// under the same input. The tab's own `agent_status` field is deliberately set
/// to `Unknown` while the agent entry carries the real status: the mark must be
/// computed from the tab's highest-attention AGENT, not from the tab's
/// all-panes aggregate, and a fixture where both agree cannot detect the
/// difference.
fn snapshot_with_agent(status: AgentStatus) -> ClientShellSnapshot {
    let mut projected = snapshot();
    projected.tabs[0].agent_status = AgentStatus::Unknown;
    projected.agents.push(ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some("cli".into()),
        display_agent: None,
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: 0,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
    });
    projected
}

/// Renders the real tab bar and returns the status mark cell. The rect comes
/// from the hit map the renderer itself fills in, so the anchor cannot drift
/// from the paint.
fn rendered_tab_mark(
    config: &ClientShellConfig,
    projected: &ClientShellSnapshot,
) -> (String, Option<ratatui::style::Color>) {
    let area = Rect::new(0, 0, 60, 1);
    let mut buffer = Buffer::empty(area);
    let mut hits = ShellHitMap::default();
    let mut tab_scroll = 0usize;
    let mut reveal_focused_tab = false;
    render_tab_bar(
        &mut buffer,
        area,
        projected,
        config,
        &mut tab_scroll,
        &mut reveal_focused_tab,
        None,
        &mut hits,
    );
    let (rect, tab_id) = hits.tabs.first().expect("the snapshot has one tab").clone();
    assert_eq!(tab_id, "tab_1");
    let cell = &buffer[(rect.x, rect.y)];
    (cell.symbol().to_string(), cell.style().fg)
}

/// Renders the real agent panel and returns the AGENT DETAIL ROW's mark cell.
/// The row rect comes from the hit map, and the mark sits one cell into the row
/// (the row's leading indent), so this reads the very cell a user sees in the
/// sidebar. If that layout ever moves, the vacuity guard in the test below
/// fires rather than the comparison silently succeeding on two blanks.
fn rendered_agent_panel_mark(
    config: &ClientShellConfig,
    projected: &ClientShellSnapshot,
) -> (String, Option<ratatui::style::Color>) {
    let area = Rect::new(0, 0, 30, 10);
    let mut buffer = Buffer::empty(area);
    let mut hits = ShellHitMap::default();
    let mut agent_scroll = 0usize;
    render_agent_panel(
        &mut buffer,
        area,
        projected,
        config,
        &mut agent_scroll,
        &mut hits,
    );
    let (rect, pane_id) = hits.agents.first().expect("one agent row").clone();
    assert_eq!(pane_id, "pane_1");
    let cell = &buffer[(rect.x + 1, rect.y)];
    (cell.symbol().to_string(), cell.style().fg)
}

/// The binding cross-surface test: for the same input the tab mark must equal
/// BOTH the agent panel's rendered mark AND upstream's own `status_icon` /
/// `status_color` called directly. The first assertion alone would pass if both
/// surfaces were routed through a fork-local glyph table; the second alone
/// would pass if the agent panel were forked and the tab left alone. Colours
/// are compared too: the symbol alphabet is ambiguous by design (the filled dot
/// covers blocked, working and done; the hollow dot covers idle), so only
/// colour separates the states a one-cell mark can carry.
#[test]
fn tab_status_mark_is_the_agent_panels_mark_and_status_icons_own() {
    for status in [
        AgentStatus::Blocked,
        AgentStatus::Done,
        AgentStatus::Working,
        AgentStatus::Idle,
        AgentStatus::Unknown,
    ] {
        let config = tab_status_config(true);
        let projected = snapshot_with_agent(status);
        let (tab_symbol, tab_fg) = rendered_tab_mark(&config, &projected);
        let (panel_symbol, panel_fg) = rendered_agent_panel_mark(&config, &projected);
        let expected_symbol = status_icon(status, config.status_indicators);
        let expected_fg = status_color(status, &config.palette);

        // Vacuity guards: a render helper that silently draws nothing returns a
        // blank on either side, and the comparisons below would then pass for
        // no reason at all.
        assert_ne!(
            tab_symbol, " ",
            "tab fixture produced no mark for {status:?}"
        );
        assert_ne!(
            panel_symbol, " ",
            "agent panel fixture produced no mark for {status:?}"
        );

        // (1) the two surfaces agree — glyph and colour.
        assert_eq!(
            tab_symbol, panel_symbol,
            "tab and agent panel must render the same mark for {status:?}"
        );
        assert_eq!(
            tab_fg, panel_fg,
            "tab and agent panel must render the same colour for {status:?}"
        );
        // (2) they agree ON upstream's own function
        // (2) they agree ON upstream's own function, not on a shared fork-local
        // wrapper. Without this, routing both surfaces through a fork-local
        // glyph table would pass (1) — the one thing that must not happen.
        assert_eq!(
            tab_symbol, expected_symbol,
            "tab must render status_icon's mark for {status:?}"
        );
        assert_eq!(
            tab_fg,
            Some(expected_fg),
            "tab must render status_color's colour for {status:?}"
        );
    }
}

/// The feature is opt-in: with it off, the tab must render byte-identically to
/// stock — same rect, same cells, no mark — and with it on, the tab widens by
/// exactly the two cells the mark occupies and the label keeps its own cells.
#[test]
fn tab_status_mark_appears_only_when_enabled_and_widens_by_two() {
    let projected = snapshot_with_agent(AgentStatus::Blocked);

    let mut enabled = ClientShellState::new(tab_status_config(true));
    enabled.set_snapshot(Box::new(projected));
    enabled.set_pane_surface(surface());
    let enabled_frame = enabled
        .compose(106, 20)
        .expect("tab bar with the status mark");
    let (enabled_rect, _) = enabled.hits.tabs.first().expect("one tab").clone();
    let cell = |x: u16, y: u16| {
        enabled_frame.cells[usize::from(y) * usize::from(enabled_frame.width) + usize::from(x)]
            .symbol
            .clone()
    };
    // The mark is the filled dot in the head cell, the name follows the separator.
    assert_eq!(cell(enabled_rect.x, enabled_rect.y), "●");

    let mut disabled = ClientShellState::new(tab_status_config(false));
    disabled.set_snapshot(Box::new(snapshot_with_agent(AgentStatus::Blocked)));
    disabled.set_pane_surface(surface());
    let disabled_frame = disabled.compose(106, 20).expect("control: stock tab bar");
    let (disabled_rect, _) = disabled.hits.tabs.first().expect("one tab").clone();
    assert_eq!(
        disabled_frame.cells[usize::from(disabled_rect.y) * usize::from(disabled_frame.width)
            + usize::from(disabled_rect.x)]
        .symbol,
        " ",
        "with the feature off the head cell is ordinary label background"
    );
    assert_eq!(
        enabled_rect.width,
        disabled_rect.width + 2,
        "the mark must widen the tab by exactly its own cell and the separator"
    );
    assert_eq!(enabled_rect.y, disabled_rect.y, "the row must not move");
}

/// A tab with no agent renders exactly as stock even with the feature on: the
/// mark is a property of a tab holding an agent, not of the setting alone.
#[test]
fn tabs_without_agents_render_stock_when_the_feature_is_on() {
    let mut projected = snapshot();
    // A second, agent-less tab so both states sit in one bar.
    projected.tabs.push(ClientShellTab {
        tab_id: "tab_2".into(),
        workspace_id: "ws_1".into(),
        number: 2,
        label: "2".into(),
        custom_label: false,
        zoomed: false,
        focused: false,
        agent_status: AgentStatus::Unknown,
    });
    projected.agents.push(ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some("cli".into()),
        display_agent: None,
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Working,
        state_change_seq: 0,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
    });
    let mut state = ClientShellState::new(tab_status_config(true));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    let frame = state.compose(106, 20).expect("mixed tab bar");

    let tab = |id: &str| {
        state
            .hits
            .tabs
            .iter()
            .find(|(_, tab_id)| tab_id == id)
            .map(|(rect, _)| *rect)
            .unwrap_or_else(|| panic!("{id} tab"))
    };
    let with_agent = tab("tab_1");
    let without_agent = tab("tab_2");
    assert_eq!(
        with_agent.width,
        without_agent.width + 2,
        "only the tab holding an agent widens"
    );
    let symbol = |rect: Rect| {
        frame.cells[usize::from(rect.y) * usize::from(frame.width) + usize::from(rect.x)]
            .symbol
            .clone()
    };
    assert_eq!(symbol(with_agent), "●");
    assert_eq!(symbol(without_agent), " ", "no mark on an agent-less tab");

    // Stock control: with the feature off both tabs are the same width again.
    let mut stock = ClientShellState::new(tab_status_config(false));
    let mut projected = snapshot();
    projected.tabs.push(ClientShellTab {
        tab_id: "tab_2".into(),
        workspace_id: "ws_1".into(),
        number: 2,
        label: "2".into(),
        custom_label: false,
        zoomed: false,
        focused: false,
        agent_status: AgentStatus::Unknown,
    });
    stock.set_snapshot(Box::new(projected));
    stock.set_pane_surface(surface());
    stock.compose(106, 20).expect("stock control frame");
    let stock_tab = |id: &str| {
        stock
            .hits
            .tabs
            .iter()
            .find(|(_, tab_id)| tab_id == id)
            .map(|(rect, _)| *rect)
            .unwrap_or_else(|| panic!("{id} stock tab"))
    };
    assert_eq!(
        stock_tab("tab_1").width,
        stock_tab("tab_2").width,
        "stock renders both tabs identically wide"
    );
    assert_eq!(stock_tab("tab_1").width, without_agent.width);
}

/// The mark belongs to the tab's highest-attention agent, mirroring the agent
/// panel's own ordering: a blocked agent outranks a working one in the same tab.
#[test]
fn tab_status_mark_shows_the_highest_attention_agent() {
    let config = tab_status_config(true);
    let mut projected = snapshot();
    projected.agents.push(ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some("working".into()),
        display_agent: None,
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Working,
        state_change_seq: 0,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
    });
    projected.agents.push(ClientShellAgent {
        pane_id: "pane_2".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some("blocked".into()),
        display_agent: None,
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Blocked,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
    });
    let (symbol, fg) = rendered_tab_mark(&config, &projected);
    assert_ne!(symbol, " ", "the tab must render a mark at all");
    assert_eq!(
        symbol,
        status_icon(AgentStatus::Blocked, config.status_indicators)
    );
    assert_eq!(
        fg,
        Some(status_color(AgentStatus::Blocked, &config.palette))
    );
}

/// Wrapped rows reflow against the SAME desired widths the single-row bar uses,
/// so the mark must survive on a tab that flowed onto a lower row and the row
/// accounting must already include its two cells — otherwise the wrapped bar and
/// the hit rects drift apart.
#[test]
fn tab_status_mark_survives_on_wrapped_rows() {
    let mut config = tab_status_config(true);
    config.tab_bar_wrap = true;
    let mut projected = snapshot();
    // Nine one-char tabs cannot share an 80-column bar once each carries a mark.
    projected.tabs.extend((2..=9).map(|number| ClientShellTab {
        tab_id: format!("tab_{number}"),
        workspace_id: "ws_1".into(),
        number,
        label: number.to_string(),
        custom_label: false,
        zoomed: false,
        focused: false,
        agent_status: AgentStatus::Unknown,
    }));
    projected
        .agents
        .extend((1..=9).map(|number| ClientShellAgent {
            pane_id: format!("pane_{number}"),
            workspace_id: "ws_1".into(),
            tab_id: format!("tab_{number}"),
            name: Some("cli".into()),
            display_agent: None,
            agent: Some("claude".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            // Blocked survives the client's acknowledged-seen projection unchanged, so the
            // composed frame carries the same status this fixture set (Done would not).
            agent_status: AgentStatus::Blocked,
            state_change_seq: u64::try_from(number).unwrap(),
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: number == 1,
        }));
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    let frame = state.compose(80, 20).expect("wrapped tab bar with marks");
    let bar_y = state.layout(80, 20).tab_bar.y;

    let lower_row_tabs: Vec<(Rect, String)> = state
        .hits
        .tabs
        .iter()
        .filter(|(rect, _)| rect.y > bar_y)
        .cloned()
        .collect();
    assert!(
        !lower_row_tabs.is_empty(),
        "fixture must wrap: every tab is on the first row"
    );
    for (rect, tab_id) in &lower_row_tabs {
        assert_eq!(
            frame.cells[usize::from(rect.y) * usize::from(frame.width) + usize::from(rect.x)]
                .symbol,
            "●",
            "the wrapped row draws {tab_id}'s mark too"
        );
    }
}
