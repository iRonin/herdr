use std::collections::HashMap;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    widgets::Paragraph,
    Frame,
};

use super::sidebar::workspace_attention_priority;
use super::text::display_width_u16;
use super::widgets::panel_contrast_fg;
use crate::app::AppState;
use crate::terminal::{TerminalId, TerminalState};

const MIN_TAB_WIDTH: u16 = 8;
const NEW_TAB_WIDTH: u16 = 3;
const TAB_SCROLL_BUTTON_WIDTH: u16 = 3;

#[derive(Debug, Clone, Default)]
pub(crate) struct TabBarView {
    pub scroll: usize,
    pub tab_hit_areas: Vec<Rect>,
    pub scroll_left_hit_area: Rect,
    pub scroll_right_hit_area: Rect,
    pub new_tab_hit_area: Rect,
}

fn tab_width(ws: &crate::workspace::Workspace, tab_idx: usize) -> u16 {
    display_width_u16(&tab_chrome_label(ws, tab_idx))
        .saturating_add(4)
        .max(MIN_TAB_WIDTH)
}

fn tab_agent_terminal<'a>(
    ws: &crate::workspace::Workspace,
    terminals: &'a HashMap<TerminalId, TerminalState>,
    tab_idx: usize,
) -> Option<&'a TerminalState> {
    let tab = ws.tabs.get(tab_idx)?;
    tab.layout
        .pane_ids()
        .into_iter()
        .filter_map(|pane_id| {
            let pane = tab.panes.get(&pane_id)?;
            let terminal = terminals.get(&pane.attached_terminal_id)?;
            terminal
                .agent_name
                .as_deref()
                .or_else(|| terminal.effective_agent_label())?;
            Some((terminal, pane.seen))
        })
        // `max_by_key` keeps the last maximum, preserving layout order for ties.
        .max_by_key(|(terminal, seen)| workspace_attention_priority(terminal.state, *seen))
        .map(|(terminal, _)| terminal)
}

fn extract_tab_agent_context(display_agent: &str) -> Option<&str> {
    let mut found = None;
    for candidate in display_agent.split_whitespace() {
        let body = candidate.strip_prefix('~').unwrap_or(candidate);
        let Some((percentage, pid)) = body.split_once("%·") else {
            continue;
        };
        if percentage.is_empty()
            || !percentage.bytes().all(|byte| byte.is_ascii_digit())
            || percentage
                .parse::<u16>()
                .ok()
                .is_none_or(|value| value > 100)
            || pid.is_empty()
            || !pid.bytes().all(|byte| byte.is_ascii_digit())
        {
            continue;
        }

        let percentage_end = candidate.len() - '·'.len_utf8() - pid.len();
        let percentage = &candidate[..percentage_end];
        if found.is_some() {
            return None;
        }
        found = Some(percentage);
    }
    found
}

fn tab_agent_context(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    tab_idx: usize,
) -> Option<String> {
    let terminal = tab_agent_terminal(ws, terminals, tab_idx)?;
    let display_agent = terminal.effective_display_agent()?;
    extract_tab_agent_context(&display_agent).map(str::to_owned)
}

fn tab_layout_width(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    show_agent_context: bool,
    tab_idx: usize,
) -> u16 {
    let context = show_agent_context
        .then(|| tab_agent_context(ws, terminals, tab_idx))
        .flatten();
    tab_width(ws, tab_idx).saturating_add(
        context
            .as_deref()
            .map_or(0, |value| display_width_u16(value).saturating_add(1)),
    )
}

fn tab_layout_widths(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    show_agent_context: bool,
) -> Vec<u16> {
    (0..ws.tabs.len())
        .map(|idx| tab_layout_width(ws, terminals, show_agent_context, idx))
        .collect()
}

fn tab_chrome_label(ws: &crate::workspace::Workspace, tab_idx: usize) -> String {
    tab_chrome_label_with_context(ws, tab_idx, None)
}

