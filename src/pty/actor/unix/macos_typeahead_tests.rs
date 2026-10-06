//! Typeahead through a real macOS pty.
//!
//! XNU keeps a tty's pending input in a 1024-byte ring. When a line editor such as bash's
//! readline returns the tty from raw to canonical mode (it does this before running every
//! command), the kernel re-feeds the unread raw input through its line discipline
//! (`ttypend()` in bsd/kern/tty.c). It writes that input back into the same ring it is
//! reading, and restarts at the ring's first slot after every line end. If the pending input
//! wraps past the ring's end, the reader receives the current line's first characters
//! repeated instead of the typed text.
//!
//! A pty also stops accepting input once about 1 KiB is unread. A Ctrl-C byte waiting behind
//! that backlog can then never be written, so a program that is not reading its input
//! cannot be interrupted.

use super::*;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// A 32-character shell command plus Enter: 33 bytes. A full tty holds 30 complete lines
/// (1022 bytes); herdr keeps whatever does not fit. 1024 = 31 * 33 + 1, so once 31 lines have
/// been typed into an empty tty, the next line starts in the ring's last slot.
const LINE: &[u8] = b"printf 'LOCAL_%s\\n' DIRECT_READY\r";

/// What a reader receives for one intact `LINE`: the tty maps the Enter (CR) to LF.
fn line_as_read() -> Vec<u8> {
    let mut line = LINE[..LINE.len() - 1].to_vec();
    line.push(b'\n');
    line
}

fn open_pty() -> (OwnedFd, OwnedFd) {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

fn termios_of(fd: RawFd) -> libc::termios {
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    assert_eq!(
        unsafe { libc::tcgetattr(fd, &mut termios) },
        0,
        "tcgetattr: {}",
        std::io::Error::last_os_error()
    );
    termios
}

fn set_termios(fd: RawFd, termios: &libc::termios) {
    assert_eq!(
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, termios) },
        0,
        "tcsetattr: {}",
        std::io::Error::last_os_error()
    );
}

/// The mode a line editor reads in: no canonical processing, no kernel echo, one byte at a time.
fn line_editor_mode(canonical: &libc::termios) -> libc::termios {
    let mut raw = *canonical;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    raw
}

/// Unread input the tty holds (FIONREAD): all of it in raw mode, complete lines in canonical.
fn unread_input(fd: RawFd) -> usize {
    let mut count: libc::c_int = 0;
    assert_eq!(
        unsafe { libc::ioctl(fd, libc::FIONREAD, &mut count) },
        0,
        "FIONREAD: {}",
        std::io::Error::last_os_error()
    );
    usize::try_from(count).expect("non-negative count")
}

fn spawn_actor(master: OwnedFd) -> (PtyIoActorHandle, std_mpsc::Receiver<()>) {
    let (idle_tx, idle_rx) = std_mpsc::channel();
    let handle = PtyIoActor::spawn_with_poll_observer(
        PtyIoActorConfig {
            pane_id: 1,
            master_fd: master,
            initially_quiesced: false,
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: None,
        },
        idle_tx,
    )
    .expect("actor spawn");
    (handle, idle_rx)
}

