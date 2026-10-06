//! Ledger #49. The client numbers a pane's rows from the top of its retained
//! history. When a full scrollback drops its oldest lines (or the history is
//! cleared), every row number now names a later line. A selection held across
//! that must never copy the renumbered rows: it copies nothing and says so, or,
//! for a double-click, only its frozen word.
use super::*;

const VIEW: usize = 4;
const WIDTH: usize = 40;
const NOT_COPIED: &str = "not copied: scrollback was trimmed";
const COPIED: &str = "copied to clipboard";

/// A pane's terminal as the server holds it: retained lines, top first, and the
/// view's offset from the bottom. Every line is unique, so a copy names its line.
struct TrimTerminal {
    lines: Vec<String>,
    next: usize,
    offset: usize,
    content_revision: u64,
    surface_revision: u64,
}

impl TrimTerminal {
    fn new(lines: usize, offset: usize) -> Self {
        let mut terminal = Self {
            lines: Vec::new(),
            next: 0,
            offset: 0,
            content_revision: 0,
            surface_revision: 1,
        };
        terminal.push(lines);
        terminal.offset = offset;
        terminal
    }

    fn line(number: usize) -> String {
        format!("alpha BRAVO{number:03} charlie")
    }

    fn push(&mut self, count: usize) {
        for _ in 0..count {
            self.lines.push(Self::line(self.next));
            self.next += 1;
        }
    }

    fn max(&self) -> usize {
        self.lines.len() - VIEW
    }

    fn top(&self) -> usize {
        self.max() - self.offset
    }

    /// Output: new lines at the bottom. A view scrolled up onto history stays
    /// on the same lines, as Ghostty keeps it.
    fn output(&mut self, count: usize) {
        self.push(count);
        if self.offset > 0 {
            self.offset += count;
        }
        self.content_revision += 1;
    }

    /// A full scrollback drops its oldest `count` lines.
    fn trim(&mut self, count: usize) {
        self.lines.drain(..count);
        self.offset = self.offset.min(self.max());
        self.content_revision += 1;
    }

    /// The scrollback is cleared: only the screen remains.
    fn clear(&mut self) {
        let keep = self.lines.len() - VIEW;
        self.lines.drain(..keep);
        self.offset = 0;
        self.content_revision += 1;
    }

    /// The text of absolute row `row` as the client numbers it now.
    fn row(&self, row: usize) -> &str {
        &self.lines[row]
    }

    fn surface(&mut self) -> PaneSurfaceFrame {
        let rows: Vec<String> = self.lines[self.top()..self.top() + VIEW]
            .iter()
            .map(|line| format!("{line:<WIDTH$}"))
            .collect();
        let mut pane_surface = surface();
        let buffer = Buffer::with_lines(rows);
        pane_surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
        pane_surface.surface_revision = self.surface_revision;
        self.surface_revision += 1;
        let pane = &mut pane_surface.panes[0];
        pane.content_revision = self.content_revision;
        pane.rect.width = buffer.area.width;
        pane.rect.height = buffer.area.height;
        pane.inner_rect = pane.rect;
        pane.scroll = Some(crate::protocol::PaneSurfaceScrollMetrics {
            offset_from_bottom: self.offset as u64,
            max_offset_from_bottom: self.max() as u64,
            viewport_rows: VIEW as u64,
        });
        pane_surface
    }

