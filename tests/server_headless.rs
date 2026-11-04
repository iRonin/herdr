//! Integration tests for headless server mode.

#![cfg(unix)]

pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::{
    cleanup_test_base, client_handshake, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid, CURRENT_PROTOCOL,
};

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/herdr-server-test-{}-{nanos}",
        std::process::id()
    ))
}

struct SpawnedHerdr {
    _master: Option<Box<dyn MasterPty + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl SpawnedHerdr {
    fn close_master(&mut self) {
        drop(self._master.take());
    }
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        self.close_master();

        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }

            unregister_spawned_herdr_pid(Some(pid));
        }
    }
}

fn cleanup_spawned_herdr(spawned: SpawnedHerdr, base: PathBuf) {
    drop(spawned);
    cleanup_test_base(&base);
}

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not appear at {}", path.display());
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not accept connections at {}", path.display());
}

fn spawn_server(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket_path: &Path,
    client_socket_path: &Path,
) -> SpawnedHerdr {
    spawn_server_with_env(
        config_home,
        runtime_dir,
        api_socket_path,
        client_socket_path,
        &[],
    )
}

fn spawn_server_with_env(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket_path: &Path,
    _client_socket_path: &Path,
    extra_env: &[(&str, &Path)],
) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(
        config_home.join("herdr/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));

    support::sanitize_herdr_env(&mut cmd);
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", api_socket_path);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    SpawnedHerdr {
        _master: Some(pair.master),
        child,
    }
}

/// The server's own log file: where it says why it stopped.
fn server_log(config_home: &Path) -> String {
    let app_dir = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    fs::read_to_string(config_home.join(app_dir).join("herdr-server.log")).unwrap_or_default()
}

/// Waits until the server log holds a line with `needle`; panics with the log when it does not within the timeout.
fn wait_for_server_log_line(config_home: &Path, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if server_log(config_home).contains(needle) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "the server log has no line with {needle:?} after {timeout:?}:\n{}",
        server_log(config_home)
    );
}

/// Waits until the pty child (the server) has exited; false when it is still running at the deadline.
fn wait_for_server_exit(spawned: &mut SpawnedHerdr, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if spawned.child.try_wait().ok().flatten().is_some() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

fn ping_socket(socket_path: &Path) -> String {
    let mut stream = UnixStream::connect(socket_path).expect("should connect to API socket");

    let request = r#"{"id":"1","method":"ping","params":{}}"#;
    writeln!(stream, "{}", request).unwrap();

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    response.trim().to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn server_creates_both_sockets() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);

    // Wait for both sockets to appear.
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Verify the client socket is a socket file.
    let metadata = fs::metadata(&client_socket).unwrap();
    let file_type = metadata.file_type();
    assert!(
        file_type.is_socket(),
        "client socket should be a socket file"
    );

    // Verify the API socket works.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "ping should return pong: {response}"
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn server_starts_without_terminal() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);

    // Wait for the API socket to appear — proves the server started.
    wait_for_socket(&api_socket, Duration::from_secs(10));

    // The server process should be running.
    if let Some(pid) = spawned.child.process_id() {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        assert_eq!(result, 0, "server process should be running");
    }

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn server_api_responds_to_ping() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    // Ping the API socket.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API should respond to ping: {response}"
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn server_removes_client_socket_on_exit() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Kill the server.
    let _ = spawned.child.kill();
    spawned.close_master();
    let _ = spawned.child.wait();

    // Give it a moment to clean up.
    thread::sleep(Duration::from_millis(300));

    // The client socket should be removed (best-effort by Drop).
    // If it still exists, it should be stale (not connectable).
    if client_socket.exists() {
        assert!(
            UnixStream::connect(&client_socket).is_err(),
            "stale client socket should not accept connections"
        );
    }

    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn server_cleans_up_stale_client_socket() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    // Create a stale client socket file (simulating a crashed server).
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    {
        let _listener = std::os::unix::net::UnixListener::bind(&client_socket).unwrap();
        // Drop the listener so the socket becomes stale.
    }

    // Now start the server — it should clean up the stale socket.
    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    // The API should work.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API should respond to ping: {response}"
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn server_persists_after_client_disconnect() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Connect to the client socket and then immediately disconnect.
    {
        let _stream = UnixStream::connect(&client_socket).expect("should connect to client socket");
        // Immediately drop the connection.
    }

    // Give the server a moment to process the disconnect.
    thread::sleep(Duration::from_millis(200));

    // The server should still be running and the API should still respond.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API should still respond after client disconnect: {response}"
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn duplicate_server_start_fails_gracefully() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    // Start the first server.
    let spawned1 = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    // Try to start a second server — it should fail.
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));

    support::sanitize_herdr_env(&mut cmd);
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", &config_home);
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", &api_socket);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");

    let mut child2 = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child2.process_id());
    drop(pair.slave);

    // Wait for the second server to exit.
    let exit_status = child2.wait().unwrap();
    unregister_spawned_herdr_pid(child2.process_id());

    // The second server should exit with a non-zero code.
    assert!(!exit_status.success(), "duplicate server start should fail");

    cleanup_spawned_herdr(spawned1, base);
}

