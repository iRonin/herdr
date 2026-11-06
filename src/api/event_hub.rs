use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::api::schema::{EventData, EventEnvelope, EventKind};

/// The longest a held connection thread blocks on the hub before it looks at its socket (a closed
/// peer) and the server's `running` flag again. Nothing else wakes an idle connection.
const SUBSCRIPTION_LIVENESS_INTERVAL: Duration = Duration::from_secs(1);
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
    pub(super) fn waiter(&self, interests: Vec<WakeInterest>) -> EventWaiter {
        let shared = Arc::new(Waiter {
            interests,
            wake: Condvar::new(),
            woken_at: AtomicU64::new(0),
            cursor_sequence: AtomicU64::new(0),
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

#[cfg(test)]
mod tests {
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
}
