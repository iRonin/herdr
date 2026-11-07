use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::api::schema::{EventData, EventEnvelope, EventKind};

/// The longest a held connection thread blocks on the hub before it looks at its socket (a closed
/// peer) and the server's `running` flag again. Nothing else wakes an idle connection, so this is
/// the idle cost of every held connection (one wake-up per interval) and the time a closed peer
/// goes unnoticed. Stopping the server does not wait for it: `wake_waiters` releases every
/// connection at once. A connection that samples live state (the agent-status snapshot, output and
/// scroll) wakes sooner, for its own sample, whatever this is.
const SUBSCRIPTION_LIVENESS_INTERVAL: Duration = Duration::from_secs(5);
/// How long a wait sleeps instead when the history lock is poisoned and cannot be waited on.
const POISONED_HISTORY_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How often a `pane.agent_status_changed` subscription asks the app for a pane snapshot while no
/// event wakes it. The snapshot is a safety net: every known change to the status, title,
/// display agent or state labels already emits an event of its own.
const AGENT_STATUS_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);

/// How long the threads that wait on a hub may block, and how often they sample live state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WaitTiming {
    pub(super) liveness: Duration,
    pub(super) agent_status_snapshot: Duration,
}

impl Default for WaitTiming {
    fn default() -> Self {
        Self {
            liveness: SUBSCRIPTION_LIVENESS_INTERVAL,
            agent_status_snapshot: AGENT_STATUS_SNAPSHOT_INTERVAL,
        }
    }
}

/// Where a waiting thread stands: the newest event it had seen and the wake generation, both read
/// at the start of a pass (see `EventWaiter::cursor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WaitCursor {
    sequence: u64,
    generation: u64,
}

/// What a waiting thread asks the hub to wake it for. A waiter holds a list of these and is woken
/// by an event that matches ANY of them; an empty list is never woken by an event (its thread
/// still comes back when its own timeout runs out, and when the server stops).
///
/// An interest says which events can change what the thread does, nothing more: it must never be
/// narrower than what the thread consumes (an event the thread would act on but the interest
/// misses is a delivery that waits for the liveness timeout), and it may be wider (a spurious wake
/// only costs one pass). `subscriptions.rs` builds the interests of each subscription kind next to
/// the code that consumes the events, and a test pins that every event a subscription reacts to
/// matches its interests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WakeInterest {
    kind: Option<EventKind>,
    pane: Option<String>,
}

impl WakeInterest {
    /// Every event.
    #[cfg(test)]
    pub(super) fn any() -> Self {
        Self {
            kind: None,
            pane: None,
        }
    }

    /// Events of this kind, whichever pane (or none) they are about.
    pub(super) fn kind(kind: EventKind) -> Self {
        Self {
            kind: Some(kind),
            pane: None,
        }
    }

    /// Events of any kind that are about this pane (see `EventScope`).
    pub(super) fn pane(pane_id: impl Into<String>) -> Self {
        Self {
            kind: None,
            pane: Some(pane_id.into()),
        }
    }

    /// Events of this kind that are about this pane.
    pub(super) fn pane_kind(pane_id: impl Into<String>, kind: EventKind) -> Self {
        Self {
            kind: Some(kind),
            pane: Some(pane_id.into()),
        }
    }

    fn matches(&self, event: &EventEnvelope, scope: &EventScope<'_>) -> bool {
        if self.kind.is_some_and(|kind| kind != event.event) {
            return false;
        }
        match &self.pane {
            None => true,
            Some(pane_id) => scope.may_concern(pane_id),
        }
    }
}

/// Whether an event wakes a waiter with these interests: any matching interest, or an event whose
/// panes cannot be told (`EventScope::Unknown`), which wakes everyone.
fn interests_wake_on(
    interests: &[WakeInterest],
    event: &EventEnvelope,
    scope: &EventScope<'_>,
) -> bool {
    matches!(scope, EventScope::Unknown)
        || interests
            .iter()
            .any(|interest| interest.matches(event, scope))
}

/// Whether the hub would wake a waiter with these interests for this event (for tests of the
/// interests a subscription kind declares).
#[cfg(test)]
pub(super) fn would_wake(interests: &[WakeInterest], event: &EventEnvelope) -> bool {
    interests_wake_on(interests, event, &EventScope::of(&event.data))
}

/// Which panes an event is about, as far as waking waiters goes.
enum EventScope<'a> {
    /// The event is about exactly these panes, by the ids it carries (none: about no pane).
    Panes(Vec<&'a str>),
    /// The event removes, moves or renumbers panes without naming them, so any pane-scoped waiter
    /// may be affected. Fail toward waking: it wakes every waiter.
    Unknown,
}