#[test]
fn client_handshake_succeeds() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Connect to the client socket and perform a handshake.
    let mut stream = UnixStream::connect(&client_socket).expect("should connect to client socket");

    // Send Hello with the current protocol version, 80 cols, 24 rows.
    let (version, error) =
        client_handshake(&mut stream, CURRENT_PROTOCOL, 80, 24).expect("handshake should succeed");

    assert_eq!(
        version, CURRENT_PROTOCOL,
        "server should report current protocol version"
    );
    assert!(
        error.is_none(),
        "handshake should not have an error: {:?}",
        error
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn client_handshake_rejects_incompatible_version() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Connect to the client socket and send Hello with version 0 (pre-persistence).
    let mut stream = UnixStream::connect(&client_socket).expect("should connect to client socket");

    let (version, error) = client_handshake(&mut stream, 0, 80, 24)
        .expect("should read Welcome response even on rejection");

    assert_eq!(
        version, CURRENT_PROTOCOL,
        "server should report its current protocol version"
    );
    assert!(
        error.is_some(),
        "version 0 should be rejected with an error"
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn client_handshake_clamps_small_terminal_size() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Send Hello with 0x0 terminal size — should be clamped.
    let mut stream = UnixStream::connect(&client_socket).expect("should connect to client socket");

    let (version, error) = client_handshake(&mut stream, CURRENT_PROTOCOL, 0, 0)
        .expect("handshake with 0x0 should succeed (server clamps)");

    assert_eq!(version, CURRENT_PROTOCOL);
    assert!(
        error.is_none(),
        "0x0 size should be accepted (clamped): {:?}",
        error
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn no_hello_client_closed_within_five_seconds() {
    // Client connection that sends no Hello is closed within 5 seconds.
    // The server sets a handshake timeout of 4 seconds to guarantee the connection
    // is closed within the 5-second deadline even with OS overhead.
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Connect but don't send Hello — just a raw connection.
    let mut stream = UnixStream::connect(&client_socket).expect("should connect to client socket");

    // Set a read timeout longer than the handshake timeout so we can detect
    // when the server closes the connection.
    stream
        .set_read_timeout(Some(Duration::from_secs(6)))
        .unwrap();

    let start = Instant::now();

    // Try to read from the stream. The server should close the connection
    // within 5 seconds, causing our read to return with an error (EOF or
    // connection reset).
    let mut buf = [0u8; 1024];
    let result = stream.read(&mut buf);
    let elapsed = start.elapsed();

    // The read should fail (connection closed by server).
    assert!(
        result.is_err() || result.unwrap() == 0,
        "server should close the connection when no Hello is sent"
    );

    // The connection should be closed within 5 seconds.
    assert!(
        elapsed < Duration::from_secs(5),
        "connection should be closed within 5 seconds, took {:?}",
        elapsed
    );

    // Verify the server is still healthy — a proper client can still connect.
    let mut good_stream =
        UnixStream::connect(&client_socket).expect("should connect after no-hello client");
    let (version, error) = client_handshake(&mut good_stream, CURRENT_PROTOCOL, 80, 24)
        .expect("proper handshake should still work after no-hello client");
    assert_eq!(version, CURRENT_PROTOCOL);
    assert!(error.is_none());

    // API should still work.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "server should still respond to ping: {response}"
    );

    cleanup_spawned_herdr(spawned, base);
}

/// Servers have "stopped with no request in the log". The generic request log records `server.stop` at DEBUG only, so
/// an API stop left nothing at the default level. The server must now say who asked, and the sender is identified by the
/// kernel (the peer credentials of the connection), not by anything the sender claims.
#[test]
fn server_stop_over_the_api_logs_the_process_that_asked() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // A process this test starts, so its id is known here: the stop command of the CLI.
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_herdr"));
    support::sanitize_herdr_env_command(&mut command);
    command
        .args(["server", "stop"])
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("HERDR_SOCKET_PATH", &api_socket)
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut requester = command.spawn().expect("spawn the stop command");
    let requester_pid = requester.id();
    let _ = requester.wait();

    assert!(
        wait_for_server_exit(&mut spawned, Duration::from_secs(10)),
        "the server must stop when asked"
    );
    let log = server_log(&config_home);
    let line = log
        .lines()
        .find(|line| line.contains("server.stop.requested"))
        .unwrap_or_else(|| panic!("the server log has no line about the stop request:\n{log}"));
    assert!(
        line.contains(&format!("requester_pid={requester_pid} ")),
        "the stop line must name the process that asked ({requester_pid}): {line}"
    );
    // The command line is the field's own value. A bare search for "server stop" in the line proves nothing: the
    // event's message ("server stop requested over the api") contains it, so it passed with `requester_cmd=unknown`.
    let requester_cmd = line
        .split("requester_cmd=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("the stop line must carry a quoted requester_cmd: {line}"));
    assert_eq!(
        requester_cmd,
        format!("{} server stop", env!("CARGO_BIN_EXE_herdr")),
        "the stop line must carry the sender's command line: {line}"
    );

    drop(spawned);
    cleanup_test_base(&base);
}

/// A signal that stops the server must be logged with its name and its sender, and must still stop the server.
/// The sender is a shell this test starts, so its process id is known here; the shell stays alive for a moment
/// after the signal so its command line can still be read. The line must come before the shutdown starts.
fn assert_signal_is_logged_with_its_sender(signal: &str, name: &str) {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));
    let server_pid = spawned.child.process_id().expect("server pid");
    // The client socket accepts connections before the server's signal watcher exists (the watcher is set up when the
    // event loop starts). A signal sent in between takes the default action and kills the server with no log line, so
    // wait for the line the server writes once the watcher is registered.
    wait_for_server_log_line(
        &config_home,
        "server.signal.handler.installed",
        Duration::from_secs(20),
    );

    let mut sender = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("kill -{signal} {server_pid}; sleep 2"))
        .spawn()
        .expect("spawn the sending shell");
    let sender_pid = sender.id();

    assert!(
        wait_for_server_exit(&mut spawned, Duration::from_secs(10)),
        "{name} must still stop the server"
    );
    let _ = sender.kill();
    let _ = sender.wait();

    let log = server_log(&config_home);
    let lines: Vec<&str> = log.lines().collect();
    let signal_at = lines
        .iter()
        .position(|line| line.contains("server.signal.received"))
        .unwrap_or_else(|| panic!("the server log has no line about the signal:\n{log}"));
    let line = lines[signal_at];
    assert!(
        line.contains(&format!("signal=\"{name}\"")),
        "the line must name the signal {name}: {line}"
    );
    assert!(
        line.contains(&format!("sender_pid={sender_pid} ")),
        "the line must name the process that sent it ({sender_pid}): {line}"
    );
    assert!(
        line.contains(&format!("sender_uid={} ", unsafe { libc::getuid() })),
        "the line must carry the sender's user id: {line}"
    );
    assert!(
        line.contains(&format!("kill -{signal}")),
        "the line must carry the sender's command line: {line}"
    );
    let shutdown_at = lines
        .iter()
        .position(|line| line.contains("server shutdown initiated"))
        .unwrap_or_else(|| panic!("the server did not shut down gracefully:\n{log}"));
    assert!(
        signal_at < shutdown_at,
        "the signal must be logged before the shutdown starts:\n{log}"
    );

    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn sigterm_to_the_server_is_logged_with_its_sender() {
    assert_signal_is_logged_with_its_sender("TERM", "SIGTERM");
}