/// Spawns the actor, delivering everything the pane outputs to the returned channel.
fn spawn_actor_with_output(
    master: OwnedFd,
) -> (
    PtyIoActorHandle,
    std_mpsc::Receiver<()>,
    std_mpsc::Receiver<Vec<u8>>,
) {
    let (idle_tx, idle_rx) = std_mpsc::channel();
    let (output_tx, output_rx) = std_mpsc::channel();
    let handle = PtyIoActor::spawn_with_poll_observer(
        PtyIoActorConfig {
            pane_id: 1,
            master_fd: master,
            initially_quiesced: false,
            on_read: Box::new(move |bytes| {
                let _ = output_tx.send(bytes.to_vec());
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
        },
        idle_tx,
    )
    .expect("actor spawn");
    (handle, idle_rx, output_rx)
}

/// Collects pane output until `until` holds for everything collected, or the deadline passes.
fn collect_output(
    rx: &std_mpsc::Receiver<Vec<u8>>,
    timeout: Duration,
    until: impl Fn(&[u8]) -> bool,
) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    while !until(&out) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(std_mpsc::RecvTimeoutError::Timeout | std_mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }
    out
}

/// Queues input for the pane, waiting while the actor's command queue is momentarily full.
fn send(handle: &PtyIoActorHandle, bytes: &[u8]) {
    let mut bytes = Bytes::copy_from_slice(bytes);
    loop {
        match handle.try_write_user_input(bytes) {
            Ok(()) => return,
            Err(mpsc::error::TrySendError::Full(rejected)) => {
                bytes = rejected;
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => panic!("PTY actor closed its input"),
        }
    }
}

fn type_input(handle: &PtyIoActorHandle, idle: &std_mpsc::Receiver<()>, bytes: &[u8]) {
    while idle.try_recv().is_ok() {}
    send(handle, bytes);
}

fn type_lines(handle: &PtyIoActorHandle, idle: &std_mpsc::Receiver<()>, count: usize) {
    while idle.try_recv().is_ok() {}
    for _ in 0..count {
        send(handle, LINE);
    }
}

/// Waits until the tty holds `expected` unread bytes, or the deadline passes. Returns the last
/// count seen; callers assert on it, so a count that never arrives fails loudly.
fn settle(fd: RawFd, expected: usize, idle: &std_mpsc::Receiver<()>) -> usize {
    settle_until(fd, idle, |unread| unread == expected)
}

/// Waits until the tty's unread count is one `accept` takes, or 20 s pass; returns the last
/// count seen. It never stops early because the count looks steady: on a busy Mac the PTY
/// actor's writes can stall for seconds in the middle of a fill (ledger #58).
fn settle_until(fd: RawFd, idle: &std_mpsc::Receiver<()>, accept: impl Fn(usize) -> bool) -> usize {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let unread = unread_input(fd);
        if accept(unread) || Instant::now() >= deadline {
            return unread;
        }
        match idle.recv_timeout(Duration::from_millis(100)) {
            Ok(()) | Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => panic!("PTY actor exited"),
        }
    }
}

/// What the typeahead guard lets into a fresh tty that nobody reads: its whole ring budget.
fn guard_budget() -> usize {
    super::macos_typeahead_guard::RING_BUDGET
}

/// Reads until `until` returns true for everything read so far, or the deadline passes.
fn read_until(fd: RawFd, timeout: Duration, until: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    read_at_most(fd, usize::MAX, timeout, until)
}

/// Reads exactly `count` bytes (a raw-mode read returns everything available, so each read is
/// capped), or fewer if the deadline passes.
fn read_exactly(fd: RawFd, count: usize, timeout: Duration) -> Vec<u8> {
    read_at_most(fd, count, timeout, |read| read.len() >= count)
}

fn read_at_most(
    fd: RawFd,
    limit: usize,
    timeout: Duration,
    until: impl Fn(&[u8]) -> bool,
) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    while !until(&out) && out.len() < limit {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
        if unsafe { libc::poll(&mut pollfd, 1, millis) } <= 0 {
            continue;
        }
        let want = buf.len().min(limit - out.len());
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), want) };
        assert!(n >= 0, "read: {}", std::io::Error::last_os_error());
        out.extend_from_slice(&buf[..n as usize]);
    }
    out
}

fn count_line_ends(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
}

/// A line editor's pattern: it waits for a line in raw mode while 30 lines are typed ahead,
/// reads 10 of them, `typed_after_read` more lines arrive, and it returns the tty to canonical
/// mode (as bash does before running a command) and reads the rest. Returns the bytes unread
/// in the tty at the switch, and everything read after it.
fn line_editor_scenario(typed_after_read: usize) -> (usize, Vec<u8>) {
    let (master, slave) = open_pty();
    let slave = slave.as_raw_fd();
    let canonical = termios_of(slave);
    set_termios(slave, &line_editor_mode(&canonical));
    let (handle, idle) = spawn_actor(master);

    // 30 lines fill the ring's first 990 slots.
    type_lines(&handle, &idle, 30);
    assert_eq!(
        settle(slave, 30 * LINE.len(), &idle),
        30 * LINE.len(),
        "setup: the tty must accept 30 lines of typeahead"
    );
    let consumed = read_exactly(slave, 10 * LINE.len(), Duration::from_secs(5));
    assert_eq!(
        String::from_utf8_lossy(&consumed),
        String::from_utf8_lossy(&line_as_read().repeat(10)),
        "setup: the first 10 lines read back intact in raw mode"
    );
    // Unread input now starts at slot 330. More lines extend it: past slot 1023 means
    // the ring wraps.
    type_lines(&handle, &idle, typed_after_read);
    // Either the tty takes every typed line, or the typeahead guard stops at its budget: the
    // 30 lines used 990 slots, so it lets 10 more bytes through and holds the rest.
    let all_typed = (20 + typed_after_read) * LINE.len();
    let guard_held = 20 * LINE.len() + (guard_budget() - 30 * LINE.len());
    let unread = settle_until(slave, &idle, |unread| {
        unread == all_typed || unread == guard_held
    });
    assert!(
        unread == all_typed || unread == guard_held,
        "setup: the tty must hold every typed line ({all_typed} bytes) or what the guard lets \
         through ({guard_held}); it holds {unread}"
    );

    set_termios(slave, &canonical);
    // A final Enter closes any line the kernel left open.
    type_input(&handle, &idle, b"\r");
    let lines = 20 + typed_after_read + 1;
    let rest = read_until(slave, Duration::from_secs(5), |read| {
        count_line_ends(read) >= lines
    });
    handle.shutdown();
    (unread, rest)
}

