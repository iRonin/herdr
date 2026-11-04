//! Which signal reached the server and who sent it.
//!
//! The server's stop signals (SIGINT, SIGTERM) and its ignored hangup (SIGHUP) are watched here with signal-hook's
//! iterator and its `WithOrigin` exfiltrator, which reports the signal and, from the kernel's signal information, the
//! sender's process id and user id (Linux and macOS). Each signal is logged at INFO with its sender before the server is
//! asked to quit, so a server stopped by a signal leaves a line that tells it from any other graceful shutdown. What
//! happens to each signal is unchanged: SIGINT and SIGTERM call `on_quit`, SIGHUP is logged and ignored.

use std::sync::Arc;

use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::exfiltrator::origin::WithOrigin;
use signal_hook::iterator::SignalsInfo;
use tracing::{info, warn};

use super::ServerQuitSignal;

/// A signal as the server reports it: its number, the sender (process id and user id) when a process sent it, and the
/// kernel's reason for it.
pub(crate) struct SignalOrigin {
    pub(crate) signal: i32,
    pub(crate) sender: Option<(u32, u32)>,
    pub(crate) cause: String,
}

/// Starts the watching thread. Returns false, with nothing left registered, when the signals cannot be registered with
/// their origin or the thread cannot start; the caller then falls back to watching the signals without their origin.
pub(crate) fn spawn(on_quit: Arc<dyn Fn(ServerQuitSignal) + Send + Sync>) -> bool {
    let mut signals = match SignalsInfo::<WithOrigin>::new([SIGINT, SIGTERM, SIGHUP]) {
        Ok(signals) => signals,
        Err(err) => {
            warn!(%err, "cannot watch signals with their origin; signals are watched without it");
            return false;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("herdr-signals".into())
        .spawn(move || {
            for origin in signals.forever() {
                // Kernel-made signals (a hangup when the terminal closes) carry no sender; macOS fills in zeros.
                let sender = origin
                    .process
                    .filter(|process| process.pid > 0)
                    .map(|process| (process.pid as u32, process.uid));
                let origin = SignalOrigin {
                    signal: origin.signal,
                    sender,
                    cause: format!("{:?}", origin.cause),
                };
                log_surviving_panics(&origin, log_signal_origin);
                match origin.signal {
                    SIGINT => on_quit(ServerQuitSignal::Interrupt),
                    SIGTERM => on_quit(ServerQuitSignal::Terminate),
                    _ => info!("ignoring SIGHUP; server keeps running"),
                }
            }
        });
    if let Err(err) = &spawned {
        warn!(%err, "cannot start the signal thread; signals are watched without their origin");
    }
    spawned.is_ok()
}

fn log_signal_origin(origin: &SignalOrigin) {
    let name = signal_hook::low_level::signal_name(origin.signal)
        .map(str::to_string)
        .unwrap_or_else(|| format!("signal {}", origin.signal));
    crate::logging::server_signal_received(
        &name,
        origin.signal != SIGHUP,
        origin.sender,
        &origin.cause,
    );
}

/// Logs a signal under `catch_unwind`. The logging step must never end the watching thread: a watcher that is dropped
/// unregisters SIGINT, SIGTERM and SIGHUP without restoring the previous handler, and from then on the signals would do
/// nothing, so only SIGKILL, which skips the graceful save, could end the server. No panic is known to be reachable (the
/// command line is read with bounds checks and every failure becomes "unknown"), but this thread is the one place where a
/// surprise costs a server that holds every agent.
fn log_surviving_panics(origin: &SignalOrigin, log: impl FnOnce(&SignalOrigin)) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| log(origin)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_while_logging_a_signal_does_not_end_the_watching_thread() {
        let origin = SignalOrigin {
            signal: SIGTERM,
            sender: Some((1, 0)),
            cause: "Sent(User)".to_string(),
        };
        // The call returns: the watcher goes on to call `on_quit` for the signal.
        log_surviving_panics(&origin, |_| panic!("the log step failed"));
    }
}
