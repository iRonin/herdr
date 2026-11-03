use super::*;

/// The signal thread logs each stop signal and then asks the server to quit. If the logging panicked and took the
/// thread with it, SIGINT, SIGTERM and SIGHUP would be ignored for the rest of the server's life, so the quit must
/// happen whatever the log step does.
#[cfg(unix)]
#[test]
fn a_panic_while_logging_a_stop_signal_still_quits_the_server() {
    let should_quit = AtomicBool::new(false);
    let (tx, mut rx) = mpsc::channel(4);
    let origin = SignalOrigin {
        signal: 15,
        sender: Some((1, 0)),
        cause: "Sent(User)".to_string(),
    };

    quit_after_logging_signal(
        &origin,
        |_| panic!("the log step failed"),
        &should_quit,
        &tx,
    );

    assert!(
        should_quit.load(Ordering::Acquire),
        "the quit flag must be set even when logging the signal panicked"
    );
    assert!(
        matches!(rx.try_recv(), Ok(ServerEvent::QuitSignal)),
        "the event loop must be woken even when logging the signal panicked"
    );
}