fn assert_lines_intact(typed_after_read: usize, unread: usize, rest: &[u8]) {
    let intact = 20 + typed_after_read;
    let expected = [line_as_read().repeat(intact), b"\n".to_vec()].concat();
    let damaged: Vec<String> = rest
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| *line != line_as_read().as_slice() && *line != b"\n")
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect();
    assert!(
        rest == expected,
        "{intact} typed lines must reach the program intact after it returns the tty to \
         canonical mode; {unread} bytes were unread in the tty at the switch. Received {} \
         lines, damaged: {damaged:?}",
        count_line_ends(rest)
    );
}

#[test]
fn typeahead_survives_a_line_editor_returning_the_tty_to_canonical_mode() {
    // Two more lines put unread input in slots 330..1056: past the ring's end.
    let (unread, rest) = line_editor_scenario(2);
    assert_lines_intact(2, unread, &rest);
}

#[test]
fn typeahead_within_the_tty_ring_survives_a_return_to_canonical_mode() {
    // Control for the test above, passing with or without a guard: one more line puts
    // unread input in slots 330..1023, inside the ring, so the kernel's re-input is harmless.
    // If this fails, the scenario itself is broken, not the ring.
    let (unread, rest) = line_editor_scenario(1);
    assert_lines_intact(1, unread, &rest);
}

/// `sleep` owns the pty as its controlling terminal and never reads it. `lines` are typed,
/// then Ctrl-C. Returns the tty's unread complete lines before the Ctrl-C (in bytes), how the
/// program ended within 5 s (None: still running), and the unread bytes afterwards.
fn ctrl_c_scenario(lines: usize) -> (usize, Option<String>, usize) {
    ctrl_c_scenario_in(lines, false)
}