#[test]
fn sighup_to_the_server_is_logged_with_its_sender() {
    assert_signal_is_logged_with_its_sender("HUP", "SIGHUP");
}

#[test]
fn sigint_to_the_server_is_logged_with_its_sender() {
    assert_signal_is_logged_with_its_sender("INT", "SIGINT");
}

/// A signal queued with `sigqueue` carries the sender's own process id and user id: the kernel passes them through
/// unchecked for a queued signal, so the sender can name any process. The log line must therefore say how the signal
/// was sent (`cause="Sent(Queue)"`), so that a reader can tell such a line from an ordinary `kill`. macOS has no
/// `sigqueue`.
#[cfg(target_os = "linux")]
#[test]
fn a_queued_signal_is_logged_as_sent_by_queue() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));
    let server_pid = spawned.child.process_id().expect("server pid");

    let value = libc::sigval {
        sival_ptr: std::ptr::null_mut(),
    };
    let queued = unsafe { libc::sigqueue(server_pid as libc::pid_t, libc::SIGTERM, value) };
    assert_eq!(queued, 0, "sigqueue to the server must succeed");

    assert!(
        wait_for_server_exit(&mut spawned, Duration::from_secs(10)),
        "a queued SIGTERM must still stop the server"
    );
    let log = server_log(&config_home);
    let line = log
        .lines()
        .find(|line| line.contains("server.signal.received"))
        .unwrap_or_else(|| panic!("the server log has no line about the signal:\n{log}"));
    assert!(
        line.contains("signal=\"SIGTERM\""),
        "the line must name the signal: {line}"
    );
    assert!(
        line.contains("cause=\"Sent(Queue)\""),
        "the line must say the signal was queued, not sent with kill: {line}"
    );

    drop(spawned);
    cleanup_test_base(&base);
}

