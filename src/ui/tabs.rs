use std::collections::HashMap;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::sidebar::workspace_attention_priority;
use super::status::state_dot;
use super::text::display_width_u16;
use super::widgets::panel_contrast_fg;
use crate::app::AppState;
use crate::detect::AgentState;
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

fn tab_chrome_label(ws: &crate::workspace::Workspace, tab_idx: usize) -> String {
    let name = ws
        .tab_display_name(tab_idx)
        .unwrap_or_else(|| (tab_idx + 1).to_string());
    if ws.tabs.get(tab_idx).is_some_and(|tab| tab.zoomed) {
        format!("{name} Z")
    } else {
        name
    }
}

fn tab_agent_state(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    tab_idx: usize,
) -> Option<(AgentState, bool)> {
    let tab = ws.tabs.get(tab_idx)?;
    tab.layout
        .pane_ids()
        .iter()
        .filter_map(|pane_id| {
            let pane = tab.panes.get(pane_id)?;
            let terminal = terminals.get(&pane.attached_terminal_id)?;
            terminal.detected_agent?;
            Some((terminal.state, pane.seen))
        })
        .max_by_key(|(state, seen)| workspace_attention_priority(*state, *seen))
}

fn tab_layout_width(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    tab_agent_status: bool,
    tab_idx: usize,
) -> u16 {
    tab_width(ws, tab_idx).saturating_add(
        u16::from(tab_agent_status && tab_agent_state(ws, terminals, tab_idx).is_some()) * 2,
    )
}

fn layout_tab_hit_areas(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    tab_agent_status: bool,
    area: Rect,
    scroll: usize,
) -> Vec<Rect> {
    let mut rects = vec![Rect::default(); ws.tabs.len()];
    if area.width == 0 || area.height == 0 {
        return rects;
    }

    let mut x = area.x;
    let right = area.x + area.width;
    for (idx, rect) in rects.iter_mut().enumerate().skip(scroll) {
        if x >= right {
            break;
        }
        let desired = tab_layout_width(ws, terminals, tab_agent_status, idx);
        let remaining = right.saturating_sub(x);
        let width = desired.min(remaining).max(1);
        *rect = Rect::new(x, area.y, width, 1);
        x = x.saturating_add(width + 1);
    }
    rects
}

/// Flow `item_widths` left-to-right with a 1-column gap between items, wrapping
/// to a new row when the next item would overflow `width`. Returns
/// `(x, y, clamped_width)` per item, where `y` is the 0-based row index and `x`
/// is relative to the left edge. The first item on a row never wraps; items
/// wider than `width` are clamped to it. `width` must be non-zero.
fn wrap_flow(item_widths: impl Iterator<Item = u16>, width: u16) -> Vec<(u16, u16, u16)> {
    let mut out = Vec::new();
    let mut x = 0u16;
    let mut y = 0u16;
    for w in item_widths {
        let w = w.min(width).max(1);
        if x > 0 && x + w > width {
            y = y.saturating_add(1);
            x = 0;
        }
        out.push((x, y, w));
        x = x.saturating_add(w + 1);
    }
    out
}

/// Rows the wrapped tab bar needs to show every tab at `width`. Includes a
/// trailing new-tab (`+`) control when `with_new_tab` is set so the reserved
/// height matches what [`layout_tab_hit_areas_wrapped`] lays out.
pub(super) fn tab_bar_wrapped_rows(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    tab_agent_status: bool,
    width: u16,
    with_new_tab: bool,
) -> u16 {
    if width == 0 || ws.tabs.is_empty() {
        return 1;
    }
    let widths = (0..ws.tabs.len())
        .map(|idx| tab_layout_width(ws, terminals, tab_agent_status, idx))
        .chain(with_new_tab.then_some(NEW_TAB_WIDTH));
    wrap_flow(widths, width)
        .last()
        .map(|(_, y, _)| y.saturating_add(1))
        .unwrap_or(1)
}

