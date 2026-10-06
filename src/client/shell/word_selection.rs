use super::*;

/// Held second press. The clicked word is resolved at the press from the cells
/// on screen, and so are the rows the pointer drags into while they are on
/// screen. Only rows off screen (autoscroll) are read from the endpoint: keep
/// one such read in flight and use the latest pointer position when it returns,
/// so remote latency cannot queue up motion.
#[derive(Debug)]
pub(super) struct ClientWordSelection {
    pub(super) pane_id: String,
    pub(super) focus_confirmed: bool,
    anchor: (u32, u16),
    anchor_bounds: (u16, u16),
    /// The clicked word exactly as the screen showed it at the press.
    word: String,
    /// Its cells as they showed then, to tell whether the screen still shows it.
    word_cells: ClientCopiedWord,
    /// The screen no longer shows the clicked word: do not highlight it.
    word_replaced: bool,
    /// The range reaches past the clicked word.
    extended: bool,
    /// The screen row the word was on at the press, inside the pane.
    press_viewport_row: u16,
    /// The word's screen cells at the press: row and first and last column.
    word_screen_cells: (u16, u16, u16),
    cursor: (u32, u16),
    /// The screen row of the pointer inside the pane.
    cursor_viewport_row: u16,
    end_col: u16,
    content_revision: u64,
    /// Text of an off-screen row, from its read.
    cached_row: Option<(u32, crate::app::actions::WordRow)>,
    pending_row: Option<u32>,
    /// An off-screen row whose read failed or was refused: not extended into,
    /// and not read again while the pointer stays there.
    ignored_row: Option<u32>,
    /// The pane's rows were renumbered (`history_rows`): every row this gesture
    /// holds names another line now. It keeps only its frozen word.
    pub(super) renumbered: bool,
    /// It was renumbered while extended past its word: what was selected is
    /// gone, so the release copies nothing and says so.
    dropped: bool,
    pub(super) dragged: bool,
    pub(super) released: bool,
}

/// A double-clicked token's cells, where its highlight stands.
#[derive(Debug, Clone)]
pub(super) struct ClientCopiedWord {
    pane_id: String,
    row: u32,
    start_col: u16,
    end_col: u16,
    /// The token's cells' symbols when it was clicked, one per column.
    shown: Vec<String>,
}

impl ClientCopiedWord {
    /// The token's cells' symbols in `surface`, indexed by column, so no
    /// character-width table is involved: a wide character's continuation cell
    /// is a cell like any other. None if the surface does not show them all.
    fn cells_in(&self, surface: &PaneSurfaceFrame) -> Option<Vec<String>> {
        let pane = surface
            .panes
            .iter()
            .find(|pane| pane.pane_id == self.pane_id)?;
        let top = pane.scroll.map_or(0, |scroll| {
            scroll
                .max_offset_from_bottom
                .saturating_sub(scroll.offset_from_bottom)
        });
        let viewport_row = u64::from(self.row)
            .checked_sub(top)
            .filter(|row| *row < u64::from(pane.inner_rect.height))?;
        let y = usize::from(pane.inner_rect.y) + viewport_row as usize;
        (self.start_col..=self.end_col)
            .map(|col| {
                surface_cell(
                    surface,
                    usize::from(pane.inner_rect.x) + usize::from(col),
                    y,
                )
                .map(|cell| cell.symbol.clone())
            })
            .collect()
    }

    /// Whether `surface` shows something else in this token's own cells. Cells
    /// the surface does not show are left to the highlight's deadline.
    pub(super) fn replaced_in(&self, surface: &PaneSurfaceFrame) -> bool {
        self.cells_in(surface)
            .is_some_and(|cells| cells != self.shown)
    }
}

fn surface_cell(
    surface: &PaneSurfaceFrame,
    x: usize,
    y: usize,
) -> Option<&crate::protocol::CellData> {
    let width = usize::from(surface.frame.width);
    if x >= width || y >= usize::from(surface.frame.height) {
        return None;
    }
    surface.frame.cells.get(y * width + x)
}

/// The word a double-click landed on, read from the painted cells.
struct PaintedWord {
    row: u32,
    bounds: (u16, u16),
    text: String,
    cells: Vec<String>,
    content_revision: u64,
}