/// The log file's size limit: a log that already has this many bytes rotates on its first write.
const LOG_SIZE_LIMIT_BYTES: usize = 5 * 1024 * 1024;

fn server_app_dir(config_home: &Path) -> PathBuf {
    let app_dir = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    config_home.join(app_dir)
}

/// A log of exactly the size limit that starts with `marker`, so the server's first log line rotates it.
fn write_full_log(path: &Path, marker: &str) {
    let mut content = marker.as_bytes().to_vec();
    content.resize(LOG_SIZE_LIMIT_BYTES, b'.');
    fs::write(path, content).unwrap();
}

/// Starts a server whose log is already full, with `session_config` (the text under `[session]`, possibly empty) in a
/// config file the server is pointed at, waits for it to be ready, shuts it down over the API, and returns the log
/// directory.
fn run_server_with_full_log(
    base: &Path,
    session_config: &str,
    old_generations: &[(&str, &str)],
) -> PathBuf {
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let log_dir = server_app_dir(&config_home);
    fs::create_dir_all(&log_dir).unwrap();
    write_full_log(&log_dir.join("herdr-server.log"), "CURRENT-LOG");
    for (suffix, marker) in old_generations {
        fs::write(log_dir.join(format!("herdr-server.log.{suffix}")), marker).unwrap();
    }
    let config_file = base.join("config-under-test.toml");
    fs::write(
        &config_file,
        format!("onboarding = false\n[session]\n{session_config}"),
    )
    .unwrap();

    let mut spawned = spawn_server_with_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        &client_socket,
        &[("HERDR_CONFIG_PATH", config_file.as_path())],
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_herdr"));
    support::sanitize_herdr_env_command(&mut command);
    command
        .args(["server", "stop"])
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("HERDR_SOCKET_PATH", &api_socket)
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _ = command.spawn().expect("spawn the shutdown command").wait();
    assert!(
        wait_for_server_exit(&mut spawned, Duration::from_secs(10)),
        "the server must stop when asked"
    );
    drop(spawned);
    log_dir
}