/// Lay out every tab across multiple rows within `area`, wrapping instead of
/// scrolling. Returns the per-tab rects (each height 1, carrying its own `y`)
/// and the trailing new-tab (`+`) rect.
///
/// When the wrapped tabs need more rows than `area.height` allows (only
/// possible when the bar height is clamped, e.g. very many tabs in a short
/// terminal), the visible rows are scrolled so the active tab's row stays in
/// view; tabs outside that window get a zero-width rect (still reachable by
/// selecting them, which re-centers the window on the new active row).
fn layout_tab_hit_areas_wrapped(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    tab_agent_status: bool,
    area: Rect,
    with_new_tab: bool,
) -> (Vec<Rect>, Rect) {
    let mut rects = vec![Rect::default(); ws.tabs.len()];
    let mut new_tab = Rect::default();
    if area.width == 0 || area.height == 0 {
        return (rects, new_tab);
    }
    let widths = (0..ws.tabs.len())
        .map(|idx| tab_layout_width(ws, terminals, tab_agent_status, idx))
        .chain(with_new_tab.then_some(NEW_TAB_WIDTH));
    let flow = wrap_flow(widths, area.width);
    let total_rows = flow.last().map(|(_, y, _)| y + 1).unwrap_or(0);

    // Common case (unclamped): show every row from the top. Only when the bar
    // is too short to fit all rows do we scroll to keep the active tab visible.
    let row_offset = if total_rows > area.height {
        let active_row = flow.get(ws.active_tab).map(|(_, y, _)| *y).unwrap_or(0);
        active_row
            .saturating_sub(area.height / 2)
            .min(total_rows.saturating_sub(area.height))
    } else {
        0
    };

    for (item, (x, y, w)) in flow.into_iter().enumerate() {
        let Some(rendered_y) = y.checked_sub(row_offset) else {
            continue; // row scrolled off the top of the window
        };
        if rendered_y >= area.height {
            continue; // row scrolled off the bottom of the window
        }
        let rect = Rect::new(area.x + x, area.y + rendered_y, w, 1);
        match rects.get_mut(item) {
            Some(slot) => *slot = rect,
            None => new_tab = rect,
        }
    }
    (rects, new_tab)
}

/// Wrapped variant of [`compute_tab_bar_view`]: every tab flows across multiple
/// rows within `area` with no horizontal scroll and no `<`/`>` buttons. The
/// trailing new-tab (`+`) control is included when `mouse_chrome` is set.
pub(crate) fn compute_wrapped_tab_bar_view(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    tab_agent_status: bool,
    area: Rect,
    mouse_chrome: bool,
) -> TabBarView {
    if area.width == 0 || area.height == 0 {
        return TabBarView::default();
    }
    let (tab_hit_areas, new_tab_hit_area) =
        layout_tab_hit_areas_wrapped(ws, terminals, tab_agent_status, area, mouse_chrome);
    TabBarView {
        scroll: 0,
        tab_hit_areas,
        scroll_left_hit_area: Rect::default(),
        scroll_right_hit_area: Rect::default(),
        new_tab_hit_area,
    }
}

