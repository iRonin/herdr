use super::*;

const TAB_SCROLL_BUTTON_WIDTH: u16 = 3;
const MIN_TAB_STRIP_WIDTH: u16 =
    MIN_TAB_WIDTH + NEW_TAB_WIDTH + TAB_SCROLL_BUTTON_WIDTH.saturating_mul(2);

pub(crate) fn render_tab_bar(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    tab_scroll: &mut usize,
    reveal_focused_tab: &mut bool,
    tab_drag_insert_index: Option<usize>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    buffer.set_style(area, Style::default().bg(palette.panel_bg));
    let tabs = focused_tabs(snapshot);
    if config.tab_bar_wrap {
        *tab_scroll = 0;
        *reveal_focused_tab = false;
        render_wrapped_tabs(buffer, area, snapshot, config, &tabs, hits);
        render_tab_drop_indicator(buffer, area, palette, &tabs, tab_drag_insert_index, hits);
        render_tab_bar_status(buffer, area, snapshot, palette);
        return;
    }
    let desired_widths = tab_desired_widths(&tabs);
    let content = tab_bar_content_area(snapshot, area);
    let mouse_chrome = config.mouse_capture;
    let new_tab_width = if mouse_chrome { NEW_TAB_WIDTH } else { 0 };
    let desired_total = desired_widths
        .iter()
        .copied()
        .fold(0_u16, u16::saturating_add)
        .saturating_add(tabs.len().saturating_sub(1).min(u16::MAX as usize) as u16)
        .saturating_add(new_tab_width);
    let overflow =
        desired_total > content.width && (!mouse_chrome || content.width >= MIN_TAB_STRIP_WIDTH);
    let available = if overflow && mouse_chrome {
        content
            .width
            .saturating_sub(NEW_TAB_WIDTH)
            .saturating_sub(TAB_SCROLL_BUTTON_WIDTH.saturating_mul(2))
    } else {
        content.width.saturating_sub(new_tab_width)
    };
    let max_scroll = max_tab_scroll(&desired_widths, available);
    if !overflow {
        *tab_scroll = 0;
    } else if *reveal_focused_tab {
        if let Some(focused) = tabs.iter().position(|tab| tab.focused) {
            *tab_scroll = centered_tab_scroll(focused, &desired_widths, available).min(max_scroll);
        }
    } else {
        *tab_scroll = (*tab_scroll).min(max_scroll);
    }
    *reveal_focused_tab = false;

    let mut x = content.x;
    let tab_right = if overflow && mouse_chrome {
        hits.tab_scroll_left = Rect::new(
            content.x,
            content.y,
            TAB_SCROLL_BUTTON_WIDTH.min(content.width),
            1,
        );
        put_text(
            buffer,
            hits.tab_scroll_left.x,
            content.y,
            hits.tab_scroll_left.width,
            " < ",
            Style::default()
                .fg(if *tab_scroll > 0 {
                    palette.overlay1
                } else {
                    palette.overlay0
                })
                .bg(palette.surface0),
        );
        x = hits.tab_scroll_left.right();
        content
            .right()
            .saturating_sub(NEW_TAB_WIDTH + TAB_SCROLL_BUTTON_WIDTH)
    } else {
        content.right().saturating_sub(new_tab_width)
    };

    let mut first_visible = None;
    let mut last_visible = None;
    for (index, tab) in tabs.iter().enumerate().skip(*tab_scroll) {
        let name = tab_label(tab);
        let desired = desired_widths[index];
        let remaining = tab_right.saturating_sub(x);
        let width = desired.min(remaining);
        if width == 0 {
            break;
        }
        let rect = Rect::new(x, area.y, width, 1);
        put_tab(buffer, rect, &name, tab, palette);
        hits.tabs.push((rect, tab.tab_id.clone()));
        first_visible.get_or_insert(index);
        last_visible = Some(index);
        x = x.saturating_add(width + 1);
        if width < desired {
            break;
        }
    }

    if overflow && mouse_chrome {
        hits.tab_scroll_right = Rect::new(tab_right, area.y, TAB_SCROLL_BUTTON_WIDTH, 1);
        let can_scroll_right = *tab_scroll < max_scroll;
        put_text(
            buffer,
            hits.tab_scroll_right.x,
            area.y,
            hits.tab_scroll_right.width,
            " > ",
            Style::default()
                .fg(if can_scroll_right {
                    palette.overlay1
                } else {
                    palette.overlay0
                })
                .bg(palette.surface0),
        );
        hits.new_tab = Rect::new(
            hits.tab_scroll_right.right(),
            area.y,
            content
                .right()
                .saturating_sub(hits.tab_scroll_right.right())
                .min(NEW_TAB_WIDTH),
            1,
        );
    } else if mouse_chrome {
        hits.new_tab = Rect::new(
            x.min(content.right()),
            area.y,
            content.right().saturating_sub(x).min(NEW_TAB_WIDTH),
            1,
        );
    }
    if mouse_chrome {
        put_text(
            buffer,
            hits.new_tab.x,
            area.y,
            hits.new_tab.width,
            " + ",
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
    }

    if first_visible.is_some_and(|index| index > 0) {
        let ellipsis_x = if hits.tab_scroll_left.width > 0 {
            hits.tab_scroll_left.right()
        } else {
            content.x
        };
        put_text(
            buffer,
            ellipsis_x,
            area.y,
            u16::from(ellipsis_x < content.right()),
            "…",
            Style::default().fg(palette.overlay0),
        );
    }
    if last_visible.is_some_and(|index| index + 1 < tabs.len()) {
        let ellipsis_x = if hits.tab_scroll_right.width > 0 {
            hits.tab_scroll_right.x.saturating_sub(1)
        } else {
            content.right().saturating_sub(1)
        };
        put_text(
            buffer,
            ellipsis_x,
            area.y,
            u16::from(ellipsis_x >= content.x && ellipsis_x < content.right()),
            "…",
            Style::default().fg(palette.overlay0),
        );
    }

    render_tab_drop_indicator(buffer, area, palette, &tabs, tab_drag_insert_index, hits);
    render_tab_bar_status(buffer, area, snapshot, palette);
}

fn focused_tabs(snapshot: &ClientShellSnapshot) -> Vec<&ClientShellTab> {
    snapshot
        .tabs
        .iter()
        .filter(|tab| Some(tab.workspace_id.as_str()) == snapshot.focused_workspace_id.as_deref())
        .collect()
}

fn tab_desired_widths(tabs: &[&ClientShellTab]) -> Vec<u16> {
    tabs.iter()
        .map(|tab| {
            let label = tab_label(tab);
            display_width(&label).saturating_add(4).max(MIN_TAB_WIDTH)
        })
        .collect()
}

fn put_tab(buffer: &mut Buffer, rect: Rect, name: &str, tab: &ClientShellTab, palette: &Palette) {
    let style = if tab.focused {
        let base = Style::default()
            .fg(panel_contrast_fg(palette))
            .bg(palette.accent);
        if tab.custom_label {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        }
    } else if tab.custom_label {
        Style::default().fg(palette.overlay1).bg(palette.surface0)
    } else {
        Style::default().fg(palette.overlay0).bg(palette.surface0)
    };
    let padding = rect.width.saturating_sub(display_width(name));
    let left = padding / 2;
    let text = format!(
        "{empty:left$}{name}{empty:right_padding$}",
        empty = "",
        left = left as usize,
        right_padding = padding.saturating_sub(left) as usize,
    );
    put_text(buffer, rect.x, rect.y, rect.width, &text, style);
}

/// Flow `item_widths` left to right with a one-column gap between items, wrapping to a new row when
/// the next item would overflow `width`. Returns `(x, row, clamped_width)` per item, where `row` is
/// zero-based and `x` is relative to the left edge. The first item on a row never wraps; an item
/// wider than `width` is clamped to it. `width` must be non-zero.
fn wrap_flow(item_widths: impl Iterator<Item = u16>, width: u16) -> Vec<(u16, u16, u16)> {
    let mut flow = Vec::new();
    let mut x = 0u16;
    let mut row = 0u16;
    for item in item_widths {
        let item = item.min(width).max(1);
        if x > 0 && x.saturating_add(item) > width {
            row = row.saturating_add(1);
            x = 0;
        }
        flow.push((x, row, item));
        x = x.saturating_add(item.saturating_add(1));
    }
    flow
}

/// Rows a wrapped tab bar needs to show every tab of the focused workspace at `width`. Includes the
/// trailing new-tab (`+`) control when `mouse_chrome` is set, so the height reserved by the layout
/// matches the rows [`render_wrapped_tabs`] lays out at the same width.
pub(crate) fn wrapped_tab_bar_rows(
    snapshot: &ClientShellSnapshot,
    width: u16,
    mouse_chrome: bool,
) -> u16 {
    // Deliberate: `ui.tab_bar_right` renders on the first row only, but every wrapped row reserves
    // its width, so this and `render_wrapped_tabs` flow against one identical content width and the
    // height reserved here always equals the rows drawn. Reclaiming that space on rows 2+ would mean
    // a per-row width, i.e. a geometry discontinuity the drop indicator and hit testing both carry.
    let content = tab_bar_content_area(snapshot, Rect::new(0, 0, width, 1));
    let tabs = focused_tabs(snapshot);
    if content.width == 0 || tabs.is_empty() {
        return 1;
    }
    let widths = tab_desired_widths(&tabs)
        .into_iter()
        .chain(mouse_chrome.then_some(NEW_TAB_WIDTH));
    wrap_flow(widths, content.width)
        .last()
        .map(|(_, row, _)| row.saturating_add(1))
        .unwrap_or(1)
}

/// Lay every tab of the focused workspace across the rows of `area`, wrapping instead of scrolling,
/// so there are no `<`/`>` controls and no overflow ellipses. Each tab's hit rect carries its own
/// row, which is all the row-aware hit testing in `mouse.rs` needs.
///
/// When the flow needs more rows than `area.height` allows — only possible once the bar height is
/// clamped, e.g. very many tabs in a short terminal — the visible rows are scrolled so the focused
/// tab's row stays in view. Tabs outside that window get no hit rect; selecting one by keyboard
/// re-anchors the window on its row.
fn render_wrapped_tabs(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    tabs: &[&ClientShellTab],
    hits: &mut ShellHitMap,
) {
    // Same reserved width on every row as `wrapped_tab_bar_rows` computed the height from; see the
    // trade recorded there.
    let content = tab_bar_content_area(snapshot, area);
    if content.width == 0 || area.height == 0 {
        return;
    }
    let palette = &config.palette;
    let mouse_chrome = config.mouse_capture;
    let widths = tab_desired_widths(tabs)
        .into_iter()
        .chain(mouse_chrome.then_some(NEW_TAB_WIDTH));
    let flow = wrap_flow(widths, content.width);
    let total_rows = flow
        .last()
        .map(|(_, row, _)| row.saturating_add(1))
        .unwrap_or(0);
    let row_offset = if total_rows > area.height {
        let focused_row = tabs
            .iter()
            .position(|tab| tab.focused)
            .and_then(|index| flow.get(index))
            .map(|(_, row, _)| *row)
            .unwrap_or(0);
        focused_row
            .saturating_sub(area.height / 2)
            .min(total_rows.saturating_sub(area.height))
    } else {
        0
    };

    for (item, (x, row, width)) in flow.into_iter().enumerate() {
        let Some(offset_row) = row.checked_sub(row_offset) else {
            continue;
        };
        if offset_row >= area.height {
            continue;
        }
        let rect = Rect::new(content.x.saturating_add(x), area.y + offset_row, width, 1);
        match tabs.get(item) {
            Some(tab) => {
                put_tab(buffer, rect, &tab_label(tab), tab, palette);
                hits.tabs.push((rect, tab.tab_id.clone()));
            }
            None => {
                hits.new_tab = rect;
                put_text(
                    buffer,
                    rect.x,
                    rect.y,
                    rect.width,
                    " + ",
                    Style::default().fg(palette.overlay1).bg(palette.panel_bg),
                );
            }
        }
    }
}

fn render_tab_drop_indicator(
    buffer: &mut Buffer,
    area: Rect,
    palette: &Palette,
    tabs: &[&ClientShellTab],
    tab_drag_insert_index: Option<usize>,
    hits: &ShellHitMap,
) {
    let Some(insert_index) = tab_drag_insert_index else {
        return;
    };
    let Some((x, y)) = tab_drop_indicator_position(hits, tabs, insert_index) else {
        return;
    };
    put_text(
        buffer,
        x.min(area.right().saturating_sub(1)),
        y.min(area.bottom().saturating_sub(1)),
        1,
        "│",
        Style::default().fg(palette.accent),
    );
}

pub(crate) fn tab_bar_status_width(snapshot: &ClientShellSnapshot) -> u16 {
    let content = snapshot.tab_bar_right.iter().fold(0u16, |width, segment| {
        width.saturating_add(display_width(&segment.text))
    });
    let separators = snapshot.tab_bar_right.len().saturating_sub(1);
    content.saturating_add(
        display_width(&snapshot.tab_bar_right_separator)
            .saturating_mul(separators.min(u16::MAX as usize) as u16),
    )
}

fn tab_bar_status_area(snapshot: &ClientShellSnapshot, area: Rect) -> Option<Rect> {
    let width = tab_bar_status_width(snapshot);
    if width == 0 {
        return None;
    }
    let reserved = width.saturating_add(1);
    (area.width.saturating_sub(reserved) >= MIN_TAB_STRIP_WIDTH)
        .then(|| Rect::new(area.right().saturating_sub(width), area.y, width, 1))
}

fn tab_bar_content_area(snapshot: &ClientShellSnapshot, area: Rect) -> Rect {
    let reserved = tab_bar_status_area(snapshot, area)
        .map(|status| status.width.saturating_add(1))
        .unwrap_or(0);
    Rect {
        width: area.width.saturating_sub(reserved),
        ..area
    }
}

fn render_tab_bar_status(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    palette: &Palette,
) {
    let Some(status) = tab_bar_status_area(snapshot, area) else {
        return;
    };
    let separator_width = display_width(&snapshot.tab_bar_right_separator);
    let mut x = status.x;
    for (index, segment) in snapshot.tab_bar_right.iter().enumerate() {
        if index > 0 && separator_width > 0 {
            put_text(
                buffer,
                x,
                area.y,
                separator_width,
                &snapshot.tab_bar_right_separator,
                Style::default().fg(palette.overlay0).bg(palette.panel_bg),
            );
            x = x.saturating_add(separator_width);
        }
        let width = display_width(&segment.text);
        let style = if segment.accent {
            Style::default()
                .fg(panel_contrast_fg(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.overlay1).bg(palette.panel_bg)
        };
        put_text(buffer, x, area.y, width, &segment.text, style);
        x = x.saturating_add(width);
    }
}

/// Cell the tab-reorder drop indicator belongs in, as `(x, y)`. The row matters because a wrapped
/// bar spans several of them; on a single-row bar every candidate rect shares `area.y`, so this
/// reduces to the horizontal-only placement.
fn tab_drop_indicator_position(
    hits: &ShellHitMap,
    tabs: &[&ClientShellTab],
    insert_index: usize,
) -> Option<(u16, u16)> {
    let visible = hits
        .tabs
        .iter()
        .filter_map(|(rect, tab_id)| {
            tabs.iter()
                .position(|tab| tab.tab_id == *tab_id)
                .map(|index| (index, *rect))
        })
        .collect::<Vec<_>>();
    let (first_index, first_rect) = *visible.first()?;
    let (last_index, last_rect) = *visible.last()?;
    if insert_index == 0 {
        return Some(if first_index == 0 {
            (first_rect.x, first_rect.y)
        } else {
            (hits.tab_scroll_left.right(), hits.tab_scroll_left.y)
        });
    }
    if let Some((_, rect)) = visible.iter().find(|(index, _)| *index == insert_index) {
        return Some((rect.x.saturating_sub(1), rect.y));
    }
    if insert_index >= tabs.len() {
        return Some(if last_index + 1 >= tabs.len() {
            (last_rect.right(), last_rect.y)
        } else {
            (
                hits.tab_scroll_right.x.saturating_sub(1),
                hits.tab_scroll_right.y,
            )
        });
    }
    None
}

fn centered_tab_scroll(focused: usize, widths: &[u16], available: u16) -> usize {
    let mut best = focused;
    let mut best_distance = u16::MAX;
    for start in 0..=focused {
        let before = widths
            .iter()
            .copied()
            .enumerate()
            .skip(start)
            .take(focused.saturating_sub(start))
            .fold(0u16, |width, (_, tab)| width.saturating_add(tab + 1));
        if before >= available {
            continue;
        }
        let focused_width = widths[focused].min(available.saturating_sub(before));
        let center = before.saturating_mul(2).saturating_add(focused_width);
        let distance = center.abs_diff(available);
        if distance <= best_distance {
            best_distance = distance;
            best = start;
        }
    }
    best
}

fn max_tab_scroll(widths: &[u16], available: u16) -> usize {
    let Some((&last, preceding)) = widths.split_last() else {
        return 0;
    };
    let mut start = preceding.len();
    let mut used = u32::from(last);
    // Keep the longest fully visible suffix, not merely a sliver of the last tab.
    // An oversized last tab must still be reachable at the start of the strip.
    for width in preceding.iter().rev() {
        let required = used + 1 + u32::from(*width);
        if required > u32::from(available) {
            break;
        }
        used = required;
        start -= 1;
    }
    start
}

fn tab_label(tab: &ClientShellTab) -> String {
    if tab.zoomed {
        format!("{} Z", tab.label)
    } else {
        tab.label.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::max_tab_scroll;

    #[test]
    fn trailing_scroll_limit_accounts_for_full_widths_and_separators() {
        for (widths, available, expected) in [
            (&[][..], 0, 0),
            (&[8, 13][..], 0, 1),
            (&[8, 13][..], 1, 1),
            (&[8, 13][..], 12, 1),
            (&[8, 13][..], 21, 1),
            (&[8, 13][..], 22, 0),
            (&[8, 13][..], 30, 0),
            (&[8, u16::MAX][..], u16::MAX, 1),
        ] {
            assert_eq!(
                max_tab_scroll(widths, available),
                expected,
                "widths={widths:?}, available={available}"
            );
        }
    }
}
