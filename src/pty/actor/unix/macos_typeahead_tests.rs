//! Ctrl-C through a real macOS pty whose input queue is full.
//!
//! A pty stops accepting input once about 1 KiB is unread. A Ctrl-C byte waiting behind
//! that backlog can then never be written, so a program that is not reading its input
//! cannot be interrupted.

use super::*;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// A 32-character shell command plus Enter: 33 bytes. A full tty holds 30 complete lines
/// (1022 bytes); herdr keeps whatever does not fit.
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

/// Waits until the tty holds `expected` unread bytes, or until the actor has gone idle twice
/// without the count changing (it is holding the rest itself). Returns the last count seen.
fn settle(fd: RawFd, expected: usize, idle: &std_mpsc::Receiver<()>) -> usize {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = unread_input(fd);
    let mut unchanged_idles = 0;
    while last != expected && Instant::now() < deadline {
        match idle.recv_timeout(Duration::from_millis(1500)) {
            Ok(()) | Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => panic!("PTY actor exited"),
        }
        let now = unread_input(fd);
        if now == last {
            unchanged_idles += 1;
            if unchanged_idles >= 2 {
                break;
            }
        } else {
            unchanged_idles = 0;
            last = now;
        }
    }
    last
}

/// Reads until `until` returns true for everything read so far, or the deadline passes.
fn read_until(fd: RawFd, timeout: Duration, until: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    while !until(&out) {
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
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        assert!(n >= 0, "read: {}", std::io::Error::last_os_error());
        out.extend_from_slice(&buf[..n as usize]);
    }
    out
}

/// `sleep` owns the pty as its controlling terminal and never reads it. `lines` are typed,
/// then Ctrl-C. Returns the tty's unread complete lines before the Ctrl-C (in bytes), how the
/// program ended within 5 s (None: still running), and the unread bytes afterwards.
fn ctrl_c_scenario(lines: usize) -> (usize, Option<String>, usize) {
    let mut command = portable_pty::CommandBuilder::new("/bin/sleep");
    command.arg("30");
    let mut spawned =
        crate::pty::backend::spawn_with_portable_pty(24, 80, command).expect("spawn sleep");
    let observer = fd::duplicate_cloexec_fd(spawned.master_fd.as_raw_fd()).expect("dup master");
    let observer = unsafe { OwnedFd::from_raw_fd(observer) };
    let (handle, idle) = spawn_actor(spawned.master_fd);

    type_lines(&handle, &idle, lines);
    // A full tty holds 30 complete lines (1022 bytes); herdr keeps the rest.
    let accepted_lines = lines.min(30);
    let unread = settle(observer.as_raw_fd(), accepted_lines * LINE.len(), &idle);
    assert_eq!(
        unread,
        accepted_lines * LINE.len(),
        "setup: the tty must hold {accepted_lines} unread lines"
    );

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
        // A paste arrives as one write, typing as many small ones.
        sent.push(&lines);
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
