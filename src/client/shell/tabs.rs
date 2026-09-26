use super::super::{status_color, status_icon, status_priority};
use super::*;
use crate::protocol::ClientShellAgent;

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
    let desired_widths = tab_desired_widths(snapshot, &tabs, config);
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
        let name = decorated_tab_label(config, snapshot, tab);
        let desired = desired_widths[index];
        let remaining = tab_right.saturating_sub(x);
        let width = desired.min(remaining);
        if width == 0 {
            break;
        }
        let rect = Rect::new(x, area.y, width, 1);
        put_tab(buffer, rect, &name, tab, snapshot, config);
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

/// The tab's highest-attention live agent, if it has one. The server only admits panes whose
/// terminal `is_agent_terminal()` to the snapshot's agent list, which is the same "pane with an
/// agent" gate the v0.8.x tab marks filtered on — so this reproduces that selection from the wire.
/// `max_by_key` keeps the last maximum, so ties preserve snapshot order.
fn tab_agent<'a>(
    snapshot: &'a ClientShellSnapshot,
    tab: &ClientShellTab,
) -> Option<&'a ClientShellAgent> {
    snapshot
        .agents
        .iter()
        .filter(|agent| agent.tab_id == tab.tab_id)
        .max_by_key(|agent| status_priority(agent.agent_status))
}

/// Mark and colour the `ui.tab_agent_status` prefix draws at the head of a tab label. The mark
/// comes from `status_icon` — the one status vocabulary the agent panel, endpoint lists and mobile
/// header all share — called directly rather than wrapped, so a vocabulary change upstream reaches
/// the tab bar for free and no second glyph table can drift. `None` when the feature is off or the
/// tab has no agent, leaving the stock label geometry untouched.
fn tab_status_prefix(
    config: &ClientShellConfig,
    snapshot: &ClientShellSnapshot,
    tab: &ClientShellTab,
) -> Option<(&'static str, ratatui::style::Color)> {
    if !config.tab_agent_status {
        return None;
    }
    let agent = tab_agent(snapshot, tab)?;
    Some((
        status_icon(agent.agent_status, config.status_indicators),
        status_color(agent.agent_status, &config.palette),
    ))
}

/// Columns the status prefix adds to a tab: one for the mark, one for the separator before the
/// name. Both stock paths (off) and agent-less tabs add zero.
fn tab_status_prefix_width(
    config: &ClientShellConfig,
    snapshot: &ClientShellSnapshot,
    tab: &ClientShellTab,
) -> u16 {
    u16::from(tab_status_prefix(config, snapshot, tab).is_some()) * 2
}

/// The icon an agent reporter prefixes `display_agent` with. Its lifecycle
/// marker sits directly after this icon in the first whitespace token. Pinning
/// the prefix is what lets the tab tell a real reporter marker from any other
/// leading emoji: the marker glyphs themselves are common, so a position-only
/// parse would false-positive.
const AGENT_LABEL_ICON: &str = "\u{1F977}";

/// Lifecycle marker from the reporter's `display_agent` grammar
/// `{icon}{marker} {~}N%\u{B7}MODEL\u{B7}PID {$cost}`. Bounded set: awaiting-user
/// (may carry a pending-ask count), ready-to-close, asked-to-stop, done,
/// agent-driven. Anything else yields `None` so the tab renders exactly as it
/// does today (fail closed).
fn extract_tab_agent_lifecycle_marker(display_agent: &str) -> Option<&str> {
    let first = display_agent.split_whitespace().next()?;
    let rest = first.strip_prefix(AGENT_LABEL_ICON)?;
    if rest.is_empty() {
        return None;
    }
    is_lifecycle_marker(rest).then_some(rest)
}

/// The awaiting-user marker may carry a pending-ask count; the others are bare.
fn is_lifecycle_marker(s: &str) -> bool {
    if let Some(count) = s.strip_prefix('\u{2753}') {
        return count.is_empty() || count.bytes().all(|b| b.is_ascii_digit());
    }
    matches!(s, "\u{1F4A4}" | "\u{270B}" | "\u{2705}" | "\u{1F916}")
}

