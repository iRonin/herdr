use super::*;

/// Which pane surface a frame showed, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PaneSurfaceView {
    /// The `pane_surface` state it showed, by `PaintedPanes::surface_seq`.
    seq: u64,
    /// Where the pane surface sat in the frame.
    origin: (u16, u16),
    /// The connection generation the surface came from.
    generation: Option<u64>,
}

/// What a composed frame shows of the panes, until that frame reaches the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ComposedPanes {
    /// A frame without pane cells.
    Nothing,
    Surface(PaneSurfaceView),
}

/// Tracks which pane cells the terminal actually shows. A plain double-click
/// takes its word from there: not from a surface installed but not yet drawn,
/// nor from a frame composed but never written.
#[derive(Debug, Default)]
pub(super) struct PaintedPanes {
    /// Counts changes to `pane_surface`, so a view can name the state it showed.
    surface_seq: u64,
    /// The last composed frame's panes, until that frame is written.
    composed: Option<ComposedPanes>,
    /// A patch applied to `pane_surface` in place: the states before and after it.
    patched: Option<(u64, u64)>,
    /// What the terminal shows, if known.
    painted: Option<PaneSurfaceView>,
    /// The surface the terminal shows, once `pane_surface` has moved past it.
    superseded: Option<(u64, PaneSurfaceFrame)>,
}

impl ClientShellState {
    /// Replaces the pane surface. If the terminal shows the surface replaced, it
    /// is kept until a newer frame reaches the terminal.
    pub(super) fn replace_pane_surface(&mut self, surface: Option<PaneSurfaceFrame>) {
        let previous = std::mem::replace(&mut self.pane_surface, surface);
        let painted = &mut self.painted_panes;
        let previous_seq = painted.surface_seq;
        painted.surface_seq = previous_seq.wrapping_add(1);
        if let Some(previous) = previous {
            if painted.painted.is_some_and(|view| view.seq == previous_seq) {
                painted.superseded = Some((previous_seq, previous));
            }
        }
    }

    /// Records that a patch is being applied to the pane surface in place. The
    /// state it replaces is not kept: the fast path writes the patch at once.
    pub(super) fn note_pane_surface_patched(&mut self) {
        let painted = &mut self.painted_panes;
        let previous_seq = painted.surface_seq;
        painted.surface_seq = previous_seq.wrapping_add(1);
        painted.patched = Some((previous_seq, painted.surface_seq));
    }

    pub(super) fn record_composed_panes(&mut self, composed: Option<(u16, u16)>) {
        self.painted_panes.composed = Some(match composed {
            Some(origin) => ComposedPanes::Surface(PaneSurfaceView {
                seq: self.painted_panes.surface_seq,
                origin,
                generation: self.pane_surface_generation,
            }),
            None => ComposedPanes::Nothing,
        });
    }

    pub(super) fn clear_composed_panes(&mut self) {
        self.painted_panes.composed = None;
    }

    /// The last composed frame reached the terminal.
    pub(crate) fn commit_presented_frame(&mut self) {
        let painted = &mut self.painted_panes;
        painted.patched = None;
        painted.superseded = None;
        painted.painted = match painted.composed.take() {
            Some(ComposedPanes::Surface(view)) if view.seq == painted.surface_seq => Some(view),
            _ => None,
        };
    }

    /// The last fast-path patch reached the terminal.
    pub(crate) fn commit_presented_patch(&mut self) {
        let painted = &mut self.painted_panes;
        painted.superseded = None;
        painted.painted = match (painted.painted, painted.patched.take()) {
            (Some(view), Some((before, after)))
                if view.seq == before && after == painted.surface_seq =>
            {
                Some(PaneSurfaceView { seq: after, ..view })
            }
            _ => None,
        };
    }

    /// What the terminal shows is no longer known, for example after a failed write.
    pub(crate) fn forget_presented_panes(&mut self) {
        let painted = &mut self.painted_panes;
        painted.painted = None;
        painted.superseded = None;
        painted.patched = None;
    }

    /// The pane surface the terminal shows, and where it sits on screen.
    pub(super) fn painted_pane_surface(&self) -> Option<(&PaneSurfaceFrame, (u16, u16))> {
        let painted = &self.painted_panes;
        let view = painted.painted?;
        if view.generation != self.active_snapshot_generation {
            return None;
        }
        let surface = if view.seq == painted.surface_seq {
            self.pane_surface.as_ref()
        } else {
            painted
                .superseded
                .as_ref()
                .filter(|(seq, _)| *seq == view.seq)
                .map(|(_, surface)| surface)
        }?;
        Some((surface, view.origin))
    }
}