fn tab_chrome_label_with_context(
    ws: &crate::workspace::Workspace,
    tab_idx: usize,
    context: Option<&str>,
) -> String {
    let mut name = ws
        .tab_display_name(tab_idx)
        .unwrap_or_else(|| (tab_idx + 1).to_string());
    if let Some(context) = context {
        name.push(' ');
        name.push_str(context);
    }
    if ws.tabs.get(tab_idx).is_some_and(|tab| tab.zoomed) {
        format!("{name} Z")
    } else {
        name
    }
}

fn layout_tab_hit_areas(tab_widths: &[u16], area: Rect, scroll: usize) -> Vec<Rect> {
    let mut rects = vec![Rect::default(); tab_widths.len()];
    if area.width == 0 || area.height == 0 {
        return rects;
    }

    let mut x = area.x;
    let right = area.x + area.width;
    for (idx, rect) in rects.iter_mut().enumerate().skip(scroll) {
        if x >= right {
            break;
        }
        let desired = tab_widths[idx];
        let remaining = right.saturating_sub(x);
        let width = desired.min(remaining).max(1);
        *rect = Rect::new(x, area.y, width, 1);
        x = x.saturating_add(width + 1);
    }
    rects
}

fn centered_tab_scroll(active_tab: usize, tab_widths: &[u16], area: Rect) -> usize {
    let mut best_scroll = active_tab;
    let mut best_distance = u16::MAX;
    let viewport_center = area.x.saturating_mul(2).saturating_add(area.width);

    for scroll in 0..=active_tab {
        let rects = layout_tab_hit_areas(tab_widths, area, scroll);
        let Some(active_rect) = rects.get(active_tab).copied() else {
            continue;
        };
        if active_rect.width == 0 {
            continue;
        }

        let active_center = active_rect
            .x
            .saturating_mul(2)
            .saturating_add(active_rect.width);
        let distance = active_center.abs_diff(viewport_center);
        if distance <= best_distance {
            best_distance = distance;
            best_scroll = scroll;
        }
    }

    best_scroll
}

fn trailing_tab_controls_x(tab_hit_areas: &[Rect], fallback_x: u16) -> u16 {
    tab_hit_areas
        .iter()
        .rev()
        .find(|rect| rect.width > 0)
        .map(|rect| rect.x + rect.width)
        .unwrap_or(fallback_x)
}

fn max_tab_scroll(tab_widths: &[u16], area: Rect) -> usize {
    (0..tab_widths.len())
        .find(|&scroll| {
            layout_tab_hit_areas(tab_widths, area, scroll)
                .last()
                .is_some_and(|rect| rect.width > 0)
        })
        .unwrap_or(0)
}

