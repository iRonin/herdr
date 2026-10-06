use super::*;

/// The numbering a mouse selection's rows were taken in. Rows count from the
/// top of the pane's retained history (`crate::selection`), so when a full
/// scrollback drops its oldest lines, or the history is cleared, every row
/// number names a later line. The client sees that as `max_offset_from_bottom`
/// falling between two updates. Output larger than the dropped lines between two
/// updates hides the drop (a known limitation).
#[derive(Debug)]
pub(super) struct SelectionRows {
    pane_id: String,
    /// The pane's `max_offset_from_bottom` in the last update seen.
    max_offset_from_bottom: usize,
}

/// A plain selection dropped because its rows were renumbered, until the gesture
/// that would have copied it ends: its release, or the copy key for a kept one.
#[derive(Debug)]
pub(super) struct RenumberedSelection {
    /// The pointer dragged: the release would have copied something.
    dragged: bool,
    /// The selection was finished and kept for the copy key (`ui.copy_on_select` off).
    kept: bool,
}

fn max_offset(scroll: Option<crate::pane::ScrollMetrics>) -> usize {
    scroll.map_or(0, |scroll| scroll.max_offset_from_bottom)
}

impl ClientShellState {
    /// A mouse selection or double-click on `hit` starts: its rows are numbered
    /// by `hit`'s scroll metrics.
    pub(super) fn number_selection_rows(&mut self, hit: &PaneHit) {
        self.renumbered_selection = None;
        self.selection_rows = Some(SelectionRows {
            pane_id: hit.pane_id.clone(),
            max_offset_from_bottom: max_offset(hit.scroll),
        });
    }

    /// A new pane surface: if the selection's pane history lost rows, the
    /// selection's rows name other lines now.
    pub(super) fn selection_sees_history(&mut self, surface: &PaneSurfaceFrame) {
        let Some(rows) = self.selection_rows.as_mut() else {
            return;
        };
        let Some(pane) = surface
            .panes
            .iter()
            .find(|pane| pane.pane_id == rows.pane_id)
        else {
            return;
        };
        let max = pane.scroll.map_or(0, |scroll| {
            usize::try_from(scroll.max_offset_from_bottom).unwrap_or(usize::MAX)
        });
        if max >= rows.max_offset_from_bottom {
            rows.max_offset_from_bottom = max;
            return;
        }
        self.selection_rows = None;
        self.stop_selection_autoscroll();
        if self.word_selection_gesture.is_some() {
            self.word_selection_renumbered();
            return;
        }
        let Some(selection) = self.selection.take() else {
            return;
        };
        if self.selection_highlight_clear_deadline.take().is_some() {
            // A copied word's short highlight: the copy is done.
            self.copied_word_highlight = None;
            return;
        }
        self.renumbered_selection = Some(RenumberedSelection {
            dragged: selection.is_dragging(),
            kept: selection.is_visible() && !selection.is_dragging(),
        });
    }

    /// The pointer moves while a renumbered plain drag is held: consume it.
    pub(super) fn drag_renumbered_selection(&mut self) -> bool {
        match self.renumbered_selection.as_mut() {
            Some(renumbered) if !renumbered.kept => {
                renumbered.dragged = true;
                true
            }
            _ => false,
        }
    }

    /// The release of a renumbered plain drag: nothing to copy. Say so if the
    /// pointer dragged; a press released without moving was a plain click.
    pub(super) fn release_renumbered_selection(&mut self, outcome: &mut ClientShellInput) -> bool {
        if self
            .renumbered_selection
            .as_ref()
            .is_none_or(|renumbered| renumbered.kept)
        {
            return false;
        }
        if self
            .renumbered_selection
            .take()
            .is_some_and(|renumbered| renumbered.dragged)
        {
            self.show_copy_failed_feedback(std::time::Instant::now());
        }
        outcome.repaint = true;
        true
    }

    /// The copy key after a kept selection was renumbered: nothing to copy, and
    /// the key is not passed to the program. Any other key or click forgets it.
    pub(super) fn copy_renumbered_selection(&mut self, outcome: &mut ClientShellInput) -> bool {
        if !self
            .renumbered_selection
            .take()
            .is_some_and(|renumbered| renumbered.kept)
        {
            return false;
        }
        self.show_copy_failed_feedback(std::time::Instant::now());
        outcome.repaint = true;
        true
    }

    pub(super) fn has_kept_renumbered_selection(&self) -> bool {
        self.renumbered_selection
            .as_ref()
            .is_some_and(|renumbered| renumbered.kept)
    }
}