/// `ctrl_c_scenario`, with the tty in raw mode (ISIG still on) when `raw` is set.
fn ctrl_c_scenario_in(lines: usize, raw: bool) -> (usize, Option<String>, usize) {
    let mut command = portable_pty::CommandBuilder::new("/bin/sleep");
    command.arg("30");
    let mut spawned =
        crate::pty::backend::spawn_with_portable_pty(24, 80, command).expect("spawn sleep");
    let observer = fd::duplicate_cloexec_fd(spawned.master_fd.as_raw_fd()).expect("dup master");
    let observer = unsafe { OwnedFd::from_raw_fd(observer) };
    if raw {
        let canonical = termios_of(observer.as_raw_fd());
        set_termios(observer.as_raw_fd(), &line_editor_mode(&canonical));
    }
    let (handle, idle) = spawn_actor(spawned.master_fd);

    type_lines(&handle, &idle, lines);
    // A full tty holds 30 complete lines (1022 bytes); herdr keeps the rest.
    let accepted_lines = lines.min(30);
    let unread = if raw {
        // In raw mode the tty reports every unread byte, partial lines included. The guard
        // stops at exactly its budget, however the input is chunked: on a fresh pty it counts
        // every accepted byte and cuts the write that would pass the budget at the budget,
        // and the tty (which would take 1022) accepts all of it. Without a guard: 1022.
        let unread = settle(observer.as_raw_fd(), guard_budget(), &idle);
        assert_eq!(
            unread,
            guard_budget(),
            "setup: herdr, not the tty, must hold the rest while the tty has room"
        );
        unread
    } else {
        let unread = settle(observer.as_raw_fd(), accepted_lines * LINE.len(), &idle);
        assert_eq!(
            unread,
            accepted_lines * LINE.len(),
            "setup: the tty must hold {accepted_lines} unread lines"
        );
        unread
    };

    type_input(&handle, &idle, b"\x03");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut status = None;
    while status.is_none() && Instant::now() < deadline {
        status = spawned.child.try_wait().expect("child status");
        if status.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let unread_after = unread_input(observer.as_raw_fd());
    if status.is_none() {
        let _ = spawned.child.kill();
        let _ = spawned.child.wait();
    }
    handle.shutdown();
    let ended_by = status.map(|status| {
        status
            .signal()
            .map_or_else(|| format!("exit {}", status.exit_code()), str::to_owned)
    });
    (unread, ended_by, unread_after)
}

fn interrupt_signal_name() -> String {
    unsafe { std::ffi::CStr::from_ptr(libc::strsignal(libc::SIGINT)) }
        .to_string_lossy()
        .into_owned()
}

#[test]
fn ctrl_c_reaches_a_program_that_is_not_reading_its_input() {
    // 40 lines: the tty fills at 30 complete lines and herdr holds the rest, Ctrl-C included.
    let (unread, ended_by, unread_after) = ctrl_c_scenario(40);
    assert_eq!(
        ended_by,
        Some(interrupt_signal_name()),
        "Ctrl-C must interrupt a program that is not reading its input; with {unread} bytes of \
         typeahead unread, the program had not ended 5 s after the Ctrl-C (None = still running)"
    );
    assert_eq!(
        unread_after, 0,
        "Ctrl-C discards the unread typeahead, as the tty does for an interrupt"
    );
}

#[test]
fn ctrl_c_held_by_the_typeahead_guard_reaches_a_raw_mode_program() {
    // In raw mode the tty would take 1022 bytes before refusing input; herdr's typeahead
    // guard stops at 990 while nothing reads, so the tty still has room and never refuses
    // the Ctrl-C. The signal character must still act at once.
    let (unread, ended_by, unread_after) = ctrl_c_scenario_in(40, true);
    assert_eq!(
        ended_by,
        Some(interrupt_signal_name()),
        "Ctrl-C must interrupt a raw-mode program that is not reading its input; with {unread} \
         bytes of typeahead unread, the program had not ended 5 s after the Ctrl-C"
    );
    assert_eq!(
        unread_after, 0,
        "the interrupt discards the unread typeahead"
    );
}

#[test]
fn ctrl_c_with_little_typeahead_interrupts_the_program() {
    // Control for the test above, passing with or without a fix: with room in the tty the
    // Ctrl-C byte is written at once and the kernel signals the program. If this fails, the
    // scenario itself is broken.
    let (unread, ended_by, unread_after) = ctrl_c_scenario(5);
    assert_eq!(
        ended_by,
        Some(interrupt_signal_name()),
        "{unread} bytes unread"
    );
    assert_eq!(
        unread_after, 0,
        "the interrupt discards the unread typeahead"
    );
}

#[test]
fn ctrl_c_stays_in_order_as_data_when_isig_is_off() {
    // A raw-mode program (an editor or a TUI) turns ISIG off: 0x03 is then ordinary input
    // and must keep its place even when the tty is full and herdr holds a backlog. An
    // out-of-band delivery here would drop or reorder user input.
    let (master, slave) = open_pty();
    let slave = slave.as_raw_fd();
    let mut modes = termios_of(slave);
    modes.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
    modes.c_iflag &= !libc::IXON;
    set_termios(slave, &modes);
    let (handle, idle) = spawn_actor(master);

    let mut expected: Vec<u8> = line_as_read().repeat(40);
    type_lines(&handle, &idle, 40);
    type_input(&handle, &idle, b"\x03");
    expected.push(b'\x03');
    type_lines(&handle, &idle, 5);
    expected.extend(line_as_read().repeat(5));
    // Give the actor time with a full tty: a wrong bypass would fire here.
    std::thread::sleep(Duration::from_secs(1));
    let expected_len = expected.len();
    let received = read_until(slave, Duration::from_secs(10), |read| {
        read.len() >= expected_len
    });
    handle.shutdown();
    assert!(
        received == expected,
        "with ISIG off, 0x03 is data: received {} of {} bytes; first difference at byte {:?}",
        received.len(),
        expected.len(),
        received
            .iter()
            .zip(expected.iter())
            .position(|(got, want)| got != want)
    );
}

#[test]
fn ctrl_c_with_noflsh_interrupts_and_keeps_the_held_input() {
    // With NOFLSH the kernel delivers the interrupt but does not flush the queues, so the
    // signal must arrive while the program is not reading AND the held input must be kept:
    // every typed line reaches the program once it reads. Without a bypass the Ctrl-C
    // cannot enter the full tty while nothing reads, so the signal never arrives in time.
    let mut command = portable_pty::CommandBuilder::new("/bin/sh");
    command.arg("-c");
    // STARTED synchronizes the test with the shell: it is printed only after the trap is
    // installed AND sleep is forked into the shell's process group. Without that, the
    // interrupt can land before the trap exists (killing the shell), or after the trap but
    // before sleep is forked (the signal reaches only the shell, which defers the trap
    // until sleep's 30 s are up). The sleep is long: without a bypass the Ctrl-C cannot
    // enter the tty while nothing reads, so the signal never arrives within the deadline.
    command.arg("trap 'echo GOT_SIGNAL' INT; sleep 30 & echo STARTED; wait; cat");
    let mut spawned =
        crate::pty::backend::spawn_with_portable_pty(24, 80, command).expect("spawn sh");
    let observer = fd::duplicate_cloexec_fd(spawned.master_fd.as_raw_fd()).expect("dup master");
    let observer = unsafe { OwnedFd::from_raw_fd(observer) };
    let mut modes = termios_of(observer.as_raw_fd());
    modes.c_lflag &= !libc::ECHO;
    modes.c_lflag |= libc::NOFLSH;
    set_termios(observer.as_raw_fd(), &modes);
    let (handle, idle, output) = spawn_actor_with_output(spawned.master_fd);

    let ready = collect_output(&output, Duration::from_secs(5), |out| {
        out.windows(b"STARTED".len())
            .any(|window| window == b"STARTED")
    });
    let is_ready = ready
        .windows(b"STARTED".len())
        .any(|window| window == b"STARTED");
    if !is_ready {
        let _ = spawned.child.kill();
        let _ = spawned.child.wait();
        handle.shutdown();
        panic!("setup: the shell did not install its trap and start within 5 s");
    }

    type_lines(&handle, &idle, 40);
    let unread = settle(observer.as_raw_fd(), 30 * LINE.len(), &idle);
    assert_eq!(
        unread,
        30 * LINE.len(),
        "setup: the tty must hold 30 unread lines"
    );

    type_input(&handle, &idle, b"\x03");
    // sh runs cat after the sleep: all 40 lines, kept, arrive complete and in order.
    // Output maps LF to CR-LF on the wire.
    let mut line_on_the_wire = LINE[..LINE.len() - 1].to_vec();
    line_on_the_wire.extend_from_slice(b"\r\n");
    let lines = line_on_the_wire.repeat(40);
    let collected = collect_output(&output, Duration::from_secs(15), |out| {
        out.windows(b"GOT_SIGNAL".len()).any(|w| w == b"GOT_SIGNAL")
            && out.windows(lines.len()).any(|w| w == lines.as_slice())
    });
    let _ = spawned.child.kill();
    let _ = spawned.child.wait();
    handle.shutdown();

    let signal_at = collected
        .windows(b"GOT_SIGNAL".len())
        .position(|window| window == b"GOT_SIGNAL");
    let first_line_at = collected
        .windows(line_on_the_wire.len())
        .position(|window| window == line_on_the_wire.as_slice());
    assert!(
        signal_at.is_some(),
        "with NOFLSH the interrupt must still be delivered past a full tty"
    );
    assert!(
        first_line_at.is_some(),
        "with NOFLSH the held input must be kept: all 40 typed lines must reach the program \
         complete and in order"
    );
    assert!(
        signal_at < first_line_at,
        "the interrupt must arrive while the program is not reading (before the kept lines); \
         without the bypass it arrives only after the program has read the backlog"
    );
}

#[test]
fn held_vstop_and_vstart_control_output_past_a_full_queue() {
    // With IXON, VSTOP/VSTART never enter the kernel's input queue, so herdr moves them
    // ahead of a held backlog (TIOCSTOP/TIOCSTART when the tty refuses the byte; verified
    // to stop and restart output with the input queue full). A program that is not reading
    // must still have its output stopped and restarted.
    let mut command = portable_pty::CommandBuilder::new("/bin/sh");
    command.arg("-c");
    command.arg("while true; do echo pulse; sleep 0.02; done");
    let mut spawned =
        crate::pty::backend::spawn_with_portable_pty(24, 80, command).expect("spawn sh");
    let observer = fd::duplicate_cloexec_fd(spawned.master_fd.as_raw_fd()).expect("dup master");
    let observer = unsafe { OwnedFd::from_raw_fd(observer) };
    let mut modes = termios_of(observer.as_raw_fd());
    modes.c_iflag |= libc::IXON;
    modes.c_lflag &= !libc::ALTWERASE;
    set_termios(observer.as_raw_fd(), &modes);
    let (handle, idle, output) = spawn_actor_with_output(spawned.master_fd);

    let flowing = collect_output(&output, Duration::from_secs(3), |out| !out.is_empty());
    assert!(
        !flowing.is_empty(),
        "setup: the program must be producing output"
    );

    type_lines(&handle, &idle, 40);
    let unread = settle(observer.as_raw_fd(), 30 * LINE.len(), &idle);
    assert_eq!(
        unread,
        30 * LINE.len(),
        "setup: the tty must hold 30 unread lines"
    );

    type_input(&handle, &idle, b"\x13"); // VSTOP

    // Drain output already in flight, then the flow must go silent.
    let _ = collect_output(&output, Duration::from_millis(500), |_| false);
    let stopped = collect_output(&output, Duration::from_secs(1), |out| !out.is_empty()).is_empty();

    type_input(&handle, &idle, b"\x11"); // VSTART
    let resumed = collect_output(&output, Duration::from_secs(3), |out| !out.is_empty());

    let _ = spawned.child.kill();
    let _ = spawned.child.wait();
    handle.shutdown();
    assert!(
        stopped,
        "a held VSTOP must stop the program's output even when the tty's input queue is full"
    );
    assert!(
        !resumed.is_empty(),
        "a held VSTART must restart the program's output"
    );
}

#[test]
fn flow_bytes_stay_in_order_as_data_when_ixon_is_off() {
    // IXON is an input flag (c_iflag); the same bit in c_lflag is ALTWERASE. With IXON off
    // and ALTWERASE on, 0x11/0x13 are ordinary input and must keep their place: checking
    // IXON in the wrong termios field would drop or reorder them here.
    let (master, slave) = open_pty();
    let slave = slave.as_raw_fd();
    let mut modes = termios_of(slave);
    modes.c_lflag &= !(libc::ICANON | libc::ECHO);
    modes.c_lflag |= libc::ALTWERASE;
    modes.c_iflag &= !libc::IXON;
    set_termios(slave, &modes);
    let (handle, idle) = spawn_actor(master);

    let mut expected: Vec<u8> = line_as_read().repeat(40);
    type_lines(&handle, &idle, 40);
    type_input(&handle, &idle, b"\x13");
    expected.push(b'\x13');
    type_input(&handle, &idle, b"\x11");
    expected.push(b'\x11');
    type_lines(&handle, &idle, 5);
    expected.extend(line_as_read().repeat(5));
    // Give the actor time with a full tty: a wrong bypass would fire here.
    std::thread::sleep(Duration::from_secs(1));
    let expected_len = expected.len();
    let received = read_until(slave, Duration::from_secs(10), |read| {
        read.len() >= expected_len
    });
    handle.shutdown();
    assert!(
        received == expected,
        "with IXON off, 0x11/0x13 are data: received {} of {} bytes; first difference at byte {:?}",
        received.len(),
        expected.len(),
        received
            .iter()
            .zip(expected.iter())
            .position(|(got, want)| got != want)
    );
}

#[test]
fn input_to_a_program_that_keeps_reading_arrives_complete_and_in_order() {
    // Must keep passing under any typeahead handling: nothing may be lost, reordered or
    // stalled while the reader keeps up, whether it reads raw (an editor or agent TUI) or
    // line by line.
    let long_run = vec![b'x'; 5000];
    let lines = (0..2000)
        .map(|index| format!("typed line {index:05} of the burst\r"))
        .collect::<String>()
        .into_bytes();
    for canonical_reader in [false, true] {
        let (master, slave) = open_pty();
        let slave = slave.as_raw_fd();
        let canonical = termios_of(slave);
        if !canonical_reader {
            set_termios(slave, &line_editor_mode(&canonical));
        }
        let (handle, _idle) = spawn_actor(master);
        let mut expected = Vec::new();
        let mut sent: Vec<&[u8]> = Vec::new();
        if !canonical_reader {
            // A long run without any line end, as in a paste into an editor.
            sent.push(&long_run);
        }
        // A paste arrives as one write, typing as many small ones. Bracketed-paste markers
        // keep their place around the paste.
        sent.push(b"\x1b[200~");
        sent.push(&lines);
        sent.push(b"\x1b[201~");
        for chunk in lines.chunks(33) {
            sent.push(chunk);
        }
        for chunk in &sent {
            expected.extend(
                chunk
                    .iter()
                    .map(|byte| if *byte == b'\r' { b'\n' } else { *byte }),
            );
        }
        let expected_len = expected.len();
        let reader = std::thread::spawn(move || {
            read_until(slave, Duration::from_secs(20), |read| {
                read.len() >= expected_len
            })
        });
        for chunk in sent {
            send(&handle, chunk);
        }
        let received = reader.join().expect("reader thread");
        handle.shutdown();
        assert!(
            received == expected,
            "canonical reader: {canonical_reader}: received {} of {} bytes; first difference at \
             byte {:?}",
            received.len(),
            expected.len(),
            received
                .iter()
                .zip(expected.iter())
                .position(|(got, want)| got != want)
        );
    }
}

/// Spawns the actor the way a live handoff's receiving server does (quiesced, so nothing is
/// known about the input already in the tty), then lets it run.
fn spawn_handoff_receiver(
    master: OwnedFd,
) -> (
    PtyIoActorHandle,
    std_mpsc::Receiver<()>,
    std_mpsc::Receiver<Vec<u8>>,
) {
    let (idle_tx, idle_rx) = std_mpsc::channel();
    let (output_tx, output_rx) = std_mpsc::channel();
    let handle = PtyIoActor::spawn_with_poll_observer(
        PtyIoActorConfig {
            pane_id: 1,
            master_fd: master,
            initially_quiesced: true,
            on_read: Box::new(move |bytes| {
                let _ = output_tx.send(bytes.to_vec());
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
        },
        idle_tx,
    )
    .expect("actor spawn");
    handle.rollback_handoff().expect("receiver starts running");
    (handle, idle_rx, output_rx)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn keys_and_small_pastes_are_written_without_reading_the_tty_state() {
    // Goal: nothing changes while unread input is small. 900 keys into a tty nobody reads stay
    // under the guard's budget, so the guard never reads the tty's state (no added syscalls),
    // and every key reaches the tty.
    let (master, slave) = open_pty();
    let slave = slave.as_raw_fd();
    let canonical = termios_of(slave);
    set_termios(slave, &line_editor_mode(&canonical));
    let (handle, idle) = spawn_actor(master);
    while idle.try_recv().is_ok() {}
    for _ in 0..900 {
        send(&handle, b"k");
    }
    assert_eq!(settle(slave, 900, &idle), 900, "every key reaches the tty");
    assert_eq!(
        handle
            .typeahead_checks
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "900 keys must not make the guard read the tty's state"
    );

    // Sustained typing into a program that keeps reading: about one look per KiB.
    let reader = std::thread::spawn(move || {
        read_until(slave, Duration::from_secs(20), |read| {
            read.len() >= 900 + 4000
        })
    });
    for _ in 0..4000 {
        send(&handle, b"k");
    }
    let received = reader.join().expect("reader thread");
    let checks = handle
        .typeahead_checks
        .load(std::sync::atomic::Ordering::Relaxed);
    handle.shutdown();
    assert_eq!(received.len(), 4900, "every key reaches the program");
    assert!(
        checks <= 100,
        "4900 keys into a reading program made the guard read the tty's state {checks} times"
    );
}

#[test]
fn a_long_unfinished_line_can_still_be_erased_and_ended() {
    // A line longer than the guard's budget, never ended, is followed by Ctrl-U (erase the
    // line) and a new line. Holding until the long line ends would hold its own Ctrl-U and
    // Enter forever: the program must receive the new line.
    let (master, slave) = open_pty();
    let slave = slave.as_raw_fd();
    let (handle, idle) = spawn_actor(master);
    type_input(&handle, &idle, &[b'x'; 1500]);
    type_input(&handle, &idle, b"\x15ok\r");
    let received = read_until(slave, Duration::from_secs(5), |read| read.contains(&b'\n'));
    handle.shutdown();
    assert_eq!(
        String::from_utf8_lossy(&received),
        "ok\n",
        "Ctrl-U and Enter after a 1500-character unfinished line must reach the program"
    );
}

#[test]
fn a_reader_waiting_for_several_bytes_receives_everything() {
    // A raw reader with VMIN > 1 and VTIME = 0: the tty reports no unread input until VMIN
    // bytes are there, so emptiness cannot be seen. Holding input here could starve the
    // reader; everything must arrive.
    let (master, slave) = open_pty();
    let slave = slave.as_raw_fd();
    let mut modes = line_editor_mode(&termios_of(slave));
    modes.c_cc[libc::VMIN] = 4;
    set_termios(slave, &modes);
    let (handle, _idle) = spawn_actor(master);
    let reader = std::thread::spawn(move || {
        read_until(slave, Duration::from_secs(10), |read| read.len() >= 1500)
    });
    for _ in 0..3 {
        send(&handle, &[b'y'; 500]);
    }
    let received = reader.join().expect("reader thread");
    handle.shutdown();
    assert_eq!(
        received.len(),
        1500,
        "a VMIN=4 reader must receive all 1500 bytes"
    );
}

#[test]
fn handoff_drains_input_the_typeahead_guard_is_holding() {
    // The tty holds 30 unread lines and has room for 15 more bytes, but the guard holds
    // them. A live handoff drains the queued input before it moves the pane: it must not
    // wait on the guard.
    let (master, slave) = open_pty();
    let slave = slave.as_raw_fd();
    let (handle, idle) = spawn_actor(master);
    type_lines(&handle, &idle, 30);
    assert_eq!(settle(slave, 30 * LINE.len(), &idle), 30 * LINE.len());
    let checks_before = handle
        .typeahead_checks
        .load(std::sync::atomic::Ordering::Relaxed);
    type_input(&handle, &idle, b"echo held line\r");
    // The guard writes 10 bytes within its budget, then reads the tty's state and holds the
    // rest: wait for that look rather than for a count that has merely stopped changing.
    let deadline = Instant::now() + Duration::from_secs(20);
    while handle
        .typeahead_checks
        .load(std::sync::atomic::Ordering::Relaxed)
        == checks_before
        && Instant::now() < deadline
    {
        let _ = idle.recv_timeout(Duration::from_millis(100));
    }
    let checks_after = handle
        .typeahead_checks
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        checks_after > checks_before,
        "setup: the guard must read the tty's state before the 15-byte line passes its budget"
    );
    // Two fresh idle signals: the actor has finished at least one whole loop, write attempt
    // included, after the guard decided to hold. A guard that wrote the line anyway would have
    // written it by then.
    while idle.try_recv().is_ok() {}
    for _ in 0..2 {
        idle.recv_timeout(Duration::from_secs(20))
            .expect("setup: the PTY actor must keep looping while the guard holds input");
    }
    assert_eq!(
        unread_input(slave),
        30 * LINE.len(),
        "setup: the guard must hold the 15-byte line while the tty has room"
    );
    let handoff = handle.begin_handoff(Duration::from_secs(5));
    let unread = unread_input(slave);
    handle.shutdown();
    handoff.expect("the handoff drains the held line");
    assert_eq!(
        unread,
        30 * LINE.len() + 15,
        "the held line reached the tty"
    );
}

#[test]
fn a_handoff_receiver_lets_a_line_typed_before_the_handoff_continue() {
    // A line was half typed before a live handoff, so it sits unfinished in the tty and the
    // receiving server knows nothing about it. Further keys of that line must reach the tty at
    // once, not wait for a line end the user has not typed.
    let (master, slave) = open_pty();
    let slave = slave.as_raw_fd();
    let typed_before = b"echo par";
    assert_eq!(
        unsafe {
            libc::write(
                master.as_raw_fd(),
                typed_before.as_ptr().cast(),
                typed_before.len(),
            )
        },
        typed_before.len() as isize
    );
    let (handle, idle, output) = spawn_handoff_receiver(master);
    type_input(&handle, &idle, b"tial");
    // The tty echoes what it receives: "tial" echoed means it arrived.
    let echoed = collect_output(&output, Duration::from_secs(3), |out| {
        contains(out, b"tial")
    });
    type_input(&handle, &idle, &[b'x'; 1200]);
    type_input(&handle, &idle, b"\x15ok\r");
    let received = read_until(slave, Duration::from_secs(5), |read| read.contains(&b'\n'));
    handle.shutdown();
    assert!(
        contains(&echoed, b"tial"),
        "keys continuing a line typed before the handoff must reach the tty without a line end"
    );
    assert_eq!(
        String::from_utf8_lossy(&received),
        "ok\n",
        "Ctrl-U and Enter after a long unfinished line must reach the program"
    );
}

#[test]
fn an_agent_submission_larger_than_the_tty_ring_arrives_intact() {
    // An agent submission (text, a pause, then Enter) of 40 lines into a program that reads
    // line by line, slowly: the guard holds part of the text, and Enter must still follow
    // the complete text.
    let (master, slave) = open_pty();
    let slave = slave.as_raw_fd();
    let mut modes = termios_of(slave);
    modes.c_lflag &= !libc::ECHO;
    set_termios(slave, &modes);
    let (handle, _idle) = spawn_actor(master);
    let text = LINE.repeat(40);
    let reply = handle
        .queue_user_input_submission(
            Bytes::from(text),
            Bytes::from_static(b"\r"),
            Duration::from_millis(10),
        )
        .expect("submission queued");
    let mut received = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while count_line_ends(&received) < 41 && Instant::now() < deadline {
        // One line at a time, with a pause: a slow line-by-line reader.
        let line = read_until(slave, Duration::from_millis(500), |read| {
            read.contains(&b'\n')
        });
        received.extend_from_slice(&line);
        std::thread::sleep(Duration::from_millis(5));
    }
    let outcome = reply.recv_timeout(Duration::from_secs(5));
    handle.shutdown();
    let expected = [line_as_read().repeat(40), b"\n".to_vec()].concat();
    assert!(
        received == expected,
        "received {} of {} bytes; first difference at byte {:?}",
        received.len(),
        expected.len(),
        received
            .iter()
            .zip(expected.iter())
            .position(|(got, want)| got != want)
    );
    assert!(
        matches!(outcome, Ok(Ok(()))),
        "the submission completes: {outcome:?}"
    );
}

#[test]
fn settle_waits_through_a_stall_in_the_middle_of_a_fill() {
    // The tests' `settle` helper must wait for the count it was asked for. On a busy Mac the
    // PTY actor's writes can stall for seconds in the middle of a fill; a helper that stops as
    // soon as the count looks steady then returns a partial count and the test fails in its
    // own setup (ledger #58: 363 of 990 bytes, at load about 198). Here the input itself stops
    // for 5 s after 10 of 30 lines, longer than any steadiness window the helper could use.
    let (master, slave) = open_pty();
    let slave = slave.as_raw_fd();
    let (handle, idle) = spawn_actor(master);
    while idle.try_recv().is_ok() {}
    let feeder = {
        let handle = handle.clone();
        std::thread::spawn(move || {
            for _ in 0..10 {
                send(&handle, LINE);
            }
            std::thread::sleep(Duration::from_secs(5));
            for _ in 0..20 {
                send(&handle, LINE);
            }
        })
    };
    let unread = settle(slave, 30 * LINE.len(), &idle);
    feeder.join().expect("feeder thread");
    handle.shutdown();
    assert_eq!(
        unread,
        30 * LINE.len(),
        "settle must wait through a 5 s stall for all 30 lines, not return a partial count"
    );
}
