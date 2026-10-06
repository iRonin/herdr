//! Keys typed while the client waits for its first shell snapshot.
//!
//! The client is connected to its first endpoint a moment before that endpoint's first snapshot
//! arrives. Until then it has no pane to send keys to, so keys read in that window used to be
//! dropped, and a line typed across the snapshot reached the pane as a tail, which a shell then
//! runs. The hold keeps those raw bytes and returns them, in order and ahead of anything read
//! later, once the first snapshot of that same endpoint and connection generation is projected.
//! The caller replays them through the ordinary input path, so whatever reads them first (the
//! pane, prefix keys, an overlay) interprets them as ordinary terminal typeahead.
//!
//! The hold is one-shot. It engages only if the client's very first input arrives while its
//! first connection waits for a snapshot, and it ends for good at that snapshot. Reconnects and
//! endpoint handoffs later in the session never re-arm it; their input rules, including the
//! activation freeze, are unchanged. Held keys are discarded, never redirected, whenever the
//! connection they were typed toward goes away, is frozen or is replaced, or a different
//! surface is projected first.

use std::time::{Duration, Instant};

use super::endpoint::ClientEndpointId;

/// A larger burst is discarded whole rather than replayed in part. Typing never comes close; a
/// paste can. Replaying more than about 1 KiB into a pane shell that is still starting can also
/// meet the macOS terminal driver's typeahead replay defect.
pub(super) const STARTUP_INPUT_HOLD_LIMIT: usize = 1024;

/// If the first snapshot has not arrived this long after the first held byte, the held bytes are
/// discarded and the client stops holding, so a server that never sends one cannot keep the
/// client's own keys (detach, machine selection) unreachable.
pub(super) const STARTUP_INPUT_HOLD_EXPIRY: Duration = Duration::from_secs(10);

