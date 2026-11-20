use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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
/// at the start of a pass (see `EventHub::wait_cursor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WaitCursor {
    sequence: u64,
    generation: u64,
}

#[derive(Clone, Default)]
pub struct EventHub {
    inner: Arc<EventHubShared>,
}

#[derive(Default)]
struct EventHubShared {
    state: Mutex<EventHubState>,
    /// Notified by every `push`, so a thread waiting for new events does not have to poll.
    wake: Condvar,
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
        state.events.push((sequence, event));
        let overflow = state.events.len().saturating_sub(Self::MAX_EVENTS);
        if overflow > 0 {
            state.events.drain(0..overflow);
        }
        drop(state);
        self.inner.wake.notify_all();
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

    /// Reads where a pass starts. Take it FIRST in the pass, before the `running` check and the
    /// history read, then wait on it: an event pushed, or a `wake_waiters`, at any point after
    /// this call makes `wait_for_event_after` return at once, so neither can be lost between the
    /// pass and the wait.
    pub(super) fn wait_cursor(&self) -> WaitCursor {
        let Ok(state) = self.inner.state.lock() else {
            return WaitCursor {
                sequence: 0,
                generation: 0,
            };
        };
        WaitCursor {
            sequence: state.next_sequence,
            generation: state.wake_generation,
        }
    }

    /// Blocks until an event newer than the cursor has been pushed, `wake_waiters` has been called
    /// since the cursor was taken, or `timeout` has passed; returns whether a newer event exists.
    /// A poisoned history cannot be waited on: this sleeps a short interval (the old polling
    /// cadence, never a hot loop) and returns `true`, so the caller's next read reports it as it
    /// always did.
    pub(super) fn wait_for_event_after(&self, cursor: WaitCursor, timeout: Duration) -> bool {
        let Ok(state) = self.inner.state.lock() else {
            std::thread::sleep(timeout.min(POISONED_HISTORY_POLL_INTERVAL));
            return true;
        };
        match self.inner.wake.wait_timeout_while(state, timeout, |state| {
            state.next_sequence <= cursor.sequence && state.wake_generation == cursor.generation
        }) {
            Ok((state, _)) => state.next_sequence > cursor.sequence,
            Err(_) => {
                std::thread::sleep(timeout.min(POISONED_HISTORY_POLL_INTERVAL));
                true
            }
        }
    }

    /// Releases every thread that took its cursor before this call and has not yet been woken, so a
    /// thread that must stop (the server is shutting down) notices at once. Set the stop flag
    /// BEFORE calling this: a thread that takes its cursor afterwards sees the flag instead.
    pub(super) fn wake_waiters(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.wake_generation += 1;
        }
        self.inner.wake.notify_all();
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
        let cursor = hub.wait_cursor();
        let started = std::time::Instant::now();
        assert!(!hub.wait_for_event_after(cursor, Duration::from_millis(60)));
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn waiting_returns_at_once_when_an_event_was_pushed_after_the_cursor() {
        let hub = EventHub::default();
        let cursor = hub.wait_cursor();
        hub.push(event());
        let started = std::time::Instant::now();
        // The timeout is far beyond the assertion below: only an immediate return passes.
        assert!(hub.wait_for_event_after(cursor, Duration::from_secs(60)));
        assert!(started.elapsed() < Duration::from_secs(30));
        // An event pushed BEFORE the cursor was taken is not new.
        assert!(!hub.wait_for_event_after(hub.wait_cursor(), Duration::from_millis(10)));
    }

    #[test]
    fn a_push_from_another_thread_wakes_a_waiting_thread() {
        let hub = EventHub::default();
        let cursor = hub.wait_cursor();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let waiter = {
            let hub = hub.clone();
            std::thread::spawn(move || {
                ready_tx.send(()).unwrap();
                let started = std::time::Instant::now();
                let found = hub.wait_for_event_after(cursor, Duration::from_secs(60));
                (found, started.elapsed())
            })
        };
        ready_rx.recv().unwrap();
        // Whether the push lands before or after the waiter blocks, it must not be missed.
        std::thread::sleep(Duration::from_millis(50));
        hub.push(event());
        let (found, waited) = waiter.join().unwrap();
        assert!(found);
        assert!(waited < Duration::from_secs(30), "{waited:?}");
    }

    #[test]
    fn waking_waiters_releases_a_blocked_thread_without_an_event() {
        let hub = EventHub::default();
        let cursor = hub.wait_cursor();
        let waiter = {
            let hub = hub.clone();
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                let found = hub.wait_for_event_after(cursor, Duration::from_secs(60));
                (found, started.elapsed())
            })
        };
        // Whether the wake lands before or after the waiter blocks, it must release it.
        std::thread::sleep(Duration::from_millis(50));
        hub.wake_waiters();
        let (found, waited) = waiter.join().unwrap();
        assert!(!found, "no event was published");
        assert!(waited < Duration::from_secs(30), "{waited:?}");
    }

    #[test]
    fn a_wake_between_taking_the_cursor_and_waiting_is_not_lost() {
        // The shutdown race: the pass takes its cursor, the server stops and wakes everyone, and only
        // then does the thread start to wait. It must return at once, not after its whole timeout.
        let hub = EventHub::default();
        let cursor = hub.wait_cursor();
        hub.wake_waiters();
        let started = std::time::Instant::now();
        assert!(!hub.wait_for_event_after(cursor, Duration::from_secs(60)));
        assert!(started.elapsed() < Duration::from_secs(30));
        // A thread that takes its cursor AFTER the wake is not released by it.
        let later = hub.wait_cursor();
        let started = std::time::Instant::now();
        assert!(!hub.wait_for_event_after(later, Duration::from_millis(60)));
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn waiting_on_a_poisoned_history_returns_true_after_a_short_sleep_not_a_hot_loop() {
        let hub = EventHub::default();
        assert!(std::panic::catch_unwind(|| {
            let _guard = hub.inner.state.lock().unwrap();
            panic!("poison the test event history");
        })
        .is_err());
        let cursor = hub.wait_cursor();
        let started = std::time::Instant::now();
        assert!(hub.wait_for_event_after(cursor, Duration::from_secs(60)));
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