pub(crate) fn compute_tab_bar_view(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    show_agent_context: bool,
    area: Rect,
    current_scroll: usize,
    follow_active: bool,
    mouse_chrome: bool,
) -> TabBarView {
    if area.width == 0 || area.height == 0 {
        return TabBarView::default();
    }

    let tab_widths = tab_layout_widths(ws, terminals, show_agent_context);
    if !mouse_chrome {
        let max_scroll = max_tab_scroll(&tab_widths, area);
        let scroll = if follow_active {
            centered_tab_scroll(ws.active_tab, &tab_widths, area).min(max_scroll)
        } else {
            current_scroll.min(max_scroll)
        };
        return TabBarView {
            scroll,
            tab_hit_areas: layout_tab_hit_areas(&tab_widths, area, scroll),
            scroll_left_hit_area: Rect::default(),
            scroll_right_hit_area: Rect::default(),
            new_tab_hit_area: Rect::default(),
        };
    }

    let area_right = area.x + area.width;
    let all_tabs_area = Rect::new(
        area.x,
        area.y,
        area.width.saturating_sub(NEW_TAB_WIDTH),
        area.height,
    );
    let all_tabs = layout_tab_hit_areas(&tab_widths, all_tabs_area, 0);
    let overflow = all_tabs.iter().any(|rect| rect.width == 0);
    if !overflow {
        let new_tab_x = trailing_tab_controls_x(&all_tabs, area.x);
        let new_tab_hit_area = Rect::new(
            new_tab_x,
            area.y,
            area_right.saturating_sub(new_tab_x).min(NEW_TAB_WIDTH),
            1,
        );
        return TabBarView {
            scroll: 0,
            tab_hit_areas: all_tabs,
            scroll_left_hit_area: Rect::default(),
            scroll_right_hit_area: Rect::default(),
            new_tab_hit_area,
        };
    }

    let left_hit_area = Rect::new(area.x, area.y, TAB_SCROLL_BUTTON_WIDTH.min(area.width), 1);
    let tab_area_x = left_hit_area.x + left_hit_area.width;
    let reserved_trailing_width = NEW_TAB_WIDTH.saturating_add(TAB_SCROLL_BUTTON_WIDTH);
    let tab_area_right = area_right.saturating_sub(reserved_trailing_width);
    let tab_area = Rect::new(
        tab_area_x,
        area.y,
        tab_area_right.saturating_sub(tab_area_x),
        area.height,
    );

    let max_scroll = max_tab_scroll(&tab_widths, tab_area);
    let scroll = if follow_active {
        centered_tab_scroll(ws.active_tab, &tab_widths, tab_area).min(max_scroll)
    } else {
        current_scroll.min(max_scroll)
    };
    let tab_hit_areas = layout_tab_hit_areas(&tab_widths, tab_area, scroll);
    let trailing_x = trailing_tab_controls_x(&tab_hit_areas, tab_area_x).min(tab_area_right);
    let right_hit_area = Rect::new(
        trailing_x,
        area.y,
        area_right
            .saturating_sub(trailing_x)
            .min(TAB_SCROLL_BUTTON_WIDTH),
        1,
    );
    let new_tab_x = right_hit_area.x + right_hit_area.width;
    let new_tab_hit_area = Rect::new(
        new_tab_x,
        area.y,
        area_right.saturating_sub(new_tab_x).min(NEW_TAB_WIDTH),
        1,
    );

    TabBarView {
        scroll,
        tab_hit_areas,
        scroll_left_hit_area: left_hit_area,
        scroll_right_hit_area: right_hit_area,
        new_tab_hit_area,
    }
}

fn tab_drop_indicator_x(
    app: &AppState,
    ws: &crate::workspace::Workspace,
    insert_idx: usize,
) -> Option<u16> {
    let mut visible_tabs = app
        .view
        .tab_hit_areas
        .iter()
        .enumerate()
        .filter(|(_, rect)| rect.width > 0);
    let first_visible = visible_tabs.clone().next()?;
    let last_visible = visible_tabs.next_back().unwrap_or(first_visible);

    if insert_idx == 0 {
        return Some(if first_visible.0 == 0 {
            first_visible.1.x
        } else {
            app.view.tab_scroll_left_hit_area.x + app.view.tab_scroll_left_hit_area.width
        });
    }

    if let Some((_, rect)) = app
        .view
        .tab_hit_areas
        .iter()
        .enumerate()
        .find(|(idx, rect)| *idx == insert_idx && rect.width > 0)
    {
        return Some(rect.x.saturating_sub(1));
    }

    if insert_idx >= ws.tabs.len() {
        return Some(if last_visible.0 + 1 >= ws.tabs.len() {
            last_visible.1.x + last_visible.1.width
        } else {
            app.view.tab_scroll_right_hit_area.x.saturating_sub(1)
        });
    }

    None
}