fn extract_tab_agent_context(display_agent: &str) -> Option<&str> {
    let mut found = None;
    for candidate in display_agent.split_whitespace() {
        let body = candidate.strip_prefix('~').unwrap_or(candidate);
        let Some((percentage, tail)) = body.split_once("%\u{b7}") else {
            continue;
        };
        // `?` is a KNOWN-UNKNOWN reading: the agent has just compacted and has no
        // context figure yet. Rendering `?%` distinguishes "not known yet" from
        // an absent segment, which means "nothing reported at all". Without it a
        // healthy post-compaction agent is indistinguishable from a broken one.
        let percentage_is_unknown = percentage == "?";
        if (!percentage_is_unknown
            && (percentage.is_empty()
                || !percentage.bytes().all(|byte| byte.is_ascii_digit())
                || percentage
                    .parse::<u16>()
                    .ok()
                    .is_none_or(|value| value > 100)))
            || tail.is_empty()
            // The reporter's grammar keeps a numeric PID as the LAST
            // \u{b7}-segment; anything between it and the % is the model name.
            // Requiring the trailing segment to be numeric is what still
            // rejects a non-reporter label such as "~42%\u{b7}pid".
            || !tail
                .rsplit('\u{b7}')
                .next()
                .is_some_and(|last| !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }

        let percentage_end = candidate.len() - '\u{b7}'.len_utf8() - tail.len();
        let percentage = &candidate[..percentage_end];
        if found.is_some() {
            return None;
        }
        found = Some(percentage);
    }
    found
}

fn tab_desired_widths(
    snapshot: &ClientShellSnapshot,
    tabs: &[&ClientShellTab],
    config: &ClientShellConfig,
) -> Vec<u16> {
    tabs.iter()
        .map(|tab| {
            let label = decorated_tab_label(config, snapshot, tab);
            // The minimum applies to the stock label alone: the mark is ADDED on top of it,
            // never absorbed into it — a one-character tab keeps its stock width and gains the
            // mark's two cells, rather than paying for the mark out of its own padding.
            display_width(&label)
                .saturating_add(4)
                .max(MIN_TAB_WIDTH)
                .saturating_add(tab_status_prefix_width(config, snapshot, tab))
        })
        .collect()
}

/// Cell of a tab's label that carries the `ui.tab_close_button` marker, when it is on and the
/// tab is wide enough to give a cell up. Rendering and hit testing both derive the cell from here,
/// so they cannot drift apart.
fn tab_close_marker_x(rect: Rect, config: &ClientShellConfig) -> Option<u16> {
    (config.tab_close_button && config.mouse_capture && rect.width >= 2)
        .then(|| rect.right().saturating_sub(1))
}