/// herdr's log rotates at 5 MB and, by upstream's default, keeps no old copy: a busy server's log reaches back about a
/// day, so an event from before that cannot be read. With `[session] log_retained_files = 2` the full log must become
/// `.1`, the old `.1` must become `.2`, and the oldest copy beyond that must go. The very first rotation happens on the
/// server's first log line, before the config is loaded, so it must already use the setting.
#[test]
fn server_keeps_the_configured_number_of_rotated_log_copies() {
    let _lock = test_lock();
    let base = unique_test_dir();

    let log_dir = run_server_with_full_log(
        &base,
        "log_retained_files = 2\n",
        &[("1", "OLD-ONE"), ("2", "OLD-TWO")],
    );

    let one =
        fs::read(log_dir.join("herdr-server.log.1")).expect("the full log must be kept as .1");
    assert!(
        one.starts_with(b"CURRENT-LOG"),
        "herdr-server.log.1 must be the log that was full"
    );
    assert_eq!(
        one.len(),
        LOG_SIZE_LIMIT_BYTES,
        "the kept copy must be whole"
    );
    assert_eq!(
        fs::read(log_dir.join("herdr-server.log.2")).expect("the old .1 must become .2"),
        b"OLD-ONE"
    );
    assert!(
        !log_dir.join("herdr-server.log.3").exists(),
        "only 2 copies are to be kept: the old .2 must be gone and nothing may become .3"
    );
    let current = fs::read_to_string(log_dir.join("herdr-server.log")).unwrap();
    assert!(
        current.contains("app.startup") && !current.contains("CURRENT-LOG"),
        "the new log must start fresh with this run's lines"
    );

    cleanup_test_base(&base);
}

/// Default preservation: without the setting a full log is deleted when it rotates, exactly as upstream does, and no
/// numbered copy is shifted or created.
#[test]
fn server_keeps_no_rotated_log_copy_by_default() {
    let _lock = test_lock();
    let base = unique_test_dir();

    let log_dir = run_server_with_full_log(&base, "", &[("1", "OLD-ONE")]);

    let current = fs::read_to_string(log_dir.join("herdr-server.log")).unwrap();
    assert!(
        current.contains("app.startup") && !current.contains("CURRENT-LOG"),
        "the full log must be replaced by a fresh one"
    );
    assert!(
        fs::read(log_dir.join("herdr-server.log.1")).is_ok_and(|content| content == b"OLD-ONE"),
        "with no copies to keep, rotation must leave a numbered file that is already there alone"
    );
    assert!(
        !log_dir.join("herdr-server.log.2").exists(),
        "nothing may be shifted without the setting"
    );

    cleanup_test_base(&base);
}

/// A bad value must not stop the server or change how it logs: it keeps no copies.
#[test]
fn server_starts_and_keeps_no_copies_when_log_retained_files_is_bad() {
    let _lock = test_lock();
    let base = unique_test_dir();

    let log_dir = run_server_with_full_log(&base, "log_retained_files = \"many\"\n", &[]);

    let current = fs::read_to_string(log_dir.join("herdr-server.log")).unwrap();
    assert!(
        current.contains("app.startup"),
        "the server must have started and logged"
    );
    assert!(
        !log_dir.join("herdr-server.log.1").exists(),
        "a bad value keeps no rotated copy"
    );
    // The logger is installed before the config is loaded, and a parse failure is logged into this very file. A bad
    // value of this one key must not turn into a whole-config parse error (every setting back to its default,
    // including the program name the Pi agent is detected by).
    assert!(
        !current.contains("config parse error"),
        "a bad log_retained_files must not make the server fall back to the defaults for every setting"
    );

    cleanup_test_base(&base);
}