fn scroll_metrics(pane: &crate::protocol::PaneSurfacePane) -> Option<crate::pane::ScrollMetrics> {
    pane.scroll.map(|metrics| crate::pane::ScrollMetrics {
        offset_from_bottom: usize::try_from(metrics.offset_from_bottom).unwrap_or(usize::MAX),
        max_offset_from_bottom: usize::try_from(metrics.max_offset_from_bottom)
            .unwrap_or(usize::MAX),
        viewport_rows: usize::try_from(metrics.viewport_rows).unwrap_or(usize::MAX),
    })
}

/// Every cell symbol of one screen row of a pane, or None if the row is outside
/// the pane or the frame does not carry all its cells.
fn pane_row_symbols<'a>(
    surface: &'a PaneSurfaceFrame,
    pane: &crate::protocol::PaneSurfacePane,
    viewport_row: u16,
) -> Option<Vec<&'a str>> {
    if viewport_row >= pane.inner_rect.height {
        return None;
    }
    let y = usize::from(pane.inner_rect.y) + usize::from(viewport_row);
    (0..pane.inner_rect.width)
        .map(|x| {
            surface_cell(surface, usize::from(pane.inner_rect.x) + usize::from(x), y)
                .map(|cell| cell.symbol.as_str())
        })
        .collect()
}

/// A pane row's text placed in the columns the client draws it across.
fn word_row(symbols: &[&str]) -> crate::app::actions::WordRow {
    crate::app::actions::WordRow::from_display_cells(
        symbols
            .iter()
            .map(|symbol| (*symbol, crate::protocol::render_ansi::symbol_width(symbol))),
    )
}

