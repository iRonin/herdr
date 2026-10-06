use super::*;

/// Held second press. The clicked word is resolved at the press from the cells
/// on screen; rows the pointer drags into are read from the endpoint. Keep only
/// one row read in flight and use the latest pointer position when it returns,
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
    cursor: (u32, u16),
    end_col: u16,
    content_revision: u64,
    cached_row: Option<(u32, crate::app::actions::WordRow)>,
    pending_row: Option<u32>,
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
    row_text: crate::app::actions::WordRow,
    content_revision: u64,
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
            cursor: (word.row, col),
            end_col: hit.inner_rect.width.saturating_sub(1),
            content_revision: word.content_revision,
            cached_row: (!word_replaced).then_some((word.row, word.row_text)),
            pending_row: None,
            dragged: false,
            released: false,
        });
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
        let scroll = pane.scroll.map(|metrics| crate::pane::ScrollMetrics {
            offset_from_bottom: usize::try_from(metrics.offset_from_bottom).unwrap_or(usize::MAX),
            max_offset_from_bottom: usize::try_from(metrics.max_offset_from_bottom)
                .unwrap_or(usize::MAX),
            viewport_rows: usize::try_from(metrics.viewport_rows).unwrap_or(usize::MAX),
        });
        let shown_at = Rect::new(
            origin.0.saturating_add(pane.inner_rect.x),
            origin.1.saturating_add(pane.inner_rect.y),
            pane.inner_rect.width,
            pane.inner_rect.height,
        );
        if shown_at != hit.inner_rect
            || scroll != hit.scroll
            || viewport_row >= pane.inner_rect.height
            || col >= pane.inner_rect.width
        {
            return None;
        }
        let y = usize::from(pane.inner_rect.y) + usize::from(viewport_row);
        let symbols = (0..pane.inner_rect.width)
            .map(|x| {
                surface_cell(surface, usize::from(pane.inner_rect.x) + usize::from(x), y)
                    .map(|cell| cell.symbol.as_str())
            })
            .collect::<Option<Vec<_>>>()?;
        let row_text = crate::app::actions::WordRow::from_display_cells(
            symbols
                .iter()
                .map(|symbol| (*symbol, crate::protocol::render_ansi::symbol_width(symbol))),
        );
        let bounds = row_text.bounds_at(col)?;
        Some(PaintedWord {
            row: crate::selection::absolute_row_for_viewport(viewport_row, scroll),
            bounds,
            text: row_text.text_in(bounds.0, bounds.1),
            cells: symbols[usize::from(bounds.0)..=usize::from(bounds.1)]
                .iter()
                .map(|symbol| (*symbol).to_owned())
                .collect(),
            row_text,
            content_revision: pane.content_revision,
        })
    }

    fn cancel_word_selection(&mut self) {
        self.word_selection_gesture = None;
        self.selection = None;
        self.stop_selection_autoscroll();
    }

    /// A new pane surface while a double-click is held. Output elsewhere keeps
    /// the clicked word. If the word's own cells change, stop highlighting it at
    /// once; the release still copies the word that was clicked. Cached row text
    /// from before the output no longer bounds words outside it.
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
            self.cancel_word_selection();
        }
    }

    pub(super) fn drag_word_selection(
        &mut self,
        cursor: (u32, u16),
        outcome: &mut ClientShellInput,
    ) {
        let Some(gesture) = self.word_selection_gesture.as_mut() else {
            return;
        };
        if gesture.released || gesture.cursor == cursor {
            return;
        }
        gesture.cursor = cursor;
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
        let Some(gesture) = self.word_selection_gesture.as_mut() else {
            return;
        };
        let (anchor_start, anchor_end) = gesture.anchor_bounds;
        let word_range = (
            (gesture.anchor.0, anchor_start),
            (gesture.anchor.0, anchor_end),
        );
        // A pointer that moved but stayed inside the clicked word has not extended anything.
        let (start, end) = if gesture.cursor.0 == gesture.anchor.0
            && (anchor_start..=anchor_end).contains(&gesture.cursor.1)
        {
            word_range
        } else {
            let Some((_, text)) = gesture
                .cached_row
                .as_ref()
                .filter(|(row, _)| *row == gesture.cursor.0)
            else {
                let row = gesture.cursor.0;
                self.request_word_selection_row(row, outcome);
                return;
            };
            let (start_col, end_col) = text
                .bounds_at(gesture.cursor.1)
                .unwrap_or((gesture.cursor.1, gesture.cursor.1));
            (
                word_range.0.min((gesture.cursor.0, start_col)),
                word_range.1.max((gesture.cursor.0, end_col)),
            )
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
                self.cancel_word_selection();
                return (true, Vec::new());
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

impl ClientWordSelection {
    /// The range reaches past the clicked word.
    pub(super) fn extended(&self) -> bool {
        self.extended
    }
}