fn put_tab(
    buffer: &mut Buffer,
    rect: Rect,
    name: &str,
    tab: &ClientShellTab,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
) {
    let palette = &config.palette;
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
    // The status mark and its separator take the head cells; the close marker takes the label's
    // last cell. The name centres in what is left, so each decoration claims its own cells and
    // the geometry and the paint cannot drift apart.
    let status_prefix = tab_status_prefix(config, snapshot, tab);
    let status_prefix_width = u16::from(status_prefix.is_some()) * 2;
    let close_marker = tab_close_marker_x(rect, config);
    let label_width = match close_marker {
        Some(_) => rect.width.saturating_sub(1),
        None => rect.width,
    }
    .saturating_sub(status_prefix_width);
    let padding = label_width.saturating_sub(display_width(name));
    let left = padding / 2;
    let text = format!(
        "{empty:left$}{name}{empty:right_padding$}",
        empty = "",
        left = left as usize,
        right_padding = padding.saturating_sub(left) as usize,
    );
    put_text(
        buffer,
        rect.x.saturating_add(status_prefix_width),
        rect.y,
        label_width,
        &text,
        style,
    );
    if let Some((mark, color)) = status_prefix {
        put_text(buffer, rect.x, rect.y, 1, mark, style.fg(color));
    }
    if let Some(x) = close_marker {
        put_text(buffer, x, rect.y, 1, "x", style);
    }
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
    config: &ClientShellConfig,
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
    let widths = tab_desired_widths(snapshot, &tabs, config)
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
    let widths = tab_desired_widths(snapshot, tabs, config)
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
                put_tab(
                    buffer,
                    rect,
                    &decorated_tab_label(config, snapshot, tab),
                    tab,
                    snapshot,
                    config,
                );
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

/// The tab's full label text: the pane name, then the `ui.tab_agent_context` decorations — the
/// reporter's lifecycle marker prefixed with one separator space and the context percentage
/// appended with one — then the zoom suffix. All three come from the SAME highest-attention
/// agent the status mark reads, so one agent's reading labels the tab. With the key off (or no
/// agent, or a non-reporter label) this is byte-identical to the stock label.
fn decorated_tab_label(
    config: &ClientShellConfig,
    snapshot: &ClientShellSnapshot,
    tab: &ClientShellTab,
) -> String {
    let mut name = tab.label.clone();
    if config.tab_agent_context {
        if let Some(display_agent) =
            tab_agent(snapshot, tab).and_then(|agent| agent.display_agent.as_deref())
        {
            if let Some(context) = extract_tab_agent_context(display_agent) {
                name.push(' ');
                name.push_str(context);
            }
            if let Some(marker) = extract_tab_agent_lifecycle_marker(display_agent) {
                name = format!("{marker} {name}");
            }
        }
    }
    if tab.zoomed {
        format!("{name} Z")
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn tab_agent_context_extracts_current_producer_format() {
        assert_eq!(
            extract_tab_agent_context("\u{1F977}\u{2705} ~42%\u{B7}4242 $1.23"),
            Some("~42%")
        );
    }

    #[test]
    fn tab_agent_context_tolerates_variable_prefix_and_suffix() {
        for (display_agent, expected) in [
            ("0%\u{B7}1", "0%"),
            ("working 100%\u{B7}999999 trailing", "100%"),
            ("\u{1F527} ready ~7%\u{B7}0042 cost=unknown", "~7%"),
        ] {
            assert_eq!(
                extract_tab_agent_context(display_agent),
                Some(expected),
                "display agent: {display_agent:?}"
            );
        }
    }

    #[test]
    fn tab_agent_context_rejects_absent_malformed_and_out_of_range_labels() {
        assert_eq!(
            extract_tab_agent_context("\u{1F977}\u{2705} working $1.23"),
            None
        );
        for display_agent in [
            "~%\u{B7}4242",
            "~42%4242",
            "~42%\u{B7}",
            "~42%\u{B7}pid",
            "101%\u{B7}4242",
            "~999%\u{B7}4242",
            "-1%\u{B7}4242",
            "42%\u{B7}-1",
            "x42%\u{B7}4242",
            "42%\u{B7}4242x",
        ] {
            assert_eq!(
                extract_tab_agent_context(display_agent),
                None,
                "display agent: {display_agent:?}"
            );
        }
    }

    #[test]
    fn unknown_context_reading_renders_as_question_mark_not_absence() {
        // The point of the feature: a just-compacted agent must be
        // DISTINGUISHABLE from one that reported nothing.
        assert_eq!(
            extract_tab_agent_context("\u{1F977} ?%\u{B7}46223"),
            Some("?%"),
            "unknown reading must render, not vanish"
        );
        assert_eq!(
            extract_tab_agent_context("\u{1F977} ?%\u{B7}claude-opus-5:max\u{B7}46223"),
            Some("?%"),
            "unknown reading must survive the model segment too"
        );
        // Absence still means absence -- the other half of the distinction.
        assert_eq!(extract_tab_agent_context("\u{1F977} \u{B7}46223"), None);
        // `?` is accepted ONLY as the whole percentage, never as a digit smuggler.
        for bad in [
            "?4%\u{B7}1",
            "4?%\u{B7}1",
            "??%\u{B7}1",
            "?%\u{B7}",
            "101%\u{B7}1",
        ] {
            assert_eq!(
                extract_tab_agent_context(&format!("\u{1F977} {bad}")),
                None,
                "must reject {bad:?}"
            );
        }
        // Real readings unaffected.
        assert_eq!(
            extract_tab_agent_context("\u{1F977} 42%\u{B7}46223"),
            Some("42%")
        );
        assert_eq!(
            extract_tab_agent_context("\u{1F977} ~2%\u{B7}46223"),
            Some("~2%")
        );
    }

    #[test]
    fn lifecycle_marker_prefixes_name_and_context_tolerates_model_segment() {
        // POSITIVE CONTROL: today's format must still yield the percentage.
        assert_eq!(
            extract_tab_agent_context("\u{1F977} 17%\u{B7}46223"),
            Some("17%"),
            "old format must keep working - reporters may adopt the new one later"
        );
        // New format: model inserted between pct and PID.
        assert_eq!(
            extract_tab_agent_context("\u{1F977} 17%\u{B7}claude-opus-5:max\u{B7}46223"),
            Some("17%"),
            "model segment must not break the percentage"
        );
        // Fails closed on an empty tail.
        assert_eq!(extract_tab_agent_context("\u{1F977} 17%\u{B7}"), None);
        // Marker extraction, whole bounded set.
        for (input, want) in [
            ("\u{1F977}\u{2705} 17%\u{B7}46223", Some("\u{2705}")),
            ("\u{1F977}\u{1F4A4} 17%\u{B7}46223", Some("\u{1F4A4}")),
            ("\u{1F977}\u{270B} 17%\u{B7}46223", Some("\u{270B}")),
            ("\u{1F977}\u{1F916} 17%\u{B7}46223", Some("\u{1F916}")),
            ("\u{1F977}\u{2753} 17%\u{B7}46223", Some("\u{2753}")),
            ("\u{1F977}\u{2753}4 17%\u{B7}46223", Some("\u{2753}4")),
            // absent marker -> None -> tab renders exactly as today
            ("\u{1F977} 17%\u{B7}46223", None),
            // no reporter icon -> None (guards against unrelated leading emoji)
            ("\u{2705} 17%\u{B7}46223", None),
            // unknown glyph after the icon -> None
            ("\u{1F977}\u{1F680} 17%\u{B7}46223", None),
            ("", None),
        ] {
            assert_eq!(
                extract_tab_agent_lifecycle_marker(input),
                want,
                "marker extraction for {input:?}"
            );
        }
    }

    /// REPORTER CONTRACT PIN. The agent reporter emits the
    /// `display_agent` label this parser consumes. That coupling has no build-time
    /// enforcement: nobody runs this suite after changing the reporter. This test
    /// is the durable statement of what herdr accepts, so the reporter side can
    /// cite it by NAME instead of relying on a code comment to point the way.
    ///
    /// Every form asserted here is a form the reporter may legitimately emit.
    /// If you are changing this test to make a new reporter format pass, that is
    /// the coordination point -- agree it with the reporter side; do not relax it quietly.
    #[test]
    fn contract_display_agent_label_forms_accepted_by_herdr() {
        for (label, want_pct, want_marker, why) in [
            // Historic form: percentage and PID only.
            (
                "\u{1F977} 17%\u{B7}46223",
                Some("17%"),
                None,
                "digits-only tail",
            ),
            // Model inserted between percentage and PID.
            (
                "\u{1F977} 17%\u{B7}claude-opus-5:max\u{B7}46223",
                Some("17%"),
                None,
                "model segment between pct and PID",
            ),
            // Estimate prefix survives both forms.
            (
                "\u{1F977} ~2%\u{B7}4242",
                Some("~2%"),
                None,
                "estimate prefix",
            ),
            (
                "\u{1F977} ~2%\u{B7}glm-5.2\u{B7}4242",
                Some("~2%"),
                None,
                "estimate prefix + model",
            ),
            // Unknown reading after a compaction.
            ("\u{1F977} ?%\u{B7}46223", Some("?%"), None, "unknown pct"),
            // Lifecycle marker rides the same first token.
            (
                "\u{1F977}\u{2705} 17%\u{B7}claude-opus-5:max\u{B7}46223",
                Some("17%"),
                Some("\u{2705}"),
                "marker + model + pid",
            ),
            (
                "\u{1F977}\u{2753}4 17%\u{B7}46223",
                Some("17%"),
                Some("\u{2753}4"),
                "pending-ask count",
            ),
            // Trailing tokens the reporter may append are IGNORED, not fatal.
            (
                "\u{1F977}\u{2705} 17%\u{B7}46223 $1.23",
                Some("17%"),
                Some("\u{2705}"),
                "cost suffix ignored",
            ),
            (
                "\u{1F977}\u{2705} 17%\u{B7}46223 $1.23 \u{25B8}BL-9",
                Some("17%"),
                Some("\u{2705}"),
                "trailing task token ignored",
            ),
        ] {
            assert_eq!(
                extract_tab_agent_context(label),
                want_pct,
                "percentage for {why}: {label:?}"
            );
            assert_eq!(
                extract_tab_agent_lifecycle_marker(label),
                want_marker,
                "marker for {why}: {label:?}"
            );
        }

        // REJECTED forms. These are the negatives that give the accepted list
        // its meaning: without them the parser could accept everything and this
        // test would still pass.
        for (label, why) in [
            ("\u{1F977} 17%\u{B7}", "empty tail"),
            ("\u{1F977} 17%\u{B7}pid", "non-numeric trailing segment"),
            ("\u{1F977} 101%\u{B7}1", "percentage out of range"),
            ("\u{1F977} ?4%\u{B7}1", "? mixed with digits"),
            (
                "\u{1F977} 17%\u{B7}1 42%\u{B7}2",
                "two candidates is ambiguous",
            ),
        ] {
            assert_eq!(
                extract_tab_agent_context(label),
                None,
                "must reject {why}: {label:?}"
            );
        }
    }
}