impl ClientShellState {
    pub(super) fn request_word_selection(
        &mut self,
        hit: &PaneHit,
        viewport_row: u16,
        col: u16,
        outcome: &mut ClientShellInput,
    ) {
        self.word_selection_generation = self.word_selection_generation.saturating_add(1);
        outcome.repaint = true;
        // The word is the one on screen. Without complete painted cells there is
        // nothing safe to select: a live row read could return other text.
        let Some(word) = self.painted_word_at(hit, viewport_row, col) else {
            self.cancel_word_selection();
            return;
        };
        let word_cells = ClientCopiedWord {
            pane_id: hit.pane_id.clone(),
            row: word.row,
            start_col: word.bounds.0,
            end_col: word.bounds.1,
            shown: word.cells,
        };
        // A newer surface the terminal has not drawn yet may already show other text there.
        let word_replaced = self
            .pane_surface
            .as_ref()
            .is_some_and(|surface| word_cells.replaced_in(surface));
        self.word_selection_gesture = Some(ClientWordSelection {
            pane_id: hit.pane_id.clone(),
            focus_confirmed: self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.focused_pane_id.as_deref())
                == Some(hit.pane_id.as_str()),
            anchor: (word.row, col),
            anchor_bounds: word.bounds,
            word: word.text,
            word_cells,
            word_replaced,
            extended: false,
            press_viewport_row: viewport_row,
            word_screen_cells: (
                hit.inner_rect.y.saturating_add(viewport_row),
                hit.inner_rect.x.saturating_add(word.bounds.0),
                hit.inner_rect.x.saturating_add(word.bounds.1),
            ),
            cursor: (word.row, col),
            cursor_viewport_row: viewport_row,
            end_col: hit.inner_rect.width.saturating_sub(1),
            content_revision: word.content_revision,
            cached_row: None,
            pending_row: None,
            ignored_row: None,
            renumbered: false,
            dropped: false,
            dragged: false,
            released: false,
        });
        self.number_selection_rows(hit);
        self.update_word_selection(outcome);
    }

    /// The word under `(viewport_row, col)` of `hit` in the painted cells, if the
    /// terminal shows that pane where `hit` says and every cell of the row is there.
    fn painted_word_at(&self, hit: &PaneHit, viewport_row: u16, col: u16) -> Option<PaintedWord> {
        let (surface, origin) = self.painted_pane_surface()?;
        let pane = surface
            .panes
            .iter()
            .find(|pane| pane.pane_id == hit.pane_id)?;
        let scroll = scroll_metrics(pane);
        let shown_at = Rect::new(
            origin.0.saturating_add(pane.inner_rect.x),
            origin.1.saturating_add(pane.inner_rect.y),
            pane.inner_rect.width,
            pane.inner_rect.height,
        );
        if shown_at != hit.inner_rect || scroll != hit.scroll || col >= pane.inner_rect.width {
            return None;
        }
        let symbols = pane_row_symbols(surface, pane, viewport_row)?;
        let row_text = word_row(&symbols);
        let bounds = row_text.bounds_at(col)?;
        Some(PaintedWord {
            row: crate::selection::absolute_row_for_viewport(viewport_row, scroll),
            bounds,
            text: row_text.text_in(bounds.0, bounds.1),
            cells: symbols[usize::from(bounds.0)..=usize::from(bounds.1)]
                .iter()
                .map(|symbol| (*symbol).to_owned())
                .collect(),
            content_revision: pane.content_revision,
        })
    }

    /// The text of absolute row `row` of a pane as the terminal shows it, if it is on screen.
    fn painted_row_text(&self, pane_id: &str, row: u32) -> Option<crate::app::actions::WordRow> {
        let (surface, _) = self.painted_pane_surface()?;
        let pane = surface.panes.iter().find(|pane| pane.pane_id == pane_id)?;
        let top = pane.scroll.map_or(0, |scroll| {
            scroll
                .max_offset_from_bottom
                .saturating_sub(scroll.offset_from_bottom)
        });
        let viewport_row = u16::try_from(u64::from(row).checked_sub(top)?).ok()?;
        Some(word_row(&pane_row_symbols(surface, pane, viewport_row)?))
    }

    fn cancel_word_selection(&mut self) {
        self.word_selection_gesture = None;
        self.selection = None;
        self.stop_selection_autoscroll();
    }

    /// A new pane surface while a double-click is held. Output elsewhere keeps
    /// the clicked word. If the word's own cells change, stop highlighting it at
    /// once; the release still copies the word that was clicked. Text of an
    /// off-screen row read before the output no longer bounds words in it.
    pub(super) fn word_selection_sees_surface(&mut self, surface: &PaneSurfaceFrame) {
        let Some(gesture) = self.word_selection_gesture.as_mut() else {
            return;
        };
        let revision = surface
            .panes
            .iter()
            .find(|pane| pane.pane_id == gesture.pane_id)
            .map(|pane| pane.content_revision);
        if revision.is_some_and(|revision| revision != gesture.content_revision) {
            gesture.cached_row = None;
        }
        if !gesture.word_replaced && gesture.word_cells.replaced_in(surface) {
            gesture.word_replaced = true;
            if !gesture.extended {
                self.selection = None;
            }
        }
    }

    /// The pane's rows were renumbered while the double-click is held. Its word
    /// and range rows name other lines now, so nothing can be highlighted. A
    /// double-click never extended keeps its frozen word, which the release
    /// copies; one extended past it is dropped: copying the word alone would
    /// hand over less than was selected.
    pub(super) fn word_selection_renumbered(&mut self) {
        let Some(gesture) = self.word_selection_gesture.as_mut() else {
            return;
        };
        gesture.renumbered = true;
        gesture.dropped = gesture.extended;
        gesture.word_replaced = true;
        gesture.extended = false;
        gesture.cached_row = None;
        self.selection = None;
    }

    fn request_word_selection_row(&mut self, row: u32, outcome: &mut ClientShellInput) {
        let Some(gesture) = self.word_selection_gesture.as_mut() else {
            return;
        };
        if gesture.pending_row.is_some() {
            return;
        }
        gesture.pending_row = Some(row);
        let pane_id = gesture.pane_id.clone();
        let params = crate::api::schema::PaneSelectionReadParams {
            pane_id: pane_id.clone(),
            anchor: crate::api::schema::PaneTextPoint { row, col: 0 },
            cursor: crate::api::schema::PaneTextPoint {
                row,
                col: gesture.end_col,
            },
            content_revision: Some(gesture.content_revision),
        };
        if !self.push_endpoint_method_with_kind(
            crate::api::schema::Method::PaneSelectionRead(params),
            PendingEndpointKind::WordSelection {
                pane_id,
                absolute_row: row,
                generation: self.word_selection_generation,
            },
            outcome,
        ) {
            if let Some(gesture) = self.word_selection_gesture.as_mut() {
                gesture.pending_row = None;
                gesture.ignored_row = Some(row);
            }
        }
    }

    /// Motion after the double-click's rows were renumbered: it extends
    /// nothing, so it is consumed. Where the gesture ends is decided at release.
    pub(super) fn drag_renumbered_word(&mut self) -> bool {
        self.word_selection_gesture
            .as_ref()
            .is_some_and(|gesture| gesture.renumbered && !gesture.released)
    }

    /// The release of a double-click whose rows were renumbered while it was
    /// held. On the screen cells the word occupied at the press, the frozen word
    /// is exactly what is selected; anywhere else the selection would be rows
    /// that name other lines, so the gesture is dropped.
    pub(super) fn release_renumbered_word(&mut self, column: u16, row: u16) {
        let Some(gesture) = self.word_selection_gesture.as_mut() else {
            return;
        };
        if !gesture.renumbered {
            return;
        }
        let (word_row, start, end) = gesture.word_screen_cells;
        gesture.dropped |= row != word_row || !(start..=end).contains(&column);
    }

    pub(super) fn drag_word_selection(
        &mut self,
        cursor: (u32, u16),
        viewport_row: u16,
        outcome: &mut ClientShellInput,
    ) {
        let Some(gesture) = self.word_selection_gesture.as_mut() else {
            return;
        };
        if gesture.released
            || (gesture.cursor, gesture.cursor_viewport_row) == (cursor, viewport_row)
        {
            return;
        }
        gesture.cursor = cursor;
        gesture.cursor_viewport_row = viewport_row;
        gesture.dragged = true;
        self.update_word_selection(outcome);
    }

    pub(super) fn finish_word_selection(&mut self, outcome: &mut ClientShellInput) {
        self.stop_selection_autoscroll();
        if let Some(gesture) = self.word_selection_gesture.as_mut() {
            gesture.released = true;
        }
        // A pending row reply will finish the selection if its bounds are not ready yet.
        self.update_word_selection(outcome);
    }

    fn update_word_selection(&mut self, outcome: &mut ClientShellInput) {
        let Some(gesture) = self.word_selection_gesture.as_ref() else {
            return;
        };
        let (anchor_start, anchor_end) = gesture.anchor_bounds;
        let word_range = (
            (gesture.anchor.0, anchor_start),
            (gesture.anchor.0, anchor_end),
        );
        let cursor = gesture.cursor;
        let in_word_columns = (anchor_start..=anchor_end).contains(&cursor.1);
        // A pointer that moved but stayed inside the clicked word has not extended
        // anything: inside its row and columns, or inside the screen cells it
        // occupied at the press, which output may since have scrolled other text into.
        let in_word = in_word_columns
            && (cursor.0 == gesture.anchor.0
                || (gesture.cursor_viewport_row == gesture.press_viewport_row
                    && self.selection_autoscroll.is_none()));
        let range = if in_word || gesture.renumbered {
            Some(word_range)
        } else if let Some(text) = self.painted_row_text(&gesture.pane_id, cursor.0) {
            // A row on screen: bounds from its painted cells, no read.
            Some(extend(word_range, cursor, text.bounds_at(cursor.1)))
        } else if gesture.ignored_row == Some(cursor.0) {
            // Its read failed or was refused: keep only the clicked word.
            Some(word_range)
        } else if let Some((_, text)) = gesture
            .cached_row
            .as_ref()
            .filter(|(row, _)| *row == cursor.0)
        {
            Some(extend(word_range, cursor, text.bounds_at(cursor.1)))
        } else {
            None
        };
        let Some((start, end)) = range else {
            self.request_word_selection_row(cursor.0, outcome);
            if self
                .word_selection_gesture
                .as_ref()
                .is_some_and(|gesture| gesture.ignored_row == Some(cursor.0))
            {
                self.update_word_selection(outcome);
            }
            return;
        };
        let Some(gesture) = self.word_selection_gesture.as_mut() else {
            return;
        };
        gesture.extended = (start, end) != word_range;
        let hidden = gesture.word_replaced && !gesture.extended;
        self.selection = (!hidden).then(|| {
            crate::selection::Selection::absolute_range(gesture.pane_id.clone(), start, end)
        });
        if gesture.released {
            let Some(gesture) = self.word_selection_gesture.take() else {
                return;
            };
            if gesture.dropped {
                self.selection = None;
                self.show_copy_failed_feedback(std::time::Instant::now());
                outcome.repaint = true;
                return;
            }
            if let Some(selection) = self.selection.as_mut() {
                selection.finish();
            }
            if self.config.copy_on_select {
                if !gesture.extended {
                    // A plain double-click copies the word it landed on, exactly as
                    // the screen showed it then. A row read could be refused by
                    // output anywhere in the pane, or return text that replaced it.
                    let now = std::time::Instant::now();
                    outcome
                        .actions
                        .push(ClientShellAction::ClipboardWrite(gesture.word.into_bytes()));
                    self.show_copy_feedback(now);
                    // Highlight it only while the screen still shows it.
                    if !gesture.word_replaced {
                        self.selection_highlight_clear_deadline =
                            Some(now + std::time::Duration::from_millis(500));
                        self.copied_word_highlight = Some(gesture.word_cells);
                    } else {
                        self.selection = None;
                        self.selection_highlight_clear_deadline = None;
                        self.copied_word_highlight = None;
                    }
                } else {
                    // Extended whole-word selections copy a live range, like a plain drag.
                    self.request_selection_copy(outcome, true);
                    self.selection = None;
                }
            }
        }
        outcome.repaint = true;
    }

    pub(super) fn complete_word_selection_row(
        &mut self,
        pane_id: String,
        absolute_row: u32,
        generation: u64,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        if self.word_selection_generation != generation
            || self.word_selection_gesture.as_ref().is_none_or(|gesture| {
                gesture.pane_id != pane_id || gesture.pending_row != Some(absolute_row)
            })
        {
            return (false, Vec::new());
        }
        if self
            .snapshot
            .as_deref()
            .is_none_or(|snapshot| !snapshot.panes.iter().any(|pane| pane.pane_id == pane_id))
        {
            self.cancel_word_selection();
            return (true, Vec::new());
        }
        let text = match result {
            Ok(crate::api::schema::ResponseResult::PaneSelection {
                pane_id: returned_pane_id,
                text,
            }) if returned_pane_id == pane_id => text,
            other => {
                if matches!(other, Ok(value) if !matches!(value, crate::api::schema::ResponseResult::PaneSelection { .. }))
                {
                    self.set_endpoint_error(
                        "endpoint returned an unexpected word-selection result",
                    );
                }
                // A failed or refused read drops only the extension into that row:
                // the clicked word stands, and a waiting release copies it.
                if let Some(gesture) = self.word_selection_gesture.as_mut() {
                    gesture.pending_row = None;
                    gesture.ignored_row = Some(absolute_row);
                }
                let mut outcome = ClientShellInput::default();
                self.update_word_selection(&mut outcome);
                return (true, outcome.actions);
            }
        };
        let Some(gesture) = self.word_selection_gesture.as_mut() else {
            return (false, Vec::new());
        };
        gesture.pending_row = None;
        gesture.cached_row = Some((absolute_row, crate::app::actions::WordRow::from_text(&text)));
        let mut outcome = ClientShellInput::default();
        self.update_word_selection(&mut outcome);
        (outcome.repaint, outcome.actions)
    }
}

/// The range from the clicked word to the token under the cursor, or the
/// cursor's own cell where there is none.
fn extend(
    word_range: ((u32, u16), (u32, u16)),
    cursor: (u32, u16),
    bounds: Option<(u16, u16)>,
) -> ((u32, u16), (u32, u16)) {
    let (start_col, end_col) = bounds.unwrap_or((cursor.1, cursor.1));
    (
        word_range.0.min((cursor.0, start_col)),
        word_range.1.max((cursor.0, end_col)),
    )
}