pub(super) fn render_tab_bar(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(active_ws_idx) = app.active else {
        return;
    };
    let Some(ws) = app.workspaces.get(active_ws_idx) else {
        return;
    };
    let p = &app.palette;

    frame.render_widget(
        Paragraph::new(" ".repeat(area.width as usize)).style(Style::default().bg(p.panel_bg)),
        area,
    );

    let first_visible_idx = app
        .view
        .tab_hit_areas
        .iter()
        .enumerate()
        .find(|(_, rect)| rect.width > 0)
        .map(|(idx, _)| idx);
    let last_visible_idx = app
        .view
        .tab_hit_areas
        .iter()
        .enumerate()
        .rev()
        .find(|(_, rect)| rect.width > 0)
        .map(|(idx, _)| idx);
    let can_scroll_left = app.view.tab_scroll_left_hit_area.width > 0 && app.tab_scroll > 0;
    let can_scroll_right = app.view.tab_scroll_right_hit_area.width > 0
        && last_visible_idx.is_some_and(|idx| idx + 1 < ws.tabs.len());

    if app.mouse_capture && app.view.tab_scroll_left_hit_area.width > 0 {
        let style = if can_scroll_left {
            Style::default().fg(p.overlay1).bg(p.surface0)
        } else {
            Style::default()
                .fg(p.overlay0)
                .bg(p.surface0)
                .add_modifier(Modifier::DIM)
        };
        frame.render_widget(
            Paragraph::new(" < ").style(style),
            app.view.tab_scroll_left_hit_area,
        );
    }

    if app.mouse_capture && app.view.tab_scroll_right_hit_area.width > 0 {
        let style = if can_scroll_right {
            Style::default().fg(p.overlay1).bg(p.surface0)
        } else {
            Style::default()
                .fg(p.overlay0)
                .bg(p.surface0)
                .add_modifier(Modifier::DIM)
        };
        frame.render_widget(
            Paragraph::new(" > ").style(style),
            app.view.tab_scroll_right_hit_area,
        );
    }

    for (idx, tab) in ws.tabs.iter().enumerate() {
        let Some(rect) = app.view.tab_hit_areas.get(idx).copied() else {
            break;
        };
        if rect.width == 0 {
            continue;
        }
        let active = idx == ws.active_tab;
        let style = if active {
            let base = Style::default().fg(panel_contrast_fg(p)).bg(p.accent);
            if tab.is_auto_named() {
                base
            } else {
                base.add_modifier(Modifier::BOLD)
            }
        } else if tab.is_auto_named() {
            Style::default()
                .fg(p.overlay0)
                .bg(p.surface0)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(p.overlay1).bg(p.surface0)
        };
        let width = rect.width as usize;
        let context = app
            .tab_agent_context
            .then(|| tab_agent_context(ws, &app.terminals, idx))
            .flatten();
        let name = tab_chrome_label_with_context(ws, idx, context.as_deref());
        let text = format!(" {:width$}", name, width = width.saturating_sub(1));
        frame.render_widget(Paragraph::new(text).style(style), rect);
    }

    if let Some(crate::app::state::DragState {
        target:
            crate::app::state::DragTarget::TabReorder {
                ws_idx,
                insert_idx: Some(insert_idx),
                ..
            },
    }) = &app.drag
    {
        if *ws_idx == active_ws_idx {
            if let Some(x) = tab_drop_indicator_x(app, ws, *insert_idx) {
                frame.buffer_mut()[(x.min(area.x + area.width.saturating_sub(1)), area.y)]
                    .set_symbol("│")
                    .set_style(Style::default().fg(p.accent));
            }
        }
    }

    if app.mouse_capture && app.view.new_tab_hit_area.width > 0 {
        frame.render_widget(
            Paragraph::new(" + ").style(Style::default().fg(p.overlay1)),
            app.view.new_tab_hit_area,
        );
    }

    if first_visible_idx.is_some_and(|idx| idx > 0) {
        let x = if app.mouse_capture && app.view.tab_scroll_left_hit_area.width > 0 {
            app.view.tab_scroll_left_hit_area.x + app.view.tab_scroll_left_hit_area.width
        } else {
            area.x
        };
        if x < area.x + area.width {
            frame.buffer_mut()[(x, area.y)]
                .set_symbol("…")
                .set_style(Style::default().fg(p.overlay0));
        }
    }
    if last_visible_idx.is_some_and(|idx| idx + 1 < ws.tabs.len()) {
        let x = if app.mouse_capture && app.view.tab_scroll_right_hit_area.width > 0 {
            app.view.tab_scroll_right_hit_area.x.saturating_sub(1)
        } else {
            area.x + area.width.saturating_sub(1)
        };
        if x >= area.x && x < area.x + area.width {
            frame.buffer_mut()[(x, area.y)]
                .set_symbol("…")
                .set_style(Style::default().fg(p.overlay0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::AppState;
    use crate::detect::{Agent, AgentState};
    use crate::terminal::AgentMetadataReport;
    use crate::workspace::Workspace;
    use ratatui::{backend::TestBackend, Terminal};

    fn buffer_row_text(buffer: &ratatui::buffer::Buffer, area: Rect, row: u16) -> String {
        (area.x..area.x + area.width)
            .map(|x| buffer[(x, row)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn set_agent_with_display_agent(
        app: &mut AppState,
        tab_idx: usize,
        pane_id: crate::layout::PaneId,
        state: AgentState,
        display_agent: Option<&str>,
    ) {
        let terminal_id = app.workspaces[0].tabs[tab_idx].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_metadata(AgentMetadataReport {
            source: "test:tab-agent-context".into(),
            agent_label: None,
            applies_to_source: None,
            title: Some("Metadata title must not replace the visible tab name".into()),
            display_agent: display_agent.map(str::to_owned),
            state_labels: std::collections::HashMap::new(),
            clear_title: false,
            clear_display_agent: false,
            clear_state_labels: false,
            ttl: None,
            seq: None,
        });
        terminal.detected_agent = Some(Agent::Claude);
        terminal.state = state;
    }

    fn workspace_with_tabs(count: usize) -> Workspace {
        assert!(count > 0);
        let mut ws = Workspace::test_new("test");
        for _ in 1..count {
            ws.test_add_tab(None);
        }
        ws
    }

    #[test]
    fn tab_agent_context_extracts_current_producer_format() {
        assert_eq!(
            extract_tab_agent_context("🥷✅ ~42%·4242 $1.23"),
            Some("~42%")
        );
    }

    #[test]
    fn tab_agent_context_parser_is_unicode_whitespace_tolerant_and_preserves_percentage() {
        for (display_agent, expected) in [
            ("working\u{2003}100%·999999\u{2009}trailing", "100%"),
            ("🔧 ready ~7%·0042 cost=unknown", "~7%"),
            ("working 0042%·0", "0042%"),
            ("working 0%·1", "0%"),
            ("working ~0%·4242", "~0%"),
        ] {
            assert_eq!(
                extract_tab_agent_context(display_agent),
                Some(expected),
                "display agent: {display_agent:?}"
            );
        }
    }

    #[test]
    fn tab_agent_context_parser_rejects_malformed_out_of_range_and_ambiguous_labels() {
        for display_agent in [
            "🥷✅ working $1.23",
            "~%·4242",
            "~42%4242",
            "~42%·",
            "~42%·pid",
            "101%·4242",
            "~999%·4242",
            "-1%·4242",
            "42%·-1",
            "x42%·4242",
            "42%·4242x",
            "４２%·4242",
            "🥷 ~42%·4242 moved 43%·4343 $1.23",
        ] {
            assert_eq!(
                extract_tab_agent_context(display_agent),
                None,
                "display agent: {display_agent:?}"
            );
        }
    }

    #[test]
    fn tab_agent_context_renders_estimate_after_visible_name_before_zoom() {
        let mut app = AppState::test_new();
        let mut ws = Workspace::test_new("test");
        ws.tabs[0].set_custom_name("Visible Tab".into());
        ws.tabs[0].zoomed = true;
        let pane_id = ws.tabs[0].root_pane;
        app.workspaces = vec![ws];
        app.ensure_test_terminals();
        set_agent_with_display_agent(
            &mut app,
            0,
            pane_id,
            AgentState::Working,
            Some("🥷✅ ~2%·4242 $1.23"),
        );
        app.active = Some(0);
        app.selected = 0;
        app.mouse_capture = false;
        assert!(!app.tab_agent_context);

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        crate::ui::compute_view(&mut app, Rect::new(0, 0, 100, 20));
        let baseline_rect = app.view.tab_hit_areas[0];
        terminal
            .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
            .unwrap();
        let baseline_row = buffer_row_text(
            terminal.backend().buffer(),
            app.view.tab_bar_rect,
            baseline_rect.y,
        );
        assert_eq!(baseline_row, " Visible Tab Z");

        app.tab_agent_context = true;
        crate::ui::compute_view(&mut app, Rect::new(0, 0, 100, 20));
        let enabled_rect = app.view.tab_hit_areas[0];
        terminal
            .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
            .unwrap();
        let enabled_row = buffer_row_text(
            terminal.backend().buffer(),
            app.view.tab_bar_rect,
            enabled_rect.y,
        );

        assert_eq!(enabled_row, " Visible Tab ~2% Z");
        assert!(!enabled_row.contains("4242"));
        assert!(!enabled_row.contains("$1.23"));
        assert!(!enabled_row.contains("Metadata title"));
        assert_eq!(
            enabled_rect.width,
            tab_width(&app.workspaces[0], 0) + display_width_u16(" ~2%")
        );
    }

    #[test]
    fn missing_invalid_and_metadata_only_context_preserve_disabled_bytes_and_width() {
        for (display_agent, detected_agent) in [
            (None, true),
            (Some("working ~%·4242"), true),
            (Some("working 101%·4242"), true),
            (Some("working 42%·4242 then 43%·4343"), true),
            (Some("working 42%·4242"), false),
        ] {
            let mut app = AppState::test_new();
            let mut ws = Workspace::test_new("test");
            ws.tabs[0].set_custom_name("baseline".into());
            let pane_id = ws.tabs[0].root_pane;
            app.workspaces = vec![ws];
            app.ensure_test_terminals();
            set_agent_with_display_agent(&mut app, 0, pane_id, AgentState::Working, display_agent);
            if !detected_agent {
                let terminal_id = app.workspaces[0].tabs[0].panes[&pane_id]
                    .attached_terminal_id
                    .clone();
                app.terminals.get_mut(&terminal_id).unwrap().detected_agent = None;
            }
            app.active = Some(0);
            app.selected = 0;
            app.mouse_capture = false;

            let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
            app.tab_agent_context = false;
            crate::ui::compute_view(&mut app, Rect::new(0, 0, 80, 20));
            let baseline_rect = app.view.tab_hit_areas[0];
            terminal
                .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
                .unwrap();
            let baseline_row = buffer_row_text(
                terminal.backend().buffer(),
                app.view.tab_bar_rect,
                baseline_rect.y,
            );

            app.tab_agent_context = true;
            crate::ui::compute_view(&mut app, Rect::new(0, 0, 80, 20));
            let enabled_rect = app.view.tab_hit_areas[0];
            terminal
                .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
                .unwrap();
            let enabled_row = buffer_row_text(
                terminal.backend().buffer(),
                app.view.tab_bar_rect,
                enabled_rect.y,
            );

            assert_eq!(
                enabled_rect, baseline_rect,
                "display agent: {display_agent:?}"
            );
            assert_eq!(
                enabled_row, baseline_row,
                "display agent: {display_agent:?}"
            );
        }
    }

    #[test]
    fn tab_agent_context_does_not_fall_back_from_highest_attention_agent() {
        let mut app = AppState::test_new();
        let mut ws = Workspace::test_new("test");
        ws.tabs[0].set_custom_name("pairing".into());
        let working_pane = ws.tabs[0].root_pane;
        let blocked_pane = ws.test_split(ratatui::layout::Direction::Horizontal);
        app.workspaces = vec![ws];
        app.ensure_test_terminals();
        set_agent_with_display_agent(
            &mut app,
            0,
            working_pane,
            AgentState::Working,
            Some("working ~2%·4242"),
        );
        set_agent_with_display_agent(&mut app, 0, blocked_pane, AgentState::Blocked, None);
        app.active = Some(0);
        app.selected = 0;
        app.mouse_capture = false;
        app.tab_agent_context = true;

        crate::ui::compute_view(&mut app, Rect::new(0, 0, 80, 20));
        let rect = app.view.tab_hit_areas[0];
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
            .unwrap();
        let row = buffer_row_text(terminal.backend().buffer(), app.view.tab_bar_rect, rect.y);

        assert_eq!(rect.width, tab_width(&app.workspaces[0], 0));
        assert!(!row.contains("~2%"), "tab row: {row:?}");
    }

    #[test]
    fn equal_attention_tab_agents_use_last_pane_in_layout_order() {
        let mut app = AppState::test_new();
        let mut ws = Workspace::test_new("test");
        ws.tabs[0].set_custom_name("tie".into());
        let first_pane = ws.tabs[0].root_pane;
        let last_pane = ws.test_split(ratatui::layout::Direction::Horizontal);
        assert_eq!(ws.tabs[0].layout.pane_ids(), vec![first_pane, last_pane]);
        app.workspaces = vec![ws];
        app.ensure_test_terminals();
        set_agent_with_display_agent(
            &mut app,
            0,
            first_pane,
            AgentState::Working,
            Some("working 11%·1111"),
        );
        set_agent_with_display_agent(
            &mut app,
            0,
            last_pane,
            AgentState::Working,
            Some("working 77%·7777"),
        );
        app.active = Some(0);
        app.selected = 0;
        app.mouse_capture = false;
        app.tab_agent_context = true;

        crate::ui::compute_view(&mut app, Rect::new(0, 0, 80, 20));
        let rect = app.view.tab_hit_areas[0];
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
            .unwrap();
        let row = buffer_row_text(terminal.backend().buffer(), app.view.tab_bar_rect, rect.y);

        assert!(row.contains("tie 77%"), "tab row: {row:?}");
        assert!(!row.contains("11%"), "tab row: {row:?}");
    }

    #[test]
    fn tab_agent_context_updates_single_row_centered_scroll_and_suffix_hit_area() {
        let mut app = AppState::test_new();
        let mut ws = workspace_with_tabs(6);
        ws.active_tab = 4;
        app.workspaces = vec![ws];
        app.ensure_test_terminals();
        let context_tab = 4;
        let pane_id = app.workspaces[0].tabs[context_tab].root_pane;
        set_agent_with_display_agent(
            &mut app,
            context_tab,
            pane_id,
            AgentState::Working,
            Some("working 42%·4242"),
        );
        let area = Rect::new(0, 0, 18, 1);

        let baseline = compute_tab_bar_view(
            &app.workspaces[0],
            &app.terminals,
            false,
            area,
            0,
            true,
            false,
        );
        let enabled = compute_tab_bar_view(
            &app.workspaces[0],
            &app.terminals,
            true,
            area,
            0,
            true,
            false,
        );

        assert_eq!(baseline.scroll, 3);
        assert_eq!(enabled.scroll, 4);
        let context_rect = enabled.tab_hit_areas[context_tab];
        assert_eq!(
            context_rect.width,
            tab_width(&app.workspaces[0], context_tab) + display_width_u16(" 42%")
        );
        let suffix_point = (context_rect.right() - 1, context_rect.y);
        let hit = enabled.tab_hit_areas.iter().position(|rect| {
            rect.width > 0
                && suffix_point.0 >= rect.x
                && suffix_point.0 < rect.right()
                && suffix_point.1 >= rect.y
                && suffix_point.1 < rect.bottom()
        });
        assert_eq!(hit, Some(context_tab));
    }

    #[test]
    fn tab_agent_context_drives_overflow_controls_new_tab_and_drop_geometry() {
        let mut app = AppState::test_new();
        app.workspaces = vec![workspace_with_tabs(3)];
        app.ensure_test_terminals();
        let pane_id = app.workspaces[0].tabs[0].root_pane;
        set_agent_with_display_agent(
            &mut app,
            0,
            pane_id,
            AgentState::Working,
            Some("working 42%·4242"),
        );
        let area = Rect::new(0, 0, 25, 1);

        let baseline = compute_tab_bar_view(
            &app.workspaces[0],
            &app.terminals,
            false,
            area,
            0,
            true,
            true,
        );
        assert_eq!(baseline.scroll_left_hit_area, Rect::default());
        assert_eq!(baseline.scroll_right_hit_area, Rect::default());
        assert_eq!(baseline.new_tab_hit_area.x, 22);

        let enabled = compute_tab_bar_view(
            &app.workspaces[0],
            &app.terminals,
            true,
            area,
            0,
            true,
            true,
        );
        assert!(enabled.scroll_left_hit_area.width > 0);
        assert!(enabled.scroll_right_hit_area.width > 0);
        assert!(enabled.new_tab_hit_area.width > 0);
        assert_eq!(
            enabled.new_tab_hit_area.x,
            enabled.scroll_right_hit_area.right()
        );
        assert_eq!(
            enabled.tab_hit_areas[0].width,
            tab_width(&app.workspaces[0], 0) + display_width_u16(" 42%")
        );

        app.view.tab_hit_areas = enabled.tab_hit_areas.clone();
        app.view.tab_scroll_left_hit_area = enabled.scroll_left_hit_area;
        app.view.tab_scroll_right_hit_area = enabled.scroll_right_hit_area;
        app.view.new_tab_hit_area = enabled.new_tab_hit_area;
        let drop_x = tab_drop_indicator_x(&app, &app.workspaces[0], 1).unwrap();
        assert_eq!(drop_x, enabled.tab_hit_areas[0].right());
    }

    #[test]
    fn tab_bar_marks_zoomed_tabs_without_renaming_them() {
        let mut app = AppState::test_new();
        let mut ws = Workspace::test_new("test");
        ws.tabs[0].zoomed = true;
        let custom_tab = ws.test_add_tab(Some("test"));
        ws.tabs[custom_tab].zoomed = true;

        app.workspaces = vec![ws];
        app.active = Some(0);
        app.view.tab_bar_rect = Rect::new(0, 0, 30, 1);
        let view = compute_tab_bar_view(
            &app.workspaces[0],
            &app.terminals,
            app.tab_agent_context,
            app.view.tab_bar_rect,
            0,
            true,
            false,
        );
        app.view.tab_hit_areas = view.tab_hit_areas;

        let backend = TestBackend::new(30, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
            .unwrap();

        let row = buffer_row_text(terminal.backend().buffer(), app.view.tab_bar_rect, 0);
        assert!(row.contains(" 1 Z"), "tab row: {row:?}");
        assert!(row.contains(" test Z"), "tab row: {row:?}");
        assert_eq!(app.workspaces[0].tab_display_name(0).as_deref(), Some("1"));
        assert_eq!(
            app.workspaces[0].tab_display_name(custom_tab).as_deref(),
            Some("test")
        );
    }

    #[test]
    fn active_auto_named_tab_keeps_readable_weight() {
        let mut app = AppState::test_new();
        let ws = Workspace::test_new("test");

        app.workspaces = vec![ws];
        app.active = Some(0);
        app.view.tab_bar_rect = Rect::new(0, 0, 30, 1);
        let view = compute_tab_bar_view(
            &app.workspaces[0],
            &app.terminals,
            app.tab_agent_context,
            app.view.tab_bar_rect,
            0,
            true,
            false,
        );
        app.view.tab_hit_areas = view.tab_hit_areas;

        let backend = TestBackend::new(30, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
            .unwrap();

        let tab_rect = app.view.tab_hit_areas[0];
        let style = terminal.backend().buffer()[(tab_rect.x + 1, tab_rect.y)].style();

        assert_eq!(style.bg, Some(app.palette.accent));
        assert!(!style.add_modifier.contains(Modifier::DIM));
        assert!(!style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn zoom_marker_counts_toward_tab_width() {
        let mut ws = Workspace::test_new("test");
        ws.tabs[0].set_custom_name("abcdefgh".into());
        ws.tabs[0].zoomed = true;

        assert_eq!(tab_width(&ws, 0), 14);
    }

    #[test]
    fn tab_width_uses_display_width_for_cjk_labels() {
        let mut ws = Workspace::test_new("test");
        ws.tabs[0].set_custom_name("提交 herdr 的反馈".into());

        assert_eq!(
            tab_width(&ws, 0),
            display_width_u16("提交 herdr 的反馈") + 4
        );
    }

    #[test]
    fn tab_bar_renders_trailing_cjk_character() {
        let mut app = AppState::test_new();
        let mut ws = Workspace::test_new("test");
        ws.tabs[0].set_custom_name("提交 herdr 的反馈".into());

        app.active = Some(0);
        app.workspaces = vec![ws];
        app.view.tab_bar_rect = Rect::new(0, 0, 30, 1);
        let view = compute_tab_bar_view(
            &app.workspaces[0],
            &app.terminals,
            app.tab_agent_context,
            app.view.tab_bar_rect,
            0,
            true,
            false,
        );
        app.view.tab_hit_areas = view.tab_hit_areas;

        let backend = TestBackend::new(30, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
            .unwrap();

        let row = buffer_row_text(terminal.backend().buffer(), app.view.tab_bar_rect, 0);
        assert!(row.contains('馈'), "tab row: {row:?}");
    }
}