fn centered_tab_scroll(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    tab_agent_status: bool,
    area: Rect,
) -> usize {
    let mut best_scroll = ws.active_tab;
    let mut best_distance = u16::MAX;
    let viewport_center = area.x.saturating_mul(2).saturating_add(area.width);

    for scroll in 0..=ws.active_tab {
        let rects = layout_tab_hit_areas(ws, terminals, tab_agent_status, area, scroll);
        let Some(active_rect) = rects.get(ws.active_tab).copied() else {
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

fn max_tab_scroll(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    tab_agent_status: bool,
    area: Rect,
) -> usize {
    (0..ws.tabs.len())
        .find(|&scroll| {
            layout_tab_hit_areas(ws, terminals, tab_agent_status, area, scroll)
                .last()
                .is_some_and(|rect| rect.width > 0)
        })
        .unwrap_or(0)
}

pub(crate) fn compute_tab_bar_view(
    ws: &crate::workspace::Workspace,
    terminals: &HashMap<TerminalId, TerminalState>,
    tab_agent_status: bool,
    area: Rect,
    current_scroll: usize,
    follow_active: bool,
    mouse_chrome: bool,
) -> TabBarView {
    if area.width == 0 || area.height == 0 {
        return TabBarView::default();
    }

    if !mouse_chrome {
        let max_scroll = max_tab_scroll(ws, terminals, tab_agent_status, area);
        let scroll = if follow_active {
            centered_tab_scroll(ws, terminals, tab_agent_status, area).min(max_scroll)
        } else {
            current_scroll.min(max_scroll)
        };
        return TabBarView {
            scroll,
            tab_hit_areas: layout_tab_hit_areas(ws, terminals, tab_agent_status, area, scroll),
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
    let all_tabs = layout_tab_hit_areas(ws, terminals, tab_agent_status, all_tabs_area, 0);
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

    let max_scroll = max_tab_scroll(ws, terminals, tab_agent_status, tab_area);
    let scroll = if follow_active {
        centered_tab_scroll(ws, terminals, tab_agent_status, tab_area).min(max_scroll)
    } else {
        current_scroll.min(max_scroll)
    };
    let tab_hit_areas = layout_tab_hit_areas(ws, terminals, tab_agent_status, tab_area, scroll);
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

    // Fill every row of the bar (height > 1 in wrap mode) with the panel bg.
    let bg = vec![" ".repeat(area.width as usize); area.height as usize].join("\n");
    frame.render_widget(
        Paragraph::new(bg).style(Style::default().bg(p.panel_bg)),
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
        let name = tab_chrome_label(ws, idx);
        if let Some((state, seen)) = app
            .tab_agent_status
            .then(|| tab_agent_state(ws, &app.terminals, idx))
            .flatten()
        {
            let (dot, dot_style) = state_dot(state, seen, p);
            let label = format!(" {:width$}", name, width = width.saturating_sub(3));
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(dot, dot_style),
                    Span::raw(label),
                ]))
                .style(style),
                rect,
            );
        } else {
            let text = format!(" {:width$}", name, width = width.saturating_sub(1));
            frame.render_widget(Paragraph::new(text).style(style), rect);
        }
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

    if !app.tab_bar_wrap && first_visible_idx.is_some_and(|idx| idx > 0) {
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
    if !app.tab_bar_wrap && last_visible_idx.is_some_and(|idx| idx + 1 < ws.tabs.len()) {
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
    use crate::workspace::Workspace;
    use ratatui::{backend::TestBackend, layout::Direction, Terminal};

    fn buffer_row_text(buffer: &ratatui::buffer::Buffer, area: Rect, row: u16) -> String {
        (area.x..area.x + area.width)
            .map(|x| buffer[(x, row)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
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
            app.tab_agent_status,
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
            app.tab_agent_status,
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
            app.tab_agent_status,
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

    fn workspace_with_tabs(count: usize) -> Workspace {
        let mut ws = Workspace::test_new("test");
        for _ in 1..count {
            ws.test_add_tab(None);
        }
        ws
    }

    #[test]
    fn wrapped_layout_flows_tabs_onto_multiple_rows() {
        // Auto-named tabs are MIN_TAB_WIDTH (8) wide + 1 gap, so 3 fit per row at
        // width 30 and the 4th wraps to the next row.
        let ws = workspace_with_tabs(7);
        let area = Rect::new(0, 0, 30, 5);
        let (rects, _new_tab) =
            layout_tab_hit_areas_wrapped(&ws, &HashMap::new(), false, area, false);

        assert_eq!(rects.len(), 7);
        assert!(
            rects.iter().all(|r| r.width > 0),
            "every tab is visible when wrapped: {rects:?}"
        );
        let rows: std::collections::BTreeSet<u16> = rects.iter().map(|r| r.y).collect();
        assert_eq!(rows.len(), 3, "tabs span three rows: {rows:?}");
        assert!(
            rects.iter().all(|r| r.x + r.width <= area.x + area.width),
            "no tab overflows the content width: {rects:?}"
        );
        assert_eq!((rects[0].y, rects[1].y, rects[2].y), (0, 0, 0));
        assert!(rects[0].x < rects[1].x && rects[1].x < rects[2].x);
        assert_eq!(rects[3].y, 1, "the fourth tab wraps to the next row");
    }

    #[test]
    fn wrapped_rows_matches_layout_row_count() {
        // The reserved height must equal the rows the layout actually uses,
        // otherwise the terminal area below would not shrink by exactly the
        // wrapped rows. Verify across widths and with/without the `+` control.
        let ws = workspace_with_tabs(10);
        for &width in &[20u16, 30, 45, 80] {
            for with_new_tab in [false, true] {
                let rows = tab_bar_wrapped_rows(&ws, &HashMap::new(), false, width, with_new_tab);
                let area = Rect::new(0, 0, width, u16::from(u8::MAX));
                let (rects, new_tab) =
                    layout_tab_hit_areas_wrapped(&ws, &HashMap::new(), false, area, with_new_tab);
                let mut ys: std::collections::BTreeSet<u16> =
                    rects.iter().filter(|r| r.width > 0).map(|r| r.y).collect();
                if with_new_tab {
                    ys.insert(new_tab.y);
                }
                let used_rows = ys.iter().max().map(|y| y + 1).unwrap_or(1);
                assert_eq!(rows, used_rows, "width={width} with_new_tab={with_new_tab}");
            }
        }
    }

    #[test]
    fn wrapped_layout_accounts_for_agent_status_width() {
        let mut app = AppState::test_new();
        app.workspaces = vec![workspace_with_tabs(2)];
        app.ensure_test_terminals();
        let pane_id = app.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let pane_terminal = app.terminals.get_mut(&terminal_id).unwrap();
        pane_terminal.detected_agent = Some(Agent::Claude);
        pane_terminal.state = AgentState::Blocked;

        let area = Rect::new(0, 0, 18, 2);
        let (without_status, _) =
            layout_tab_hit_areas_wrapped(&app.workspaces[0], &app.terminals, false, area, false);
        let (with_status, _) =
            layout_tab_hit_areas_wrapped(&app.workspaces[0], &app.terminals, true, area, false);

        assert_eq!(without_status[0].y, without_status[1].y);
        assert!(with_status[1].y > with_status[0].y);
        assert_eq!(
            tab_bar_wrapped_rows(&app.workspaces[0], &app.terminals, true, area.width, false),
            2
        );
    }

    #[test]
    fn wrap_mode_renders_every_tab_without_scroll_chrome() {
        let mut app = AppState::test_new();
        app.tab_bar_wrap = true;
        app.mouse_capture = true;
        app.workspaces = vec![workspace_with_tabs(7)];
        app.active = Some(0);

        let rows = tab_bar_wrapped_rows(
            &app.workspaces[0],
            &app.terminals,
            app.tab_agent_status,
            30,
            true,
        );
        assert!(rows > 1, "seven tabs should need multiple rows, got {rows}");
        app.view.tab_bar_rect = Rect::new(0, 0, 30, rows);
        let view = compute_wrapped_tab_bar_view(
            &app.workspaces[0],
            &app.terminals,
            app.tab_agent_status,
            app.view.tab_bar_rect,
            true,
        );
        app.view.tab_hit_areas = view.tab_hit_areas;
        app.view.new_tab_hit_area = view.new_tab_hit_area;
        // Wrap mode never uses scroll buttons.
        assert_eq!(view.scroll_left_hit_area.width, 0);
        assert_eq!(view.scroll_right_hit_area.width, 0);

        let backend = TestBackend::new(30, rows);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut all_rows = String::new();
        for row in 0..rows {
            all_rows.push_str(&buffer_row_text(buffer, app.view.tab_bar_rect, row));
            all_rows.push('\n');
        }

        for label in 1..=7 {
            assert!(
                all_rows.contains(&format!(" {label}")),
                "tab {label} rendered somewhere: {all_rows:?}"
            );
        }
        assert!(all_rows.contains('+'), "new-tab button shown: {all_rows:?}");
        assert!(!all_rows.contains('…'), "no overflow markers: {all_rows:?}");
        assert!(!all_rows.contains('<'), "no scroll buttons: {all_rows:?}");
        assert!(!all_rows.contains('>'), "no scroll buttons: {all_rows:?}");
    }

    #[test]
    fn wrap_mode_hit_testing_selects_tabs_on_lower_rows() {
        let ws = workspace_with_tabs(7);
        let area = Rect::new(0, 0, 30, 5);
        let (rects, _) = layout_tab_hit_areas_wrapped(&ws, &HashMap::new(), false, area, true);

        // A tab that wrapped onto a lower row carries the correct y. Mirror the
        // row-aware point-in-rect test the mouse layer (`AppState::tab_at`) uses
        // and confirm a click at that tab's center resolves uniquely to it.
        let (idx, rect) = rects
            .iter()
            .enumerate()
            .find(|(_, r)| r.y > 0 && r.width > 0)
            .map(|(idx, r)| (idx, *r))
            .expect("a tab wraps onto a lower row");
        let (col, row) = (rect.x + rect.width / 2, rect.y);
        let hits: Vec<usize> = rects
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.width > 0
                    && row >= r.y
                    && row < r.y + r.height
                    && col >= r.x
                    && col < r.x + r.width
            })
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            hits,
            vec![idx],
            "click on a lower-row tab hits only that tab"
        );
    }

    #[test]
    fn wrap_layout_keeps_active_tab_visible_when_rows_are_clamped() {
        // 40 tabs at width 30 wrap to ~14 rows, but the bar height is clamped to
        // 3 rows here. The active tab (deep in the list) must stay visible and
        // reachable, and nothing should panic or fall outside the area.
        let mut ws = workspace_with_tabs(40);
        ws.active_tab = 35;
        let area = Rect::new(0, 0, 30, 3);

        let (rects, _new_tab) =
            layout_tab_hit_areas_wrapped(&ws, &HashMap::new(), false, area, true);
        assert_eq!(rects.len(), 40);

        let active = rects[ws.active_tab];
        assert!(active.width > 0, "active tab stays visible under the clamp");
        assert!(
            active.y >= area.y && active.y < area.y + area.height,
            "active tab is within the visible rows: {active:?}"
        );
        // Fewer tabs are shown than exist, but every rendered rect fits the area.
        let shown = rects.iter().filter(|r| r.width > 0).count();
        assert!(shown < 40, "clamp hides some tabs: {shown} shown");
        assert!(shown > 0);
        for r in rects.iter().filter(|r| r.width > 0) {
            assert!(
                r.y >= area.y && r.y < area.y + area.height,
                "row in area: {r:?}"
            );
            assert!(
                r.x + r.width <= area.x + area.width,
                "column in area: {r:?}"
            );
        }
    }

    #[test]
    fn tab_agent_status_renders_blocked_dot_only_when_enabled() {
        let mut app = AppState::test_new();
        let ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        app.workspaces = vec![ws];
        app.ensure_test_terminals();
        let terminal_id = app.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let pane_terminal = app.terminals.get_mut(&terminal_id).unwrap();
        pane_terminal.detected_agent = Some(Agent::Claude);
        pane_terminal.state = AgentState::Blocked;
        app.active = Some(0);
        app.selected = 0;

        app.tab_agent_status = true;
        crate::ui::compute_view(&mut app, Rect::new(0, 0, 80, 20));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
            .unwrap();
        let enabled_tab_rect = app.view.tab_hit_areas[0];
        let dot_cell = &terminal.backend().buffer()[(enabled_tab_rect.x + 1, enabled_tab_rect.y)];
        assert_eq!(dot_cell.symbol(), "●");
        assert_eq!(dot_cell.style().fg, Some(app.palette.red));
        assert_eq!(dot_cell.style().bg, Some(app.palette.accent));
        assert_eq!(enabled_tab_rect.width, tab_width(&app.workspaces[0], 0) + 2);
        assert_eq!(
            terminal.backend().buffer()[(enabled_tab_rect.x + 3, enabled_tab_rect.y)].symbol(),
            "1"
        );

        app.tab_agent_status = false;
        crate::ui::compute_view(&mut app, Rect::new(0, 0, 80, 20));
        terminal
            .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
            .unwrap();
        let disabled_tab_rect = app.view.tab_hit_areas[0];
        let leading_label_cell =
            &terminal.backend().buffer()[(disabled_tab_rect.x + 1, disabled_tab_rect.y)];
        assert_ne!(leading_label_cell.symbol(), "●");
        assert_eq!(disabled_tab_rect.width, tab_width(&app.workspaces[0], 0));
    }

    #[test]
    fn tab_agent_status_uses_highest_attention_pane() {
        let mut app = AppState::test_new();
        let mut ws = Workspace::test_new("test");
        let working_pane = ws.tabs[0].root_pane;
        let blocked_pane = ws.test_split(Direction::Horizontal);
        app.workspaces = vec![ws];
        app.ensure_test_terminals();

        for (pane_id, state) in [
            (working_pane, AgentState::Working),
            (blocked_pane, AgentState::Blocked),
        ] {
            let terminal_id = app.workspaces[0].tabs[0].panes[&pane_id]
                .attached_terminal_id
                .clone();
            let pane_terminal = app.terminals.get_mut(&terminal_id).unwrap();
            pane_terminal.detected_agent = Some(Agent::Claude);
            pane_terminal.state = state;
        }
        app.active = Some(0);
        app.selected = 0;
        app.tab_agent_status = true;

        crate::ui::compute_view(&mut app, Rect::new(0, 0, 80, 20));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| render_tab_bar(&app, frame, app.view.tab_bar_rect))
            .unwrap();

        let dot_cell = &terminal.backend().buffer()
            [(app.view.tab_hit_areas[0].x + 1, app.view.tab_hit_areas[0].y)];
        assert_eq!(dot_cell.symbol(), "●");
        assert_eq!(dot_cell.style().fg, Some(app.palette.red));
    }
}