    /// The server's `pane.selection.read`: whatever the rows hold now.
    fn read(
        &self,
        params: &crate::api::schema::PaneSelectionReadParams,
    ) -> Result<crate::api::schema::ResponseResult, ClientShellEndpointError> {
        let (first, last) = (params.anchor.row as usize, params.cursor.row as usize);
        if last >= self.lines.len() {
            return Err(ClientShellEndpointError {
                code: Some("selection_unavailable".into()),
                message: "selection is outside the pane history".into(),
            });
        }
        let text = (first..=last)
            .map(|row| {
                let line = format!("{:<WIDTH$}", self.lines[row]);
                let start = if row == first {
                    usize::from(params.anchor.col)
                } else {
                    0
                };
                let end = if row == last {
                    usize::from(params.cursor.col) + 1
                } else {
                    WIDTH
                };
                line[start..end.min(WIDTH)].trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(crate::api::schema::ResponseResult::PaneSelection {
            pane_id: params.pane_id.clone(),
            text,
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum Delivery {
    Surface,
    Patch,
}

/// The terminal's current state reaching the client, as a full surface or as a
/// patch of every pane row, and drawn.
fn deliver(state: &mut ClientShellState, terminal: &mut TrimTerminal, delivery: Delivery) {
    let next = terminal.surface();
    match delivery {
        Delivery::Surface => state.set_pane_surface(next),
        Delivery::Patch => {
            let base = state
                .pane_surface
                .as_ref()
                .expect("a surface")
                .surface_revision;
            let width = usize::from(next.frame.width);
            let rows = (0..usize::from(next.frame.height))
                .map(|y| crate::protocol::PaneSurfacePatchRow {
                    x: 0,
                    y: y as u16,
                    cells: next.frame.cells[y * width..(y + 1) * width].to_vec(),
                })
                .collect();
            assert!(matches!(
                state.apply_pane_surface_patch(crate::protocol::PaneSurfacePatch {
                    boot_id: next.boot_id.clone(),
                    projection_revision: next.projection_revision,
                    base_surface_revision: base,
                    surface_revision: base + 1,
                    panes: next.panes.clone(),
                    rows,
                    cursor: next.frame.cursor.clone(),
                }),
                super::super::surface_patch::ClientPaneSurfacePatchOutcome::Applied(_)
            ));
            terminal.surface_revision = base + 2;
        }
    }
    state.compose(106, 20).expect("composed frame");
    state.commit_presented_frame();
}

fn trim_state(copy_on_select: bool, terminal: &mut TrimTerminal) -> ClientShellState {
    let mut config = Config::default();
    config.ui.copy_on_select = copy_on_select;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    deliver(&mut state, terminal, Delivery::Surface);
    state
}

fn mouse(
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

fn key(state: &mut ClientShellState, code: KeyCode, modifiers: KeyModifiers) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        code, modifiers,
    ))])
}

/// Answer every selection read in `actions` from the terminal as it is now,
/// until none is left. Returns the reads sent and every clipboard payload.
fn serve(
    state: &mut ClientShellState,
    terminal: &TrimTerminal,
    mut actions: Vec<ClientShellAction>,
) -> (usize, Vec<String>) {
    let mut reads = 0;
    let mut copied = Vec::new();
    while !actions.is_empty() {
        let mut next = Vec::new();
        for action in actions {
            match action {
                ClientShellAction::Endpoint { request, .. } => {
                    if let crate::api::schema::Method::PaneSelectionRead(params) = &request.method {
                        reads += 1;
                        let result = terminal.read(params);
                        next.extend(
                            state
                                .handle_endpoint_result("boot-1", &request.id, result)
                                .1,
                        );
                    }
                }
                ClientShellAction::ClipboardWrite(bytes) => {
                    copied.push(String::from_utf8_lossy(&bytes).into_owned());
                }
                _ => {}
            }
        }
        actions = next;
    }
    (reads, copied)
}

fn notice(state: &ClientShellState) -> Option<&str> {
    state
        .copy_feedback
        .as_ref()
        .map(|feedback| feedback.message.as_str())
}

#[derive(Clone, Copy, Debug)]
enum Renumber {
    Trim,
    Clear,
    /// Output only: the history grows, nothing is renumbered (control).
    None,
}

/// A plain drag over two rows, held while the history is trimmed or cleared
/// under it. The copy would be other lines (the server reads the renumbered
/// rows), so the highlight goes at once and the release copies nothing, with a
/// notice. With output alone the same drag copies exactly what was selected.
#[test]
fn held_drag_across_a_renumbering_copies_nothing_and_says_so() {
    let mut failures = Vec::new();
    for offset in [0, 40] {
        for delivery in [Delivery::Surface, Delivery::Patch] {
            for renumber in [Renumber::Trim, Renumber::Clear, Renumber::None] {
                let case = format!("offset {offset} {delivery:?} {renumber:?}");
                let mut terminal = TrimTerminal::new(60, offset);
                let mut state = trim_state(true, &mut terminal);
                let top = terminal.top();
                let expected = format!(
                    "{}\n{}",
                    terminal.row(top + 1),
                    &terminal.row(top + 2)[..13]
                );
                mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 0);
                mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 2, 12);
                if state.selection.as_ref().map(|s| s.ordered_cells())
                    != Some(((top as u32 + 1, 0), (top as u32 + 2, 12)))
                {
                    failures.push(format!("{case}: drag not selected"));
                    continue;
                }
                terminal.output(1);
                match renumber {
                    Renumber::Trim => terminal.trim(10),
                    Renumber::Clear => terminal.clear(),
                    Renumber::None => {}
                }
                deliver(&mut state, &mut terminal, delivery);
                let renumbered = !matches!(renumber, Renumber::None);
                if renumbered && state.selection.is_some() {
                    failures.push(format!("{case}: highlight kept over renumbered rows"));
                }
                let release = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 2, 12);
                let (reads, copied) = serve(&mut state, &terminal, release.actions);
                if renumbered {
                    if reads != 0 || !copied.is_empty() {
                        failures.push(format!(
                            "{case}: never read renumbered rows: {reads} reads, copied {copied:?}"
                        ));
                    }
                    if notice(&state) != Some(NOT_COPIED) {
                        failures.push(format!("{case}: notice {:?}", notice(&state)));
                    }
                } else {
                    if copied != [expected.clone()] {
                        failures.push(format!(
                            "{case}: control copied {copied:?}, want {expected:?}"
                        ));
                    }
                    if notice(&state) != Some(COPIED) {
                        failures.push(format!("{case}: control notice {:?}", notice(&state)));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A press that had not moved yet when the rows were renumbered. If the pointer
/// then drags, the release copies nothing and says so; a press released without
/// moving is a plain click and gets no notice.
#[test]
fn press_before_a_renumbering_then_drag_copies_nothing_and_says_so() {
    let mut failures = Vec::new();
    for offset in [0, 40] {
        for drags in [true, false] {
            let case = format!("offset {offset} drags {drags}");
            let mut terminal = TrimTerminal::new(60, offset);
            let mut state = trim_state(true, &mut terminal);
            mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 0);
            terminal.output(1);
            terminal.trim(10);
            deliver(&mut state, &mut terminal, Delivery::Surface);
            if drags {
                mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 2, 12);
                if state.selection.is_some() {
                    failures.push(format!("{case}: selected from a renumbered anchor"));
                }
            }
            let release = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 2, 12);
            let (reads, copied) = serve(&mut state, &terminal, release.actions);
            if reads != 0 || !copied.is_empty() {
                failures.push(format!("{case}: {reads} reads, copied {copied:?}"));
            }
            let want = drags.then_some(NOT_COPIED);
            if notice(&state) != want {
                failures.push(format!(
                    "{case}: notice {:?}, want {want:?}",
                    notice(&state)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// `ui.copy_on_select` off: a finished drag stays selected until copied by key.
/// Renumbered before the key, it is dropped at once; the copy key then copies
/// nothing, says so and is not passed to the program. Only that one key is
/// taken: the next Ctrl+C reaches the program again, and any other key first
/// gives Ctrl+C back to the program. With output alone the key copies the text.
#[test]
fn kept_selection_renumbered_before_its_copy_key_copies_nothing_and_says_so() {
    let mut failures = Vec::new();
    for offset in [0, 40] {
        for (renumber, other_key_first) in [(true, false), (true, true), (false, false)] {
            let case = format!("offset {offset} renumber {renumber} other key {other_key_first}");
            let mut terminal = TrimTerminal::new(60, offset);
            let mut state = trim_state(false, &mut terminal);
            let top = terminal.top();
            let expected = format!(
                "{}\n{}",
                terminal.row(top + 1),
                &terminal.row(top + 2)[..13]
            );
            mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 0);
            mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 2, 12);
            let release = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 2, 12);
            if !serve(&mut state, &terminal, release.actions).1.is_empty()
                || state.selection.is_none()
            {
                failures.push(format!("{case}: selection not kept"));
                continue;
            }
            terminal.output(1);
            if renumber {
                terminal.trim(10);
            }
            deliver(&mut state, &mut terminal, Delivery::Surface);
            if renumber && state.selection.is_some() {
                failures.push(format!("{case}: highlight kept over renumbered rows"));
            }
            if other_key_first {
                key(&mut state, KeyCode::Char('x'), KeyModifiers::empty());
            }
            let copy = key(&mut state, KeyCode::Char('c'), KeyModifiers::CONTROL);
            let to_program = !copy.requests.is_empty();
            let (reads, copied) = serve(&mut state, &terminal, copy.actions);
            if !renumber {
                if copied != [expected.clone()] || to_program {
                    failures.push(format!(
                        "{case}: control copied {copied:?} want {expected:?}, to program {to_program}"
                    ));
                }
                continue;
            }
            if reads != 0 || !copied.is_empty() {
                failures.push(format!("{case}: {reads} reads, copied {copied:?}"));
            }
            if other_key_first {
                if !to_program {
                    failures.push(format!(
                        "{case}: Ctrl+C after another key must reach the program"
                    ));
                }
                continue;
            }
            if to_program {
                failures.push(format!("{case}: the copy key reached the program"));
            }
            if notice(&state) != Some(NOT_COPIED) {
                failures.push(format!("{case}: notice {:?}", notice(&state)));
            }
            let again = key(&mut state, KeyCode::Char('c'), KeyModifiers::CONTROL);
            if again.requests.is_empty() {
                failures.push(format!("{case}: a second Ctrl+C must reach the program"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A double-click dragged on into the next rows, held across a renumbering. The
/// range's rows are renumbered, and so is the word's own row, so nothing can be
/// placed on screen any more: the highlight goes, the gesture stays on its
/// frozen word, later motion extends nothing, and the release copies the word.
#[test]
fn extended_double_click_across_a_renumbering_copies_only_its_word() {
    let mut failures = Vec::new();
    for offset in [0, 40] {
        for delivery in [Delivery::Surface, Delivery::Patch] {
            let case = format!("offset {offset} {delivery:?}");
            let mut terminal = TrimTerminal::new(60, offset);
            let mut state = trim_state(true, &mut terminal);
            let top = terminal.top();
            let word = terminal.row(top + 1)[6..14].to_owned();
            mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 8);
            mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 1, 8);
            mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 8);
            let drag = mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 2, 18);
            if !drag.actions.is_empty()
                || state.selection.as_ref().map(|s| s.ordered_cells())
                    != Some(((top as u32 + 1, 6), (top as u32 + 2, 21)))
            {
                failures.push(format!(
                    "{case}: not extended from the screen: {:?} {:?}",
                    state.selection.as_ref().map(|s| s.ordered_cells()),
                    drag.actions
                ));
                continue;
            }
            terminal.output(1);
            terminal.trim(10);
            deliver(&mut state, &mut terminal, delivery);
            if state.selection.is_some() {
                failures.push(format!(
                    "{case}: highlight over renumbered rows: {:?}",
                    state.selection.as_ref().map(|s| s.ordered_cells())
                ));
            }
            let later = mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 3, 30);
            let (later_reads, _) = serve(&mut state, &terminal, later.actions);
            if later_reads != 0 || state.selection.is_some() {
                failures.push(format!(
                    "{case}: extended after the renumbering: {later_reads} reads, {:?}",
                    state.selection.as_ref().map(|s| s.ordered_cells())
                ));
            }
            let release = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 3, 30);
            let (reads, copied) = serve(&mut state, &terminal, release.actions);
            if reads != 0 || copied != [word.clone()] {
                failures.push(format!(
                    "{case}: {reads} reads, copied {copied:?}, want [{word:?}]"
                ));
            }
            if notice(&state) != Some(COPIED) {
                failures.push(format!("{case}: notice {:?}", notice(&state)));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A plain double-click is not affected: it copies its frozen word. Its short
/// copied highlight sits on rows that are then renumbered, so it goes at once.
#[test]
fn plain_double_click_across_a_renumbering_copies_its_word() {
    let mut failures = Vec::new();
    for offset in [0, 40] {
        let case = format!("offset {offset}");
        let mut terminal = TrimTerminal::new(60, offset);
        let mut state = trim_state(true, &mut terminal);
        let word = terminal.row(terminal.top() + 1)[6..14].to_owned();
        mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 8);
        mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 1, 8);
        mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 8);
        terminal.output(1);
        terminal.trim(10);
        deliver(&mut state, &mut terminal, Delivery::Surface);
        let release = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 1, 8);
        let (reads, copied) = serve(&mut state, &terminal, release.actions);
        if reads != 0 || copied != [word.clone()] || notice(&state) != Some(COPIED) {
            failures.push(format!(
                "{case}: {reads} reads, copied {copied:?}, notice {:?}",
                notice(&state)
            ));
        }
        // The word was copied and is highlighted for a moment; then the rows move.
        let mut terminal = TrimTerminal::new(60, offset);
        let mut state = trim_state(true, &mut terminal);
        mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 8);
        mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 1, 8);
        mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 8);
        mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 1, 8);
        if state.selection.is_none() {
            failures.push(format!("{case}: copied word not highlighted"));
        }
        terminal.output(1);
        terminal.trim(10);
        deliver(&mut state, &mut terminal, Delivery::Surface);
        if state.selection.is_some() {
            failures.push(format!(
                "{case}: copied highlight kept over renumbered rows"
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