impl<'a> EventScope<'a> {
    /// The scope of an event. The match names every variant on purpose: a new event must be
    /// classified here before the crate builds, not fall into a default.
    fn of(data: &'a EventData) -> Self {
        match data {
            EventData::PaneCreated { pane } | EventData::PaneUpdated { pane } => {
                Self::Panes(vec![pane.pane_id.as_str()])
            }
            EventData::PaneClosed { pane_id, .. }
            | EventData::PaneFocused { pane_id, .. }
            | EventData::PaneOutputChanged { pane_id, .. }
            | EventData::PaneExited { pane_id, .. }
            | EventData::PaneAgentDetected { pane_id, .. }
            | EventData::PaneAgentStatusChanged { pane_id, .. } => Self::Panes(vec![pane_id]),
            // A moved pane gets a new id: the event is about the old one and the new one.
            EventData::PaneMoved {
                previous_pane_id,
                pane,
                ..
            } => Self::Panes(vec![previous_pane_id, pane.pane_id.as_str()]),
            EventData::PaneReadStateChanged { read_state } => {
                Self::Panes(vec![read_state.pane_id.as_str()])
            }
            EventData::LayoutUpdated { layout } => Self::Panes(
                layout
                    .panes
                    .iter()
                    .map(|pane| pane.pane_id.as_str())
                    .collect(),
            ),
            // These events leave every pane as it was.
            EventData::WorkspaceCreated { .. }
            | EventData::WorkspaceUpdated { .. }
            | EventData::WorkspaceMetadataUpdated { .. }
            | EventData::WorkspaceRenamed { .. }
            | EventData::WorkspaceMoved { .. }
            | EventData::WorkspaceReordered { .. }
            | EventData::WorkspaceFocused { .. }
            | EventData::WorktreeCreated { .. }
            | EventData::WorktreeOpened { .. }
            | EventData::TabCreated { .. }
            | EventData::TabRenamed { .. }
            | EventData::TabFocused { .. } => Self::Panes(Vec::new()),
            // A tab or workspace that closes takes its panes along (no `pane.closed` for them), and
            // a tab that moves to another workspace renames them (no `pane.moved`). The events
            // carry no pane list.
            EventData::TabClosed { .. }
            | EventData::TabMoved { .. }
            | EventData::WorkspaceClosed { .. }
            | EventData::WorktreeRemoved { .. } => Self::Unknown,
        }
    }

    fn may_concern(&self, pane_id: &str) -> bool {
        match self {
            Self::Panes(ids) => ids.contains(&pane_id),
            Self::Unknown => true,
        }
    }
}

#[derive(Clone, Default)]
pub struct EventHub {
    inner: Arc<EventHubShared>,
}

#[derive(Default)]
struct EventHubShared {
    state: Mutex<EventHubState>,
    timing: WaitTiming,
    #[cfg(test)]
    history_reads: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct EventHubState {
    next_sequence: u64,
    events: Vec<(u64, crate::api::schema::EventEnvelope)>,
    /// Bumped by `wake_waiters`: a waiter that saw an older value on entry returns.
    wake_generation: u64,
    /// The threads blocked, or about to block, on the hub. Changed only through `EventHub::waiter`
    /// and the drop of its guard.
    waiters: Vec<Arc<Waiter>>,
}

/// One registered waiting thread. Its own condition variable lets `push` wake exactly this thread;
/// every condition variable is used with the hub's one mutex.
struct Waiter {
    interests: Vec<WakeInterest>,
    wake: Condvar,
    /// The sequence of the event that last woke it. Read and written with the hub mutex held: the
    /// thread's wait ends once this is newer than its cursor.
    woken_at: AtomicU64,
    /// The sequence of the cursor of the thread's current pass, set with the hub mutex held.
    cursor_sequence: AtomicU64,
    /// How many notifications `push` and `wake_waiters` have sent it.
    #[cfg(test)]
    wakes: AtomicU64,
}

/// A waiter whose cursor is this many events behind the newest one is woken whatever the event
/// was, so that it reads the history and moves on before the retained history (`MAX_EVENTS`) drops
/// events it has not seen: a subscription connection reads a history that holds events of every
/// kind, and one that has fallen out of it is closed with `events_lost`. Half the history leaves
/// the thread the other half to be scheduled in.
const LAG_WAKE_EVENTS: u64 = EventHub::MAX_EVENTS as u64 / 2;

impl EventHubState {
    /// Marks the waiters that `event` (just given `sequence`) must wake and returns them: the
    /// ones with a matching interest, every one when the event's panes are unknown, and the ones
    /// that have fallen `LAG_WAKE_EVENTS` behind. A waiter that already has a wake pending for
    /// its current pass is left alone: its next wait returns at once and reads this event too.
    fn wake_interested(&self, event: &EventEnvelope, sequence: u64) -> Vec<Arc<Waiter>> {
        if self.waiters.is_empty() {
            return Vec::new();
        }
        let scope = EventScope::of(&event.data);
        let mut woken = Vec::new();
        for waiter in &self.waiters {
            let cursor = waiter.cursor_sequence.load(Ordering::Relaxed);
            if waiter.woken_at.load(Ordering::Relaxed) > cursor {
                continue;
            }
            let lagging = sequence.saturating_sub(cursor) >= LAG_WAKE_EVENTS;
            if lagging || interests_wake_on(&waiter.interests, event, &scope) {
                waiter.woken_at.store(sequence, Ordering::Relaxed);
                woken.push(Arc::clone(waiter));
            }
        }
        woken
    }
}

impl Waiter {
    fn notify(&self) {
        #[cfg(test)]
        self.wakes.fetch_add(1, Ordering::Relaxed);
        self.wake.notify_one();
    }

    fn woken_after(&self, sequence: u64) -> bool {
        self.woken_at.load(Ordering::Relaxed) > sequence
    }
}

/// A thread registered to wait on the hub (see `EventHub::waiter`). One pass of its loop is
/// `cursor`, the work, then `wait_after` with that cursor.
pub(super) struct EventWaiter {
    hub: EventHub,
    shared: Arc<Waiter>,
}

impl EventWaiter {
    /// Reads where a pass starts. Take it FIRST in the pass, before the `running` check and the
    /// history read, then wait on it: a matching event pushed, or a `wake_waiters`, at any point
    /// after this call makes `wait_after` return at once, so neither can be lost between the pass
    /// and the wait.
    pub(super) fn cursor(&self) -> WaitCursor {
        let Ok(state) = self.hub.inner.state.lock() else {
            return WaitCursor {
                sequence: 0,
                generation: 0,
            };
        };
        self.shared
            .cursor_sequence
            .store(state.next_sequence, Ordering::Relaxed);
        WaitCursor {
            sequence: state.next_sequence,
            generation: state.wake_generation,
        }
    }

