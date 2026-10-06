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
    /// The next surfaces carry no scroll metrics (a transient frame).
    scroll_hidden: bool,
}

impl TrimTerminal {
    fn new(lines: usize, offset: usize) -> Self {
        let mut terminal = Self {
            lines: Vec::new(),
            next: 0,
            offset: 0,
            content_revision: 0,
            surface_revision: 1,
            scroll_hidden: false,
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
        pane.scroll = (!self.scroll_hidden).then(|| crate::protocol::PaneSurfaceScrollMetrics {
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
/// taken: the next Ctrl+C reaches the program again, and any other key or a
/// click first gives Ctrl+C back to the program. With output alone the key
/// copies the text.
#[test]
fn kept_selection_renumbered_before_its_copy_key_copies_nothing_and_says_so() {
    let mut failures = Vec::new();
    for offset in [0, 40] {
        for (renumber, first) in [
            (true, "nothing"),
            (true, "a key"),
            (true, "a click"),
            (false, "nothing"),
        ] {
            let other_key_first = first != "nothing";
            let case = format!("offset {offset} renumber {renumber}, first {first}");
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
            if first == "a key" {
                key(&mut state, KeyCode::Char('x'), KeyModifiers::empty());
            } else if first == "a click" {
                mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 0, 30);
                mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 30);
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
                    failures.push(format!("{case}: Ctrl+C afterwards must reach the program"));
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
/// range's rows are renumbered, and so is the word's own row, so nothing that
/// was selected can be copied or shown: the highlight goes, and the release
/// copies nothing and says so, like a plain drag. Copying only the word under a
/// "copied" notice would hand over less than was selected. The gesture stays
/// dropped, with or without later motion (here to the pane's bottom edge, or back onto the word):
/// motion neither extends, scrolls the view nor brings the word back. A trim between
/// the two clicks does not blind the gesture to a later one.
#[test]
fn extended_double_click_across_a_renumbering_copies_nothing_and_says_so() {
    let mut failures = Vec::new();
    for offset in [0, 40] {
        for (delivery, trim_between_clicks, later_motion) in [
            (Delivery::Surface, false, "to the edge"),
            (Delivery::Patch, false, "to the edge"),
            (Delivery::Surface, true, "to the edge"),
            (Delivery::Surface, false, "to the edge and back to the word"),
            (Delivery::Surface, false, "none"),
        ] {
            let back_to_word = later_motion == "to the edge and back to the word";
            let case = format!(
                "offset {offset} {delivery:?} trim between clicks {trim_between_clicks} later motion {later_motion}"
            );
            let mut terminal = TrimTerminal::new(60, offset);
            let mut state = trim_state(true, &mut terminal);
            mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 8);
            mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 1, 8);
            if trim_between_clicks {
                // The rows the second press numbers its word in are the new ones.
                terminal.output(1);
                terminal.trim(10);
                deliver(&mut state, &mut terminal, Delivery::Surface);
            }
            let top = terminal.top();
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
            let (later_reads, _) = if later_motion == "none" {
                (0, Vec::new())
            } else {
                let later = mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 3, 30);
                serve(&mut state, &terminal, later.actions)
            };
            if later_reads != 0 || state.selection.is_some() || state.selection_autoscroll.is_some()
            {
                failures.push(format!(
                    "{case}: extended or autoscrolled after the renumbering: {later_reads} reads, {:?}, autoscroll {}",
                    state.selection.as_ref().map(|s| s.ordered_cells()),
                    state.selection_autoscroll.is_some()
                ));
            }
            let (row, col) = match later_motion {
                "none" => (2, 18),
                _ if back_to_word => (1, 8),
                _ => (3, 30),
            };
            if back_to_word {
                let back = mouse(
                    &mut state,
                    MouseEventKind::Drag(MouseButton::Left),
                    row,
                    col,
                );
                if !back.actions.is_empty() || state.selection.is_some() {
                    failures.push(format!("{case}: the word came back: {:?}", back.actions));
                }
            }
            let release = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), row, col);
            let (reads, copied) = serve(&mut state, &terminal, release.actions);
            if reads != 0 || !copied.is_empty() {
                failures.push(format!("{case}: {reads} reads, copied {copied:?}"));
            }
            if notice(&state) != Some(NOT_COPIED) {
                failures.push(format!("{case}: notice {:?}", notice(&state)));
            }
            if state.selection.is_some() {
                failures.push(format!(
                    "{case}: highlighted over renumbered rows after the release: {:?}",
                    state.selection.as_ref().map(|s| s.ordered_cells())
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A plain double-click, never extended, is not affected: it copies its frozen
/// word, without a highlight, which would sit on renumbered rows. Its short
/// copied highlight likewise goes at once when the rows are renumbered.
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
        if state.selection.is_some() {
            failures.push(format!("{case}: word highlighted over renumbered rows"));
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
        // The copy is done: nothing is left to say "not copied" later, and the
        // highlight's deadline no longer holds patches off the fast path.
        if state.renumbered_selection.is_some()
            || state.selection_highlight_clear_deadline.is_some()
        {
            failures.push(format!(
                "{case}: leftover state after the copied highlight went"
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The no-copy notice is drawn as a failure (the palette's red), never with the
/// green mark of a copy that was made.
#[test]
fn the_no_copy_notice_is_drawn_in_red() {
    let mut terminal = TrimTerminal::new(60, 40);
    let mut state = trim_state(true, &mut terminal);
    mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 0);
    mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 2, 12);
    terminal.trim(10);
    deliver(&mut state, &mut terminal, Delivery::Surface);
    mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 2, 12);
    assert_eq!(notice(&state), Some(NOT_COPIED));
    let frame = state.compose(106, 20).expect("composed frame");
    let buffer = frame.to_ratatui_buffer().expect("buffer");
    let dot = buffer
        .content()
        .iter()
        .position(|cell| cell.symbol() == "●")
        .expect("the notice's mark is drawn");
    let palette = &state.config.palette;
    assert_eq!(buffer.content()[dot].fg, palette.red, "a failure mark");
    assert_ne!(palette.red, palette.green, "the palette tells them apart");
    let text: String = buffer.content()[dot + 2..dot + 2 + NOT_COPIED.len()]
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert_eq!(text, NOT_COPIED);
}

/// Keyboard copy mode numbers rows the same way (measured on the real client:
/// a `v` selection held across a trim copied lines 273 later). A selection in
/// copy mode is dropped at once when its rows are renumbered, and `y` then
/// copies nothing and says so. With output alone `y` copies what was selected.
#[test]
fn copy_mode_selection_renumbered_before_y_copies_nothing_and_says_so() {
    let mut failures = Vec::new();
    for offset in [0, 40] {
        for renumber in [true, false] {
            let case = format!("offset {offset} renumber {renumber}");
            let mut terminal = TrimTerminal::new(60, offset);
            let mut state = trim_state(true, &mut terminal);
            let top = terminal.top();
            let expected = format!("{}\na", terminal.row(top + 1));
            let mut enter = ClientShellInput::default();
            state.record_binding(
                crate::input::KeybindMatch::Action(crate::input::KeybindAction::CopyMode),
                &mut enter,
            );
            for byte in [b"k", b"k", b"v", b"j"] {
                state.handle_input_bytes(byte);
            }
            if state.selection.as_ref().map(|s| s.ordered_cells())
                != Some(((top as u32 + 1, 0), (top as u32 + 2, 0)))
            {
                failures.push(format!(
                    "{case}: copy-mode selection {:?}",
                    state.selection.as_ref().map(|s| s.ordered_cells())
                ));
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
            let copy = state.handle_input_bytes(b"y");
            let (reads, copied) = serve(&mut state, &terminal, copy.actions);
            if renumber {
                if reads != 0 || !copied.is_empty() {
                    failures.push(format!("{case}: {reads} reads, copied {copied:?}"));
                }
                if notice(&state) != Some(NOT_COPIED) {
                    failures.push(format!("{case}: notice {:?}", notice(&state)));
                }
            } else if copied != [expected.clone()] {
                failures.push(format!(
                    "{case}: control copied {copied:?}, want {expected:?}"
                ));
            }
        }
        // After the drop, a new selection the user then cancels is not "trimmed".
        let mut terminal = TrimTerminal::new(60, offset);
        let mut state = trim_state(true, &mut terminal);
        let mut enter = ClientShellInput::default();
        state.record_binding(
            crate::input::KeybindMatch::Action(crate::input::KeybindAction::CopyMode),
            &mut enter,
        );
        state.handle_input_bytes(b"v");
        state.handle_input_bytes(b"k");
        terminal.output(1);
        terminal.trim(10);
        deliver(&mut state, &mut terminal, Delivery::Surface);
        state.handle_input_bytes(b"v");
        key(&mut state, KeyCode::Esc, KeyModifiers::empty());
        let copy = state.handle_input_bytes(b"y");
        let (_, copied) = serve(&mut state, &terminal, copy.actions);
        if !copied.is_empty() || notice(&state).is_some() {
            failures.push(format!(
                "offset {offset}: cancelled new selection: copied {copied:?}, notice {:?}",
                notice(&state)
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The drop is judged between two updates the client sees, not against the
/// history at the press: output first grows the history past where it was,
/// then a trim drops lines. The second update renumbers the rows.
#[test]
fn a_trim_after_the_history_grew_still_renumbers_a_held_drag() {
    for delivery in [Delivery::Surface, Delivery::Patch] {
        let mut terminal = TrimTerminal::new(60, 40);
        let mut state = trim_state(true, &mut terminal);
        mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 0);
        mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 2, 12);
        terminal.output(15);
        deliver(&mut state, &mut terminal, delivery);
        assert!(
            state.selection.is_some(),
            "{delivery:?}: growth renumbers nothing"
        );
        terminal.trim(10);
        deliver(&mut state, &mut terminal, delivery);
        assert!(state.selection.is_none(), "{delivery:?}: dropped at once");
        let release = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 2, 12);
        let (reads, copied) = serve(&mut state, &terminal, release.actions);
        assert_eq!((reads, copied), (0, Vec::<String>::new()), "{delivery:?}");
        assert_eq!(notice(&state), Some(NOT_COPIED), "{delivery:?}");
    }
}

/// A double-click never extended keeps its frozen word across a renumbering. It
/// is decided where it is released (herdr-lead's ruling): on the screen cells
/// the word occupied at the press, the word is exactly what is selected, so it
/// is copied; anywhere else the selection would be rows that name other lines,
/// so nothing is copied and the notice says so. A slip off and back is fine.
/// "On the word" is those press-time screen cells, not the current hit map, so a
/// moment without pane hits does not count as off the word.
#[test]
fn double_click_renumbered_while_held_is_decided_where_it_is_released() {
    let mut failures = Vec::new();
    for offset in [0, 40] {
        for (motion, copies_word, hits_gone) in [
            (&[(2, 18)][..], false, false),
            (&[(2, 18), (1, 8)][..], true, false),
            (&[(1, 10)][..], true, false),
            (&[(1, 10), (1, 30)][..], false, false),
            (&[(1, 5)][..], false, false),
            (&[(1, 13)][..], true, false),
            (&[(1, 10)][..], true, true),
        ] {
            let case = format!("offset {offset} motion {motion:?} hits gone {hits_gone}");
            let mut terminal = TrimTerminal::new(60, offset);
            let mut state = trim_state(true, &mut terminal);
            let word = terminal.row(terminal.top() + 1)[6..14].to_owned();
            mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 8);
            mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 1, 8);
            mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 8);
            terminal.output(1);
            terminal.trim(10);
            deliver(&mut state, &mut terminal, Delivery::Surface);
            let inner = state.hits.panes[0].inner_rect;
            if hits_gone {
                state.hits.panes.clear();
            }
            let send = |state: &mut ClientShellState, kind, row: u16, col: u16| {
                state
                    .handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
                        kind,
                        column: inner.x + col,
                        row: inner.y + row,
                        modifiers: KeyModifiers::empty(),
                    })])
                    .actions
            };
            let mut actions = Vec::new();
            for &(row, col) in motion {
                actions.extend(send(
                    &mut state,
                    MouseEventKind::Drag(MouseButton::Left),
                    row,
                    col,
                ));
            }
            let (row, col) = *motion.last().unwrap();
            actions.extend(send(
                &mut state,
                MouseEventKind::Up(MouseButton::Left),
                row,
                col,
            ));
            let (reads, copied) = serve(&mut state, &terminal, actions);
            let (want_copy, want_notice) = if copies_word {
                (vec![word.clone()], COPIED)
            } else {
                (Vec::new(), NOT_COPIED)
            };
            if reads != 0 || copied != want_copy || notice(&state) != Some(want_notice) {
                failures.push(format!(
                    "{case}: {reads} reads, copied {copied:?} want {want_copy:?}, notice {:?}",
                    notice(&state)
                ));
            }
            if state.selection.is_some() {
                failures.push(format!("{case}: highlighted over renumbered rows"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A frame without scroll metrics says nothing about the history: it is not a
/// renumbering. A held drag and a kept selection across one still copy exactly
/// what was selected.
#[test]
fn a_frame_without_scroll_metrics_is_not_a_renumbering() {
    let mut failures = Vec::new();
    for offset in [0, 40] {
        for copy_on_select in [true, false] {
            let case = format!("offset {offset} copy_on_select {copy_on_select}");
            let mut terminal = TrimTerminal::new(60, offset);
            let mut state = trim_state(copy_on_select, &mut terminal);
            let top = terminal.top();
            let expected = format!(
                "{}\n{}",
                terminal.row(top + 1),
                &terminal.row(top + 2)[..13]
            );
            mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 0);
            mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 2, 12);
            let mut actions = Vec::new();
            if !copy_on_select {
                actions.extend(
                    mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 2, 12).actions,
                );
            }
            terminal.scroll_hidden = true;
            deliver(&mut state, &mut terminal, Delivery::Surface);
            terminal.scroll_hidden = false;
            deliver(&mut state, &mut terminal, Delivery::Surface);
            if state.selection.is_none() {
                failures.push(format!("{case}: selection dropped"));
            }
            let finish = if copy_on_select {
                mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 2, 12)
            } else {
                key(&mut state, KeyCode::Char('c'), KeyModifiers::CONTROL)
            };
            actions.extend(finish.actions);
            let (_, copied) = serve(&mut state, &terminal, actions);
            if copied != [expected.clone()] || notice(&state) != Some(COPIED) {
                failures.push(format!(
                    "{case}: copied {copied:?} want {expected:?}, notice {:?}",
                    notice(&state)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Real Ghostty: a pane that grows taller pulls lines from its history into
/// the view. `max_offset_from_bottom` drops by the growth, but no row is
/// renumbered: the total is unchanged and every absolute row shows the same line.
#[tokio::test(flavor = "current_thread")]
async fn review_a_taller_pane_drops_max_offset_but_renumbers_no_row() {
    let history: String = (0..60).map(|line| format!("L{line:03}\r\n")).collect();
    let runtime = crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
        40,
        6,
        1024 * 1024,
        history.as_bytes(),
    );
    let line_at = |row: usize| {
        let metrics = runtime.scroll_metrics().expect("metrics");
        runtime.set_scroll_offset_from_bottom(metrics.max_offset_from_bottom - row);
        let text = runtime.visible_text();
        runtime.set_scroll_offset_from_bottom(0);
        text.lines().next().unwrap_or_default().trim().to_owned()
    };
    let before = runtime.scroll_metrics().expect("metrics");
    let rows = [0usize, 10, 30, 45];
    let shown_before: Vec<String> = rows.iter().map(|row| line_at(*row)).collect();
    assert!(
        shown_before.iter().all(|line| line.starts_with('L')),
        "the instrument reads lines: {shown_before:?}"
    );
    runtime.resize(11, 40, 0, 0);
    let after = runtime.scroll_metrics().expect("metrics");
    let shown_after: Vec<String> = rows.iter().map(|row| line_at(*row)).collect();
    eprintln!(
        "before {before:?} after {after:?} rows {rows:?} {shown_before:?} -> {shown_after:?}"
    );
    assert_eq!(after.viewport_rows, 11);
    assert!(
        after.max_offset_from_bottom < before.max_offset_from_bottom,
        "max_offset_from_bottom drops: {before:?} -> {after:?}"
    );
    assert_eq!(
        after.max_offset_from_bottom + after.viewport_rows,
        before.max_offset_from_bottom + before.viewport_rows,
        "the total is unchanged"
    );
    assert_eq!(
        shown_after, shown_before,
        "every absolute row shows the same line"
    );
}

/// The pane's terminal grows taller (another client of the same tab took the
/// tab's geometry) while this client's own pane rect stays the same, so the old
/// resize guard does not fire. No row was renumbered: a kept selection must still
/// copy exactly what it selected, with no "not copied" notice.
#[test]
fn review_a_taller_pane_is_not_a_trim_for_a_kept_selection() {
    let mut terminal = TrimTerminal::new(60, 0);
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
    assert!(serve(&mut state, &terminal, release.actions).1.is_empty());
    assert!(state.selection.is_some(), "kept");
    let mut taller = terminal.surface();
    let scroll = taller.panes[0].scroll.as_mut().expect("metrics");
    scroll.viewport_rows += 5;
    scroll.max_offset_from_bottom -= 5;
    state.set_pane_surface(taller);
    state.compose(106, 20).expect("composed frame");
    state.commit_presented_frame();
    let copy = key(&mut state, KeyCode::Char('c'), KeyModifiers::CONTROL);
    let (_, copied) = serve(&mut state, &terminal, copy.actions);
    assert_eq!(
        (copied, notice(&state)),
        (vec![expected], Some(COPIED)),
        "a taller pane renumbers nothing"
    );
}

/// Real Ghostty: the pane's terminal changes WIDTH with lines longer than the
/// new width. Pins the guard's known blind spot: narrowing renumbers rows while
/// the total rises; widening back renumbers them and lowers the total.
#[tokio::test(flavor = "current_thread")]
async fn review_width_reflow_renumbers_rows_and_narrowing_raises_the_total() {
    let history: String = (0..60)
        .map(|line| format!("L{line:03} {}\r\n", "x".repeat(50)))
        .collect();
    let runtime = crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
        80,
        6,
        1024 * 1024,
        history.as_bytes(),
    );
    let line_at = |row: usize| {
        let metrics = runtime.scroll_metrics().expect("metrics");
        runtime.set_scroll_offset_from_bottom(metrics.max_offset_from_bottom.saturating_sub(row));
        let text = runtime.visible_text();
        runtime.set_scroll_offset_from_bottom(0);
        text.lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(4)
            .collect::<String>()
    };
    let rows = [0usize, 10, 30, 50];
    let report = |label: &str| {
        let metrics = runtime.scroll_metrics().expect("metrics");
        let shown: Vec<String> = rows.iter().map(|row| line_at(*row)).collect();
        let total = metrics.max_offset_from_bottom + metrics.viewport_rows;
        eprintln!(
            "{label}: total {total} max {} rows {rows:?} -> {shown:?}",
            metrics.max_offset_from_bottom
        );
        (total, shown)
    };
    let (total80, shown80) = report("80 cols");
    runtime.resize(6, 40, 0, 0);
    let (total40, shown40) = report("40 cols");
    runtime.resize(6, 80, 0, 0);
    let (total80b, shown80b) = report("80 cols again");
    // Narrowing reflows long lines: rows are renumbered while the total RISES,
    // so neither a max-offset nor a total-rows guard can see it (a documented
    // limitation, like a burst larger than a trim). Widening back renumbers
    // again and lowers the total, which the guard does see.
    assert!(
        total40 > total80 && shown40 != shown80,
        "narrowing: {total80}->{total40}, {shown80:?} -> {shown40:?}"
    );
    assert!(
        total80b < total40 && shown80b != shown40,
        "widening: {total40}->{total80b}, {shown40:?} -> {shown80b:?}"
    );
}

/// The same taller pane in copy mode: a `v` selection still copies on `y`.
#[test]
fn a_taller_pane_is_not_a_trim_for_a_copy_mode_selection() {
    for offset in [0, 40] {
        let mut terminal = TrimTerminal::new(60, offset);
        let mut state = trim_state(true, &mut terminal);
        let top = terminal.top();
        let expected = format!("{}\na", terminal.row(top + 1));
        let mut enter = ClientShellInput::default();
        state.record_binding(
            crate::input::KeybindMatch::Action(crate::input::KeybindAction::CopyMode),
            &mut enter,
        );
        for byte in [b"k", b"k", b"v", b"j"] {
            state.handle_input_bytes(byte);
        }
        assert!(state.selection.is_some(), "offset {offset}: selected");
        let mut taller = terminal.surface();
        let scroll = taller.panes[0].scroll.as_mut().expect("metrics");
        scroll.viewport_rows += 5;
        scroll.max_offset_from_bottom -= 5;
        state.set_pane_surface(taller);
        state.compose(106, 20).expect("composed frame");
        state.commit_presented_frame();
        let copy = state.handle_input_bytes(b"y");
        let (_, copied) = serve(&mut state, &terminal, copy.actions);
        assert_eq!(
            (copied, notice(&state)),
            (vec![expected], Some(COPIED)),
            "offset {offset}: a taller pane renumbers nothing"
        );
    }
}
