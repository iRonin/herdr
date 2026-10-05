use super::*;

/// Held second press. Keep only one row read in flight and use the latest
/// pointer position when it returns, so remote latency cannot queue up motion.
#[derive(Debug)]
pub(super) struct ClientWordSelection {
    pub(super) pane_id: String,
    pub(super) focus_confirmed: bool,
    anchor: (u32, u16),
    anchor_bounds: Option<(u16, u16)>,
    cursor: (u32, u16),
    end_col: u16,
    content_revision: Option<u64>,
    cached_row: Option<(u32, String)>,
    pending_row: Option<u32>,
    pub(super) dragged: bool,
    pub(super) released: bool,
}

/// The token a plain double-click copied, where its highlight stands.
#[derive(Debug)]
pub(super) struct ClientCopiedWord {
    pane_id: String,
    row: u32,
    start_col: u16,
    end_col: u16,
    /// The highlighted cells' symbols when the word was copied, one per column.
    shown: Vec<String>,
}

impl ClientCopiedWord {
    /// Records what `surface` shows in the highlighted cells, if it shows them
    /// and its pane is still at `revision`, the revision the word was read at.
    fn new(
        surface: &PaneSurfaceFrame,
        pane_id: String,
        row: u32,
        start_col: u16,
        end_col: u16,
        revision: Option<u64>,
    ) -> Option<Self> {
        let mut word = Self {
            pane_id,
            row,
            start_col,
            end_col,
            shown: Vec::new(),
        };
        let (shown_revision, shown) = word.cells_in(surface)?;
        if Some(shown_revision) != revision {
            return None;
        }
        word.shown = shown;
        Some(word)
    }

    /// The pane's content revision and the highlighted cells' symbols in
    /// `surface`, indexed by column, so no character-width table is involved: a
    /// wide character's continuation cell is a cell like any other. None if the
    /// surface does not show them all.
    fn cells_in(&self, surface: &PaneSurfaceFrame) -> Option<(u64, Vec<String>)> {
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
        let cells = (self.start_col..=self.end_col)
            .map(|col| {
                surface
                    .frame
                    .cells
                    .get(
                        y * usize::from(surface.frame.width)
                            + usize::from(pane.inner_rect.x)
                            + usize::from(col),
                    )
                    .map(|cell| cell.symbol.clone())
            })
            .collect::<Option<Vec<_>>>()?;
        Some((pane.content_revision, cells))
    }

    /// Whether `surface` shows something else in this token's own cells. Cells
    /// the surface does not show are left to the highlight's deadline.
    pub(super) fn replaced_in(&self, surface: &PaneSurfaceFrame) -> bool {
        self.cells_in(surface)
            .is_some_and(|(_, cells)| cells != self.shown)
    }
}

impl ClientShellState {
    pub(super) fn request_word_selection(
        &mut self,
        hit: &PaneHit,
        viewport_row: u16,
        col: u16,
        outcome: &mut ClientShellInput,
    ) {
        let row = crate::selection::absolute_row_for_viewport(viewport_row, hit.scroll);
        self.word_selection_generation = self.word_selection_generation.saturating_add(1);
        self.word_selection_gesture = Some(ClientWordSelection {
            pane_id: hit.pane_id.clone(),
            focus_confirmed: self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.focused_pane_id.as_deref())
                == Some(hit.pane_id.as_str()),
            anchor: (row, col),
            anchor_bounds: None,
            cursor: (row, col),
            end_col: hit.inner_rect.width.saturating_sub(1),
            content_revision: self.pane_surface.as_ref().and_then(|surface| {
                surface
                    .panes
                    .iter()
                    .find(|pane| pane.pane_id == hit.pane_id)
                    .map(|pane| pane.content_revision)
            }),
            cached_row: None,
            pending_row: None,
            dragged: false,
            released: false,
        });
        self.request_word_selection_row(row, outcome);
    }

    fn cancel_word_selection(&mut self) {
        self.word_selection_gesture = None;
        self.selection = None;
        self.stop_selection_autoscroll();
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
            content_revision: gesture.content_revision,
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
        let Some(gesture) = self.word_selection_gesture.as_ref() else {
            return;
        };
        let Some((anchor_start, anchor_end)) = gesture.anchor_bounds else {
            return;
        };
        let Some((_, text)) = gesture
            .cached_row
            .as_ref()
            .filter(|(row, _)| *row == gesture.cursor.0)
        else {
            self.request_word_selection_row(gesture.cursor.0, outcome);
            return;
        };
        let (start_col, end_col) =
            crate::app::actions::word_bounds_at_column(text, gesture.cursor.1)
                .unwrap_or((gesture.cursor.1, gesture.cursor.1));
        let start = (gesture.anchor.0, anchor_start).min((gesture.cursor.0, start_col));
        let end = (gesture.anchor.0, anchor_end).max((gesture.cursor.0, end_col));
        self.selection = Some(crate::selection::Selection::absolute_range(
            gesture.pane_id.clone(),
            start,
            end,
        ));
        if gesture.released {
            // A plain double-click copies the token its own validated row read
            // highlighted. A later read can be refused by output anywhere in the
            // pane, or return whatever those columns hold by then. A pointer that
            // moved but ended inside the clicked word has not extended anything.
            let plain = (start, end)
                == (
                    (gesture.anchor.0, anchor_start),
                    (gesture.anchor.0, anchor_end),
                );
            let copied_word = plain.then(|| {
                (
                    gesture.pane_id.clone(),
                    crate::app::actions::text_in_cell_range(text, start.1, end.1),
                )
            });
            let pressed_revision = gesture.content_revision;
            if let Some(selection) = self.selection.as_mut() {
                selection.finish();
            }
            self.word_selection_gesture = None;
            if self.config.copy_on_select {
                if let Some((pane_id, text)) = copied_word {
                    let now = std::time::Instant::now();
                    outcome
                        .actions
                        .push(ClientShellAction::ClipboardWrite(text.into_bytes()));
                    self.show_copy_feedback(now);
                    self.selection_highlight_clear_deadline =
                        Some(now + std::time::Duration::from_millis(500));
                    // The row was read at the revision on screen at the press. A patch
                    // that arrived before the reply can show newer text without
                    // cancelling the gesture (the patch fast path skips that check), so record the
                    // cells only while the screen still shows that revision; otherwise
                    // nothing on screen is the copied word, so drop the highlight now.
                    self.copied_word_highlight = self.pane_surface.as_ref().and_then(|surface| {
                        ClientCopiedWord::new(
                            surface,
                            pane_id,
                            start.0,
                            start.1,
                            end.1,
                            pressed_revision,
                        )
                    });
                    if self.copied_word_highlight.is_none() {
                        self.selection = None;
                        self.selection_highlight_clear_deadline = None;
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
        if gesture.anchor_bounds.is_none() {
            gesture.anchor_bounds =
                crate::app::actions::word_bounds_at_column(&text, gesture.anchor.1);
            if gesture.anchor_bounds.is_none() {
                self.cancel_word_selection();
                return (true, Vec::new());
            }
        }
        gesture.cached_row = Some((absolute_row, text));
        let mut outcome = ClientShellInput::default();
        self.update_word_selection(&mut outcome);
        (outcome.repaint, outcome.actions)
    }
}