    /// Blocks until an event this waiter is interested in has been pushed after the cursor,
    /// `wake_waiters` has been called since the cursor was taken, or `timeout` has passed; returns
    /// whether an event woke it. A poisoned history cannot be waited on: this sleeps a short
    /// interval (the old polling cadence, never a hot loop) and returns `true`, so the caller's
    /// next read reports it as it always did.
    pub(super) fn wait_after(&self, cursor: WaitCursor, timeout: Duration) -> bool {
        let Ok(state) = self.hub.inner.state.lock() else {
            std::thread::sleep(timeout.min(POISONED_HISTORY_POLL_INTERVAL));
            return true;
        };
        match self
            .shared
            .wake
            .wait_timeout_while(state, timeout, |state| {
                !self.shared.woken_after(cursor.sequence)
                    && state.wake_generation == cursor.generation
            }) {
            Ok(_) => self.shared.woken_after(cursor.sequence),
            Err(_) => {
                std::thread::sleep(timeout.min(POISONED_HISTORY_POLL_INTERVAL));
                true
            }
        }
    }

    /// How many times the hub has notified this waiter.
    #[cfg(test)]
    pub(super) fn wake_count(&self) -> u64 {
        self.shared.wakes.load(Ordering::Relaxed)
    }

    /// How many interests the hub scans for this waiter on every push.
    #[cfg(test)]
    pub(super) fn interest_count(&self) -> usize {
        self.shared.interests.len()
    }
}

impl Drop for EventWaiter {
    fn drop(&mut self) {
        if let Ok(mut state) = self.hub.inner.state.lock() {
            state
                .waiters
                .retain(|waiter| !Arc::ptr_eq(waiter, &self.shared));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EventHistoryError {
    Lost,
    Unavailable,
}

impl EventHub {
    const MAX_EVENTS: usize = 512;

    pub fn push(&self, event: crate::api::schema::EventEnvelope) {
        let Ok(mut state) = self.inner.state.lock() else {
            return;
        };
        state.next_sequence += 1;
        let sequence = state.next_sequence;
        let woken = state.wake_interested(&event, sequence);
        state.events.push((sequence, event));
        let overflow = state.events.len().saturating_sub(Self::MAX_EVENTS);
        if overflow > 0 {
            state.events.drain(0..overflow);
        }
        drop(state);
        // After the unlock, so a woken thread does not run into the mutex this thread still holds.
        for waiter in woken {
            waiter.notify();
        }
    }

    pub fn events_after(&self, sequence: u64) -> Vec<(u64, crate::api::schema::EventEnvelope)> {
        #[cfg(test)]
        self.inner
            .history_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Ok(state) = self.inner.state.lock() else {
            return Vec::new();
        };
        state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect()
    }

    pub(super) fn events_after_checked(
        &self,
        sequence: u64,
    ) -> Result<Vec<(u64, crate::api::schema::EventEnvelope)>, EventHistoryError> {
        #[cfg(test)]
        self.inner
            .history_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| EventHistoryError::Unavailable)?;
        if state
            .events
            .first()
            .is_some_and(|(first, _)| sequence < first.saturating_sub(1))
        {
            return Err(EventHistoryError::Lost);
        }
        Ok(state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect())
    }

    pub fn current_sequence(&self) -> u64 {
        let Ok(state) = self.inner.state.lock() else {
            return 0;
        };
        state.next_sequence
    }

    pub(super) fn timing(&self) -> WaitTiming {
        self.inner.timing
    }

    /// Registers a thread that will wait on the hub, to be woken by events that match `interests`
    /// (see `WakeInterest`), by `wake_waiters`, and by a backlog of events it has not looked at
    /// (see `LAG_WAKE_EVENTS`). Register BEFORE the first `EventWaiter::cursor`; the registration
    /// ends when the returned guard is dropped.
    ///
    /// Equal interests are kept once: every `push` scans every interest of every waiter with the
    /// hub mutex held, on the application's thread, and one request can carry tens of thousands of
    /// identical subscriptions. What is left is at most one interest per event kind plus one or two
    /// per distinct pane.
    pub(super) fn waiter(&self, interests: Vec<WakeInterest>) -> EventWaiter {
        let mut seen = std::collections::HashSet::new();
        let interests: Vec<WakeInterest> = interests
            .into_iter()
            .filter(|interest| {
                seen.insert((
                    interest.kind.map(|kind| std::mem::discriminant(&kind)),
                    interest.pane.clone(),
                ))
            })
            .collect();
        let shared = Arc::new(Waiter {
            interests,
            wake: Condvar::new(),
            woken_at: AtomicU64::new(0),
            cursor_sequence: AtomicU64::new(0),
            #[cfg(test)]
            wakes: AtomicU64::new(0),
        });
        if let Ok(mut state) = self.inner.state.lock() {
            shared
                .cursor_sequence
                .store(state.next_sequence, Ordering::Relaxed);
            state.waiters.push(Arc::clone(&shared));
        }
        EventWaiter {
            hub: self.clone(),
            shared,
        }
    }

    /// Releases every waiter that took its cursor before this call and has not yet been woken, so
    /// a thread that must stop (the server is shutting down) notices at once. Set the stop flag
    /// BEFORE calling this: a thread that takes its cursor afterwards sees the flag instead.
    pub(super) fn wake_waiters(&self) {
        let waiters = match self.inner.state.lock() {
            Ok(mut state) => {
                state.wake_generation += 1;
                state.waiters.clone()
            }
            Err(_) => Vec::new(),
        };
        for waiter in waiters {
            waiter.notify();
        }
    }

    #[cfg(test)]
    pub(super) fn with_timing(timing: WaitTiming) -> Self {
        Self {
            inner: Arc::new(EventHubShared {
                timing,
                ..EventHubShared::default()
            }),
        }
    }

    /// How many times the history was read: a test sees how often a subscriber looks at it.
    #[cfg(test)]
    pub(super) fn history_reads(&self) -> u64 {
        self.inner
            .history_reads
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Events of every kind, for tests of what wakes what. `event` names every `EventKind` on purpose:
/// a new kind must get a sample here before the crate's tests build.
#[cfg(test)]
pub(super) mod samples {
    use crate::api::schema::{
        AgentStatus, EventData, EventEnvelope, EventKind, PaneInfo, PaneLayoutPane, PaneLayoutRect,
        PaneLayoutSnapshot, PaneReadState, TabInfo, WorkspaceInfo, WorktreeInfo,
    };

    pub(in crate::api) fn pane_info(pane_id: &str) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.into(),
            terminal_id: "term_1".into(),
            workspace_id: "workspace_1".into(),
            tab_id: "tab_1".into(),
            focused: true,
            cwd: None,
            foreground_cwd: None,
            restore_error: None,
            label: None,
            agent: Some("pi".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status: AgentStatus::Working,
            state_labels: Default::default(),
            tokens: Default::default(),
            agent_session: None,
            scroll: None,
            revision: 0,
        }
    }

    fn workspace_info() -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: "workspace_1".into(),
            number: 1,
            label: "one".into(),
            focused: true,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: "tab_1".into(),
            agent_status: AgentStatus::Idle,
            tokens: Default::default(),
            worktree: None,
        }
    }

    fn tab_info() -> TabInfo {
        TabInfo {
            tab_id: "tab_1".into(),
            workspace_id: "workspace_1".into(),
            number: 1,
            label: "one".into(),
            focused: true,
            pane_count: 1,
            agent_status: AgentStatus::Idle,
        }
    }

    fn worktree_info() -> WorktreeInfo {
        WorktreeInfo {
            path: "/tmp/wt".into(),
            branch: None,
            is_bare: false,
            is_detached: false,
            is_prunable: false,
            is_linked_worktree: true,
            open_workspace_id: None,
            label: "wt".into(),
        }
    }

    /// The panes `event` is about are `pane_id` and, for a move, its new id `moved_<pane_id>`; the
    /// events that name no pane use ids that are not `pane_id`.
    pub(in crate::api) fn event(kind: EventKind, pane_id: &str) -> EventEnvelope {
        let workspace_id = || "workspace_1".to_string();
        let tab_id = || "tab_1".to_string();
        let data = match kind {
            EventKind::WorkspaceCreated => EventData::WorkspaceCreated {
                workspace: workspace_info(),
            },
            EventKind::WorkspaceUpdated => EventData::WorkspaceUpdated {
                workspace: workspace_info(),
            },
            EventKind::WorkspaceMetadataUpdated => EventData::WorkspaceMetadataUpdated {
                workspace: workspace_info(),
            },
            EventKind::WorkspaceClosed => EventData::WorkspaceClosed {
                workspace_id: workspace_id(),
                workspace: None,
            },
            EventKind::WorkspaceRenamed => EventData::WorkspaceRenamed {
                workspace_id: workspace_id(),
                label: "renamed".into(),
            },
            EventKind::WorkspaceMoved => EventData::WorkspaceMoved {
                workspace_id: workspace_id(),
                insert_index: 0,
                workspaces: Vec::new(),
            },
            EventKind::WorkspaceReordered => EventData::WorkspaceReordered {
                workspace_ids: vec![workspace_id()],
                before_workspace_id: None,
                workspaces: Vec::new(),
            },
            EventKind::WorkspaceFocused => EventData::WorkspaceFocused {
                workspace_id: workspace_id(),
            },
            EventKind::WorktreeCreated => EventData::WorktreeCreated {
                workspace: workspace_info(),
                worktree: worktree_info(),
            },
            EventKind::WorktreeOpened => EventData::WorktreeOpened {
                workspace: workspace_info(),
                worktree: worktree_info(),
                already_open: false,
            },
            EventKind::WorktreeRemoved => EventData::WorktreeRemoved {
                workspace_id: workspace_id(),
                workspace: None,
                worktree: worktree_info(),
                forced: false,
            },
            EventKind::TabCreated => EventData::TabCreated { tab: tab_info() },
            EventKind::TabClosed => EventData::TabClosed {
                tab_id: tab_id(),
                workspace_id: workspace_id(),
            },
            EventKind::TabRenamed => EventData::TabRenamed {
                tab_id: tab_id(),
                workspace_id: workspace_id(),
                label: "renamed".into(),
            },
            EventKind::TabMoved => EventData::TabMoved {
                tab_id: "tab_3".into(),
                workspace_id: "workspace_2".into(),
                insert_index: 0,
                tabs: Vec::new(),
                previous_tab_id: Some(tab_id()),
                previous_workspace_id: Some(workspace_id()),
            },
            EventKind::TabFocused => EventData::TabFocused {
                tab_id: tab_id(),
                workspace_id: workspace_id(),
            },
            EventKind::PaneCreated => EventData::PaneCreated {
                pane: pane_info(pane_id),
            },
            EventKind::PaneClosed => EventData::PaneClosed {
                pane_id: pane_id.into(),
                workspace_id: workspace_id(),
            },
            EventKind::PaneUpdated => EventData::PaneUpdated {
                pane: pane_info(pane_id),
            },
            EventKind::PaneFocused => EventData::PaneFocused {
                pane_id: pane_id.into(),
                workspace_id: workspace_id(),
            },
            EventKind::PaneMoved => EventData::PaneMoved {
                previous_pane_id: pane_id.into(),
                previous_workspace_id: workspace_id(),
                previous_tab_id: tab_id(),
                pane: Box::new(pane_info(&format!("moved_{pane_id}"))),
                created_workspace: None,
                created_tab: None,
                closed_workspace_id: None,
                closed_tab_id: None,
            },
            EventKind::PaneOutputChanged => EventData::PaneOutputChanged {
                pane_id: pane_id.into(),
                workspace_id: workspace_id(),
                revision: 1,
            },
            EventKind::PaneExited => EventData::PaneExited {
                pane_id: pane_id.into(),
                workspace_id: workspace_id(),
            },
            EventKind::PaneAgentDetected => EventData::PaneAgentDetected {
                pane_id: pane_id.into(),
                workspace_id: workspace_id(),
                agent: Some("pi".into()),
                released: false,
                final_status: None,
            },
            EventKind::PaneAgentStatusChanged => EventData::PaneAgentStatusChanged {
                pane_id: pane_id.into(),
                workspace_id: workspace_id(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: None,
                display_agent: None,
                state_labels: Default::default(),
            },
            EventKind::PaneReadStateChanged => EventData::PaneReadStateChanged {
                read_state: PaneReadState {
                    pane_id: pane_id.into(),
                    workspace_id: workspace_id(),
                    seen: true,
                    agent_status: AgentStatus::Idle,
                },
            },
            EventKind::LayoutUpdated => EventData::LayoutUpdated {
                layout: PaneLayoutSnapshot {
                    workspace_id: workspace_id(),
                    tab_id: tab_id(),
                    zoomed: false,
                    area: PaneLayoutRect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    focused_pane_id: pane_id.into(),
                    panes: vec![PaneLayoutPane {
                        pane_id: pane_id.into(),
                        focused: true,
                        rect: PaneLayoutRect {
                            x: 0,
                            y: 0,
                            width: 80,
                            height: 24,
                        },
                    }],
                    splits: Vec::new(),
                },
            },
        };
        EventEnvelope { event: kind, data }
    }
}

#[cfg(test)]
mod tests {
    use super::samples::event as sample;
    use super::*;
    use crate::api::schema::{EventData, EventEnvelope, EventKind};

    fn event() -> EventEnvelope {
        EventEnvelope {
            event: EventKind::WorkspaceFocused,
            data: EventData::WorkspaceFocused {
                workspace_id: "workspace_1".into(),
            },
        }
    }

    /// A timeout far beyond what any assertion allows: only an immediate return passes.
    const FAR: Duration = Duration::from_secs(60);
    const SOON: Duration = Duration::from_millis(60);

    #[test]
    fn checked_history_distinguishes_retained_boundary_from_lost_events() {
        let hub = EventHub::default();
        assert!(hub.events_after_checked(0).unwrap().is_empty());
        for _ in 0..EventHub::MAX_EVENTS {
            hub.push(event());
        }
        assert_eq!(
            hub.events_after_checked(0).unwrap().len(),
            EventHub::MAX_EVENTS
        );
        hub.push(event());
        assert_eq!(hub.events_after_checked(0), Err(EventHistoryError::Lost));
        let retained = hub.events_after_checked(1).unwrap();
        assert_eq!(retained.len(), EventHub::MAX_EVENTS);
        assert_eq!(retained.first().unwrap().0, 2);
        assert_eq!(retained.last().unwrap().0, hub.current_sequence());
        assert!(hub
            .events_after_checked(hub.current_sequence())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_liveness_interval_is_five_seconds_and_the_agent_status_snapshot_stays_at_one() {
        // WHY: the liveness interval is the idle cost of a held connection; the snapshot is how soon a
        // change that no event reports reaches an agent-status subscriber. They are separate on purpose.
        let timing = WaitTiming::default();
        assert_eq!(timing.liveness, Duration::from_secs(5));
        assert_eq!(timing.agent_status_snapshot, Duration::from_secs(1));
    }

    #[test]
    fn waiting_for_events_times_out_when_nothing_is_pushed() {
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![WakeInterest::any()]);
        let cursor = waiter.cursor();
        let started = std::time::Instant::now();
        assert!(!waiter.wait_after(cursor, SOON));
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn waiting_returns_at_once_when_an_event_was_pushed_after_the_cursor() {
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![WakeInterest::any()]);
        let cursor = waiter.cursor();
        hub.push(event());
        let started = std::time::Instant::now();
        assert!(waiter.wait_after(cursor, FAR));
        assert!(started.elapsed() < Duration::from_secs(30));
        // An event pushed BEFORE the cursor was taken is not new.
        assert!(!waiter.wait_after(waiter.cursor(), Duration::from_millis(10)));
    }

    #[test]
    fn a_push_from_another_thread_wakes_a_waiting_thread() {
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![WakeInterest::any()]);
        let cursor = waiter.cursor();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let started = std::time::Instant::now();
            let found = waiter.wait_after(cursor, FAR);
            (found, started.elapsed())
        });
        ready_rx.recv().unwrap();
        // Whether the push lands before or after the waiter blocks, it must not be missed.
        std::thread::sleep(Duration::from_millis(50));
        hub.push(event());
        let (found, waited) = thread.join().unwrap();
        assert!(found);
        assert!(waited < Duration::from_secs(30), "{waited:?}");
    }