/// What the hold needs to know about the client at one moment.
#[derive(Clone, Copy, Debug)]
pub(super) struct StartupView<'a> {
    /// The active endpoint and its connection generation, while that connection accepts input.
    pub(super) connected: Option<(&'a ClientEndpointId, u64)>,
    /// The endpoint and connection generation of the projected snapshot, once there is one.
    /// The generation is `None` if the client cannot tell which connection it came from.
    pub(super) snapshot: Option<(&'a ClientEndpointId, Option<u64>)>,
    pub(super) now: Instant,
}

#[derive(Debug)]
enum HoldState {
    /// No input read yet.
    Armed,
    Holding {
        endpoint: ClientEndpointId,
        generation: u64,
        since: Instant,
        bytes: Vec<u8>,
    },
    /// The hold overflowed. Every byte is dropped until the first snapshot, so no partial burst
    /// is ever replayed.
    Discarding {
        endpoint: ClientEndpointId,
        generation: u64,
        since: Instant,
    },
    Done,
}

#[derive(Debug)]
pub(super) struct StartupInputHold {
    state: HoldState,
}

impl StartupInputHold {
    pub(super) fn new() -> Self {
        Self {
            state: HoldState::Armed,
        }
    }

    #[cfg(test)]
    fn is_done(&self) -> bool {
        matches!(self.state, HoldState::Done)
    }

    /// Settles the hold against the client's current state. Returns the held bytes when the
    /// snapshot they were waiting for has been projected; the caller must process them before
    /// any input read after this call.
    pub(super) fn take_release(&mut self, view: StartupView<'_>) -> Option<Vec<u8>> {
        let (endpoint, generation, since) = match &self.state {
            HoldState::Holding {
                endpoint,
                generation,
                since,
                ..
            }
            | HoldState::Discarding {
                endpoint,
                generation,
                since,
            } => (endpoint, *generation, *since),
            HoldState::Armed => {
                if view.snapshot.is_some() {
                    self.state = HoldState::Done;
                }
                return None;
            }
            HoldState::Done => return None,
        };
        // Expiry and the connection are settled first: a snapshot that is first seen after the
        // expiry, or whose connection no longer accepts input, must not release the keys.
        if view.connected != Some((endpoint, generation))
            || view.now.saturating_duration_since(since) >= STARTUP_INPUT_HOLD_EXPIRY
        {
            self.state = HoldState::Done;
            return None;
        }
        if view.snapshot == Some((endpoint, Some(generation))) {
            return match std::mem::replace(&mut self.state, HoldState::Done) {
                HoldState::Holding { bytes, .. } if !bytes.is_empty() => Some(bytes),
                _ => None,
            };
        }
        if view.snapshot.is_some() {
            self.state = HoldState::Done;
        }
        None
    }

    /// Routes one read from the terminal. Returns the bytes the caller must process now: held
    /// bytes being released, followed by `data`. `None` means the bytes were held or discarded.
    pub(super) fn input(&mut self, data: Vec<u8>, view: StartupView<'_>) -> Option<Vec<u8>> {
        let released = self.take_release(view);
        match &mut self.state {
            HoldState::Done => Some(match released {
                Some(mut bytes) => {
                    bytes.extend_from_slice(&data);
                    bytes
                }
                None => data,
            }),
            HoldState::Armed => match view.connected {
                Some((endpoint, generation)) if view.snapshot.is_none() => {
                    let endpoint = endpoint.clone();
                    self.state = if data.len() > STARTUP_INPUT_HOLD_LIMIT {
                        HoldState::Discarding {
                            endpoint,
                            generation,
                            since: view.now,
                        }
                    } else {
                        HoldState::Holding {
                            endpoint,
                            generation,
                            since: view.now,
                            bytes: data,
                        }
                    };
                    None
                }
                // The first input went to whatever the client shows without a connection (the
                // machine chooser). Holding later input could replay a line without its head.
                _ => {
                    self.state = HoldState::Done;
                    Some(data)
                }
            },
            HoldState::Holding {
                endpoint,
                generation,
                since,
                bytes,
            } => {
                if bytes.len() + data.len() > STARTUP_INPUT_HOLD_LIMIT {
                    self.state = HoldState::Discarding {
                        endpoint: endpoint.clone(),
                        generation: *generation,
                        since: *since,
                    };
                } else {
                    bytes.extend_from_slice(&data);
                }
                None
            }
            HoldState::Discarding { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> ClientEndpointId {
        ClientEndpointId::Local
    }

    fn remote() -> ClientEndpointId {
        ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        )
    }

    fn waiting(endpoint: &ClientEndpointId, generation: u64, now: Instant) -> StartupView<'_> {
        StartupView {
            connected: Some((endpoint, generation)),
            snapshot: None,
            now,
        }
    }

    fn projected(endpoint: &ClientEndpointId, generation: u64, now: Instant) -> StartupView<'_> {
        StartupView {
            connected: Some((endpoint, generation)),
            snapshot: Some((endpoint, Some(generation))),
            now,
        }
    }

    #[test]
    fn keys_before_the_first_snapshot_are_replayed_in_order_ahead_of_later_keys() {
        let local = local();
        let now = Instant::now();
        let mut hold = StartupInputHold::new();
        assert_eq!(hold.input(b"ec".to_vec(), waiting(&local, 1, now)), None);
        assert_eq!(hold.input(b"ho a".to_vec(), waiting(&local, 1, now)), None);
        // The first read after the snapshot carries the held keys first, then its own.
        assert_eq!(
            hold.input(b"b\r".to_vec(), projected(&local, 1, now)),
            Some(b"echo ab\r".to_vec())
        );
        assert!(hold.is_done());
        assert_eq!(
            hold.input(b"x".to_vec(), projected(&local, 1, now)),
            Some(b"x".to_vec())
        );
    }

    #[test]
    fn held_keys_are_released_by_the_snapshot_without_waiting_for_more_input() {
        let local = local();
        let now = Instant::now();
        let mut hold = StartupInputHold::new();
        assert_eq!(hold.input(b"ls\r".to_vec(), waiting(&local, 7, now)), None);
        assert_eq!(hold.take_release(waiting(&local, 7, now)), None);
        assert_eq!(
            hold.take_release(projected(&local, 7, now)),
            Some(b"ls\r".to_vec())
        );
        assert_eq!(hold.take_release(projected(&local, 7, now)), None);
    }

    #[test]
    fn keys_never_reach_a_different_endpoint_or_connection() {
        let local = local();
        let remote = remote();
        let now = Instant::now();

        // Another endpoint becomes active before the snapshot.
        let mut hold = StartupInputHold::new();
        assert_eq!(hold.input(b"rm -r".to_vec(), waiting(&local, 1, now)), None);
        assert_eq!(hold.take_release(waiting(&remote, 2, now)), None);
        assert!(hold.is_done());
        assert_eq!(hold.take_release(projected(&remote, 2, now)), None);
        assert_eq!(
            hold.input(b"x".to_vec(), projected(&remote, 2, now)),
            Some(b"x".to_vec()),
            "only the new key reaches the new endpoint"
        );

        // The same endpoint reconnects with a new generation before the snapshot.
        let mut hold = StartupInputHold::new();
        assert_eq!(hold.input(b"abc".to_vec(), waiting(&local, 1, now)), None);
        assert_eq!(hold.take_release(projected(&local, 2, now)), None);
        assert!(hold.is_done());

        // A different surface is projected while the held connection is still the active one.
        let mut hold = StartupInputHold::new();
        assert_eq!(hold.input(b"abc".to_vec(), waiting(&local, 1, now)), None);
        let other = StartupView {
            connected: Some((&local, 1)),
            snapshot: Some((&remote, Some(3))),
            now,
        };
        assert_eq!(hold.input(b"d".to_vec(), other), Some(b"d".to_vec()));

        // The connection stops accepting input (lost, or frozen by an activation).
        let mut hold = StartupInputHold::new();
        assert_eq!(hold.input(b"abc".to_vec(), waiting(&local, 1, now)), None);
        let frozen = StartupView {
            connected: None,
            snapshot: None,
            now,
        };
        assert_eq!(hold.take_release(frozen), None);
        assert!(hold.is_done());
        assert_eq!(hold.take_release(projected(&local, 1, now)), None);
    }

    #[test]
    fn an_overflow_discards_everything_until_the_snapshot_and_never_replays_a_tail() {
        let local = local();
        let now = Instant::now();
        let mut hold = StartupInputHold::new();
        assert_eq!(
            hold.input(
                vec![b'a'; STARTUP_INPUT_HOLD_LIMIT - 1],
                waiting(&local, 1, now)
            ),
            None
        );
        assert_eq!(hold.input(b"bc".to_vec(), waiting(&local, 1, now)), None);
        // Still discarding: a key after the overflow is not held, so it cannot replay as a tail.
        assert_eq!(
            hold.input(b"tail\r".to_vec(), waiting(&local, 1, now)),
            None
        );
        assert_eq!(hold.take_release(projected(&local, 1, now)), None);
        assert!(hold.is_done());
        assert_eq!(
            hold.input(b"x".to_vec(), projected(&local, 1, now)),
            Some(b"x".to_vec())
        );

        // A single read over the limit is discarded the same way.
        let mut hold = StartupInputHold::new();
        assert_eq!(
            hold.input(
                vec![b'p'; STARTUP_INPUT_HOLD_LIMIT + 1],
                waiting(&local, 1, now)
            ),
            None
        );
        assert_eq!(hold.take_release(projected(&local, 1, now)), None);

        // Exactly the limit is still held and replayed.
        let mut hold = StartupInputHold::new();
        assert_eq!(
            hold.input(
                vec![b'q'; STARTUP_INPUT_HOLD_LIMIT],
                waiting(&local, 1, now)
            ),
            None
        );
        assert_eq!(
            hold.take_release(projected(&local, 1, now)),
            Some(vec![b'q'; STARTUP_INPUT_HOLD_LIMIT])
        );
    }

    #[test]
    fn the_hold_engages_only_for_the_first_connection_and_never_re_arms() {
        let local = local();
        let now = Instant::now();

        // First input while no connection accepts input: it goes to the client as before.
        let mut hold = StartupInputHold::new();
        let unconnected = StartupView {
            connected: None,
            snapshot: None,
            now,
        };
        assert_eq!(hold.input(b"j".to_vec(), unconnected), Some(b"j".to_vec()));
        assert_eq!(
            hold.input(b"k".to_vec(), waiting(&local, 1, now)),
            Some(b"k".to_vec()),
            "input that already went to the client is never followed by held input"
        );

        // A snapshot that arrives before any input ends the hold unused.
        let mut hold = StartupInputHold::new();
        assert_eq!(hold.take_release(projected(&local, 1, now)), None);
        assert!(hold.is_done());

        // After release, a later reconnect that again waits for a snapshot does not hold.
        let mut hold = StartupInputHold::new();
        assert_eq!(hold.input(b"a".to_vec(), waiting(&local, 1, now)), None);
        assert_eq!(
            hold.take_release(projected(&local, 1, now)),
            Some(b"a".to_vec())
        );
        assert_eq!(
            hold.input(b"b".to_vec(), waiting(&local, 2, now)),
            Some(b"b".to_vec())
        );
    }

    /// A hold with one line typed toward local connection 1 at `start`.
    fn holding_a_line(local: &ClientEndpointId, start: Instant) -> StartupInputHold {
        let mut hold = StartupInputHold::new();
        assert_eq!(
            hold.input(b"rm x\r".to_vec(), waiting(local, 1, start)),
            None
        );
        hold
    }

    #[test]
    fn a_matching_snapshot_inside_the_expiry_releases_the_held_keys() {
        let local = local();
        let start = Instant::now();
        let mut hold = holding_a_line(&local, start);
        let before = start + STARTUP_INPUT_HOLD_EXPIRY - Duration::from_millis(1);
        assert_eq!(
            hold.take_release(projected(&local, 1, before)),
            Some(b"rm x\r".to_vec())
        );
    }

    #[test]
    fn a_matching_snapshot_first_seen_at_the_expiry_does_not_replay_the_held_keys() {
        let local = local();
        let start = Instant::now();
        let mut hold = holding_a_line(&local, start);
        let after = start + STARTUP_INPUT_HOLD_EXPIRY;
        assert_eq!(hold.take_release(projected(&local, 1, after)), None);
        assert!(hold.is_done());
        assert_eq!(
            hold.input(b"x".to_vec(), projected(&local, 1, after)),
            Some(b"x".to_vec()),
            "only the new key reaches the pane"
        );
    }

    #[test]
    fn a_matching_snapshot_of_a_frozen_connection_does_not_release_the_held_keys() {
        let local = local();
        let start = Instant::now();
        let mut hold = holding_a_line(&local, start);
        let frozen = StartupView {
            connected: None,
            snapshot: Some((&local, Some(1))),
            now: start,
        };
        assert_eq!(hold.take_release(frozen), None);
        assert!(hold.is_done());
        assert_eq!(hold.take_release(projected(&local, 1, start)), None);
    }

    #[test]
    fn a_retained_snapshot_does_not_release_the_held_keys_to_another_active_connection() {
        let local = local();
        let remote = remote();
        let start = Instant::now();
        for active in [(&local, 2), (&remote, 1)] {
            let mut hold = holding_a_line(&local, start);
            let changed = StartupView {
                connected: Some(active),
                snapshot: Some((&local, Some(1))),
                now: start,
            };
            assert_eq!(hold.take_release(changed), None, "active {active:?}");
            assert!(hold.is_done());
            assert_eq!(
                hold.input(b"y".to_vec(), changed),
                Some(b"y".to_vec()),
                "only the new key reaches the new connection"
            );
        }
    }

    #[test]
    fn a_snapshot_that_never_comes_releases_the_keyboard_after_the_expiry() {
        let local = local();
        let start = Instant::now();
        let mut hold = StartupInputHold::new();
        assert_eq!(hold.input(b"abc".to_vec(), waiting(&local, 1, start)), None);
        let before = start + STARTUP_INPUT_HOLD_EXPIRY - Duration::from_millis(1);
        assert_eq!(hold.input(b"d".to_vec(), waiting(&local, 1, before)), None);
        let after = start + STARTUP_INPUT_HOLD_EXPIRY;
        assert_eq!(hold.take_release(waiting(&local, 1, after)), None);
        assert!(hold.is_done(), "the held keys are discarded, not replayed");
        assert_eq!(
            hold.input(b"e".to_vec(), waiting(&local, 1, after)),
            Some(b"e".to_vec())
        );

        // The expiry also ends a discarding hold.
        let mut hold = StartupInputHold::new();
        assert_eq!(
            hold.input(
                vec![b'p'; STARTUP_INPUT_HOLD_LIMIT + 1],
                waiting(&local, 1, start)
            ),
            None
        );
        assert_eq!(
            hold.input(b"f".to_vec(), waiting(&local, 1, after)),
            Some(b"f".to_vec())
        );
    }
}