    #[test]
    fn waking_waiters_releases_a_blocked_thread_without_an_event() {
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![WakeInterest::any()]);
        let cursor = waiter.cursor();
        let thread = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let found = waiter.wait_after(cursor, FAR);
            (found, started.elapsed())
        });
        // Whether the wake lands before or after the waiter blocks, it must release it.
        std::thread::sleep(Duration::from_millis(50));
        hub.wake_waiters();
        let (found, waited) = thread.join().unwrap();
        assert!(!found, "no event was published");
        assert!(waited < Duration::from_secs(30), "{waited:?}");
    }

    #[test]
    fn a_wake_between_taking_the_cursor_and_waiting_is_not_lost() {
        // The shutdown race: the pass takes its cursor, the server stops and wakes everyone, and only
        // then does the thread start to wait. It must return at once, not after its whole timeout.
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![WakeInterest::any()]);
        let cursor = waiter.cursor();
        hub.wake_waiters();
        let started = std::time::Instant::now();
        assert!(!waiter.wait_after(cursor, FAR));
        assert!(started.elapsed() < Duration::from_secs(30));
        // A thread that takes its cursor AFTER the wake is not released by it.
        let later = waiter.cursor();
        let started = std::time::Instant::now();
        assert!(!waiter.wait_after(later, SOON));
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn waiting_on_a_poisoned_history_returns_true_after_a_short_sleep_not_a_hot_loop() {
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![WakeInterest::any()]);
        assert!(std::panic::catch_unwind(|| {
            let _guard = hub.inner.state.lock().unwrap();
            panic!("poison the test event history");
        })
        .is_err());
        let cursor = waiter.cursor();
        let started = std::time::Instant::now();
        assert!(waiter.wait_after(cursor, FAR));
        let waited = started.elapsed();
        // It slept (the caller loops, so returning at once would spin) but did not wait out the timeout.
        assert!(waited >= Duration::from_millis(50), "{waited:?}");
        assert!(waited < Duration::from_secs(30), "{waited:?}");
    }

    #[test]
    fn checked_history_reports_unavailable_instead_of_empty_after_poison() {
        let hub = EventHub::default();
        assert!(std::panic::catch_unwind(|| {
            let _guard = hub.inner.state.lock().unwrap();
            panic!("poison the test event history");
        })
        .is_err());
        assert_eq!(
            hub.events_after_checked(0),
            Err(EventHistoryError::Unavailable)
        );
    }

    // ---- which events wake which waiter ----------------------------------------------------------

    /// Takes the cursor, pushes `event` and says whether the waiter was woken (its wait returned at
    /// once because of the event) rather than timing out.
    fn woken_by(hub: &EventHub, waiter: &EventWaiter, event: EventEnvelope) -> bool {
        let cursor = waiter.cursor();
        hub.push(event);
        waiter.wait_after(cursor, Duration::from_millis(5))
    }

    #[test]
    fn equal_interests_are_registered_once() {
        // WHY: `push` scans every interest of every waiter with the hub mutex held, on the
        // application's thread, and a single request can carry tens of thousands of identical
        // subscriptions: the scan must stay bounded by what is distinct, and dropping the copies
        // must not change what wakes the waiter.
        let hub = EventHub::default();
        let mut interests = Vec::new();
        for _ in 0..5_000 {
            interests.push(WakeInterest::kind(EventKind::PaneCreated));
            interests.push(WakeInterest::pane("pane_a"));
            interests.push(WakeInterest::pane_kind(
                "pane_a",
                EventKind::PaneReadStateChanged,
            ));
            interests.push(WakeInterest::pane_kind(
                "pane_b",
                EventKind::PaneReadStateChanged,
            ));
        }
        let waiter = hub.waiter(interests);
        assert_eq!(waiter.interest_count(), 4);
        // Each distinct interest still wakes the waiter, and nothing else does.
        assert!(woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneCreated, "pane_c")
        ));
        assert!(woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneAgentStatusChanged, "pane_a")
        ));
        assert!(woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneReadStateChanged, "pane_b")
        ));
        assert!(!woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneAgentStatusChanged, "pane_b")
        ));
        // Interests that differ only in the kind or only in the pane are not copies.
        let distinct = hub.waiter(vec![
            WakeInterest::kind(EventKind::PaneCreated),
            WakeInterest::kind(EventKind::PaneClosed),
            WakeInterest::pane("pane_a"),
            WakeInterest::pane("pane_b"),
            WakeInterest::pane_kind("pane_a", EventKind::PaneClosed),
            WakeInterest::pane_kind("pane_b", EventKind::PaneClosed),
        ]);
        assert_eq!(distinct.interest_count(), 6);
    }

    #[test]
    fn a_waiter_on_one_pane_is_not_woken_by_events_about_another_pane() {
        // The point of the filtered wake: with one subscriber per pane, an event about pane B must
        // not cost the subscriber of pane A a thread wake-up.
        let hub = EventHub::default();
        let a = hub.waiter(vec![WakeInterest::pane("pane_a")]);
        for kind in [
            EventKind::PaneAgentStatusChanged,
            EventKind::PaneReadStateChanged,
            EventKind::PaneUpdated,
            EventKind::PaneFocused,
            EventKind::PaneClosed,
        ] {
            assert!(!woken_by(&hub, &a, sample(kind, "pane_b")), "{kind:?}");
        }
        assert_eq!(a.wake_count(), 0);
    }

    #[test]
    fn a_waiter_on_one_pane_is_woken_at_once_by_an_event_about_that_pane() {
        let hub = EventHub::default();
        let a = hub.waiter(vec![WakeInterest::pane("pane_a")]);
        let b = hub.waiter(vec![WakeInterest::pane("pane_b")]);
        let cursor = a.cursor();
        hub.push(sample(EventKind::PaneAgentStatusChanged, "pane_a"));
        let started = std::time::Instant::now();
        assert!(a.wait_after(cursor, FAR));
        assert!(started.elapsed() < Duration::from_secs(30));
        assert_eq!((a.wake_count(), b.wake_count()), (1, 0));
    }

    #[test]
    fn a_pane_and_kind_interest_ignores_other_kinds_about_the_same_pane() {
        // A read-state subscription consumes only `pane.read_state_changed`: the many other
        // events about its own pane must not wake it.
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![WakeInterest::pane_kind(
            "pane_a",
            EventKind::PaneReadStateChanged,
        )]);
        assert!(!woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneAgentStatusChanged, "pane_a")
        ));
        assert!(!woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneReadStateChanged, "pane_b")
        ));
        assert!(woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneReadStateChanged, "pane_a")
        ));
    }

    #[test]
    fn a_kind_interest_sees_every_event_of_its_kind_from_any_pane_and_no_other() {
        // A `workspace.renamed` or `pane.created` subscription is not about one pane: it must see
        // all events of its kind, as it did when every event woke every connection.
        let hub = EventHub::default();
        let created = hub.waiter(vec![WakeInterest::kind(EventKind::PaneCreated)]);
        assert!(woken_by(
            &hub,
            &created,
            sample(EventKind::PaneCreated, "pane_a")
        ));
        assert!(woken_by(
            &hub,
            &created,
            sample(EventKind::PaneCreated, "pane_b")
        ));
        assert!(!woken_by(
            &hub,
            &created,
            sample(EventKind::PaneClosed, "pane_a")
        ));
        assert!(!woken_by(
            &hub,
            &created,
            sample(EventKind::TabCreated, "pane_a")
        ));
    }

    #[test]
    fn an_interest_in_every_event_sees_every_kind() {
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![WakeInterest::any()]);
        for kind in samples_of_every_kind() {
            assert!(woken_by(&hub, &waiter, sample(kind, "pane_a")), "{kind:?}");
        }
    }

    fn samples_of_every_kind() -> Vec<EventKind> {
        crate::api::schema::KNOWN_EVENT_KINDS.to_vec()
    }

    #[test]
    fn a_waiter_with_interests_in_two_panes_is_woken_by_either() {
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![
            WakeInterest::pane("pane_a"),
            WakeInterest::pane_kind("pane_b", EventKind::PaneReadStateChanged),
        ]);
        assert!(woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneUpdated, "pane_a")
        ));
        assert!(woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneReadStateChanged, "pane_b")
        ));
        assert!(!woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneUpdated, "pane_c")
        ));
    }

    #[test]
    fn an_event_whose_panes_are_unknown_wakes_every_waiter() {
        // A tab or workspace that closes (or a tab that moves) takes panes along without naming
        // them: nobody can say it does not concern them, so everybody is woken, whatever they
        // listen for, including a waiter with no event interest at all.
        for kind in [
            EventKind::TabClosed,
            EventKind::TabMoved,
            EventKind::WorkspaceClosed,
            EventKind::WorktreeRemoved,
        ] {
            let hub = EventHub::default();
            let waiters = [
                hub.waiter(vec![WakeInterest::pane("pane_a")]),
                hub.waiter(vec![WakeInterest::pane_kind(
                    "pane_b",
                    EventKind::PaneReadStateChanged,
                )]),
                hub.waiter(vec![WakeInterest::kind(EventKind::WorkspaceFocused)]),
                hub.waiter(Vec::new()),
            ];
            let cursors: Vec<_> = waiters.iter().map(EventWaiter::cursor).collect();
            hub.push(sample(kind, "pane_a"));
            for (index, (waiter, cursor)) in waiters.iter().zip(cursors).enumerate() {
                assert!(
                    waiter.wait_after(cursor, FAR),
                    "{kind:?} did not wake waiter {index}"
                );
            }
        }
    }

    #[test]
    fn events_that_leave_every_pane_alone_wake_no_pane_waiter() {
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![WakeInterest::pane("pane_a")]);
        for kind in [
            EventKind::WorkspaceCreated,
            EventKind::WorkspaceUpdated,
            EventKind::WorkspaceMetadataUpdated,
            EventKind::WorkspaceRenamed,
            EventKind::WorkspaceMoved,
            EventKind::WorkspaceReordered,
            EventKind::WorkspaceFocused,
            EventKind::WorktreeCreated,
            EventKind::WorktreeOpened,
            EventKind::TabCreated,
            EventKind::TabRenamed,
            EventKind::TabFocused,
        ] {
            assert!(!woken_by(&hub, &waiter, sample(kind, "pane_b")), "{kind:?}");
        }
    }

    #[test]
    fn a_moved_pane_wakes_the_waiters_of_its_old_and_its_new_id() {
        let hub = EventHub::default();
        let old = hub.waiter(vec![WakeInterest::pane("pane_a")]);
        let new = hub.waiter(vec![WakeInterest::pane("moved_pane_a")]);
        let other = hub.waiter(vec![WakeInterest::pane("pane_b")]);
        let cursors = [old.cursor(), new.cursor(), other.cursor()];
        hub.push(sample(EventKind::PaneMoved, "pane_a"));
        assert!(old.wait_after(cursors[0], FAR));
        assert!(new.wait_after(cursors[1], FAR));
        assert!(!other.wait_after(cursors[2], Duration::from_millis(5)));
    }

    #[test]
    fn a_layout_update_wakes_the_waiters_of_the_panes_in_it() {
        let hub = EventHub::default();
        let inside = hub.waiter(vec![WakeInterest::pane("pane_a")]);
        let outside = hub.waiter(vec![WakeInterest::pane("pane_b")]);
        let cursors = [inside.cursor(), outside.cursor()];
        hub.push(sample(EventKind::LayoutUpdated, "pane_a"));
        assert!(inside.wait_after(cursors[0], FAR));
        assert!(!outside.wait_after(cursors[1], Duration::from_millis(5)));
    }

    #[test]
    fn a_waiter_with_no_interests_is_woken_by_no_event_but_by_the_stop_wake() {
        // A sampling-only connection (output matches, scroll changes) never reads the history:
        // events must not wake it, the server's stop must.
        let hub = EventHub::default();
        let waiter = hub.waiter(Vec::new());
        assert!(!woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneUpdated, "pane_a")
        ));
        assert_eq!(waiter.wake_count(), 0);
        let cursor = waiter.cursor();
        hub.wake_waiters();
        let started = std::time::Instant::now();
        waiter.wait_after(cursor, FAR);
        assert!(started.elapsed() < Duration::from_secs(30));
        assert_eq!(waiter.wake_count(), 1);
    }

    #[test]
    fn waking_waiters_wakes_every_registered_waiter() {
        // Shutdown: every connection thread, whatever it listens for, is released by one call.
        let hub = EventHub::default();
        let waiters = [
            hub.waiter(vec![WakeInterest::pane("pane_a")]),
            hub.waiter(vec![WakeInterest::kind(EventKind::WorkspaceFocused)]),
            hub.waiter(Vec::new()),
        ];
        let threads: Vec<_> = waiters
            .into_iter()
            .map(|waiter| {
                let cursor = waiter.cursor();
                std::thread::spawn(move || {
                    let started = std::time::Instant::now();
                    waiter.wait_after(cursor, FAR);
                    started.elapsed()
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(50));
        hub.wake_waiters();
        for thread in threads {
            assert!(thread.join().unwrap() < Duration::from_secs(30));
        }
    }

    #[test]
    fn a_wake_that_is_already_pending_is_not_sent_again() {
        // A burst of matching events wakes the thread once: it reads them all in its next pass.
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![WakeInterest::pane("pane_a")]);
        let cursor = waiter.cursor();
        for _ in 0..50 {
            hub.push(sample(EventKind::PaneUpdated, "pane_a"));
        }
        assert_eq!(waiter.wake_count(), 1);
        assert!(waiter.wait_after(cursor, FAR));
        // The next pass starts a new cursor, and a later event wakes it again.
        assert!(woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneUpdated, "pane_a")
        ));
        assert_eq!(waiter.wake_count(), 2);
    }

    #[test]
    fn a_waiter_that_falls_half_the_history_behind_is_woken_whatever_the_events_are() {
        // A subscription connection that sleeps through events of other panes must still read the
        // history before it loses events (`events_lost` closes the connection).
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![WakeInterest::pane("pane_a")]);
        let cursor = waiter.cursor();
        for _ in 0..LAG_WAKE_EVENTS - 1 {
            hub.push(sample(EventKind::PaneUpdated, "pane_b"));
        }
        assert_eq!(waiter.wake_count(), 0);
        hub.push(sample(EventKind::PaneUpdated, "pane_b"));
        assert_eq!(waiter.wake_count(), 1);
        let started = std::time::Instant::now();
        assert!(waiter.wait_after(cursor, FAR));
        assert!(started.elapsed() < Duration::from_secs(30));
        // Once it has looked (a new cursor), the count starts again.
        let cursor = waiter.cursor();
        hub.push(sample(EventKind::PaneUpdated, "pane_b"));
        assert!(!waiter.wait_after(cursor, Duration::from_millis(5)));
    }

    #[test]
    fn a_dropped_waiter_is_no_longer_woken_or_counted() {
        let hub = EventHub::default();
        let waiter = hub.waiter(vec![WakeInterest::any()]);
        let other = hub.waiter(vec![WakeInterest::any()]);
        assert_eq!(hub.inner.state.lock().unwrap().waiters.len(), 2);
        drop(waiter);
        let remaining = hub.inner.state.lock().unwrap();
        assert_eq!(remaining.waiters.len(), 1);
        assert!(Arc::ptr_eq(&remaining.waiters[0], &other.shared));
    }

    #[test]
    fn a_waiter_registered_late_is_not_woken_for_events_before_it() {
        let hub = EventHub::default();
        for _ in 0..LAG_WAKE_EVENTS * 2 {
            hub.push(sample(EventKind::PaneUpdated, "pane_b"));
        }
        let waiter = hub.waiter(vec![WakeInterest::pane("pane_a")]);
        assert!(!woken_by(
            &hub,
            &waiter,
            sample(EventKind::PaneUpdated, "pane_b")
        ));
        assert_eq!(waiter.wake_count(), 0);
    }
}
