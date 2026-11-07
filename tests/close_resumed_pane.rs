//! `[session] close_pane_when_resumed_agent_exits`, end to end with a real server and a real shell: a restart resumes a fake
//! agent in a pane; what becomes of the pane when the agent ends, and what the layout saved at a stop still holds.
#![cfg(unix)]

pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WAIT: Duration = Duration::from_secs(30);

/// One test's world: a config home, a runtime dir, a session saved by an earlier "run" with one pane whose agent session is
/// `session.jsonl`, and a fake agent named `pi` that the pane's shell finds first on its PATH.
struct World {
    launch_argv: Option<Vec<&'static str>>,
    base: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    socket: PathBuf,
    data_dir: PathBuf,
}

impl World {
    fn new(agent_body: &str, setting: Option<bool>) -> Self {
        Self::build(agent_body, setting, None)
    }

    /// Like `new`, with a pane that was started with a command: its saved layout holds the launch command
    /// `pi --model opus`, which the restart replays (plus the session) instead of the plain resume command.
    fn with_launch_command(agent_body: &str, setting: Option<bool>) -> Self {
        Self::build(agent_body, setting, Some(vec!["pi", "--model", "opus"]))
    }

    fn build(
        agent_body: &str,
        setting: Option<bool>,
        launch_argv: Option<Vec<&'static str>>,
    ) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = PathBuf::from(format!(
            "/tmp/herdr-closeres-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        let socket = runtime_dir.join("herdr.sock");
        let data_dir = config_home.join(if cfg!(debug_assertions) {
            "herdr-dev"
        } else {
            "herdr"
        });
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(base.join("bin")).unwrap();
        support::register_runtime_dir(&runtime_dir);

        let log = base.join("agent.log");
        let agent = base.join("bin/pi");
        fs::write(
            &agent,
            format!(
                "#!/bin/sh\necho \"start $$\" >> '{log}'\necho \"args $*\" >> '{log}'\n{body}\n",
                log = log.display(),
                body = agent_body.replace("{LOG}", &log.display().to_string())
            ),
        )
        .unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();

        let setting_line = match setting {
            Some(value) => format!("close_pane_when_resumed_agent_exits = {value}\n"),
            None => String::new(),
        };
        fs::write(
            data_dir.join("config.toml"),
            format!(
                "onboarding = false\n[terminal]\ndefault_shell = \"/bin/sh\"\nshell_mode = \"non_login\"\n[session]\nstartup_per_agent_delay_ms = 0\n{setting_line}"
            ),
        )
        .unwrap();
        let world = Self {
            launch_argv,
            base,
            config_home,
            runtime_dir,
            socket,
            data_dir,
        };
        world.save_session();
        world
    }

    fn save_session(&self) {
        let mut pane = serde_json::json!({"cwd": self.base, "agent_session": {
            "source": "herdr:pi", "agent": "pi", "kind": "path",
            "value": self.session_path()
        }});
        if let Some(argv) = &self.launch_argv {
            pane["launch_argv"] = serde_json::json!(argv);
        }
        let snapshot = serde_json::json!({
            "version": 3, "active": 0, "selected": 0,
            "workspaces": [{
                "id": "w1", "custom_name": "resumed", "identity_cwd": self.base,
                "tabs": [{"layout": {"Pane": 7},
                          "panes": {"7": pane},
                          "zoomed": false, "focused": 7, "root_pane": 7}]
            }]
        });
        fs::write(self.data_dir.join("session.json"), snapshot.to_string()).unwrap();
    }

    fn session_path(&self) -> PathBuf {
        self.base.join("session.jsonl")
    }

    fn agent_log(&self) -> String {
        fs::read_to_string(self.base.join("agent.log")).unwrap_or_default()
    }

    /// The arguments of every run of the fake agent, one string per run.
    fn agent_args(&self) -> Vec<String> {
        self.agent_log()
            .lines()
            .filter_map(|line| line.strip_prefix("args "))
            .map(str::to_string)
            .collect()
    }

    fn agent_starts(&self) -> Vec<u32> {
        self.agent_log()
            .lines()
            .filter_map(|line| line.strip_prefix("start "))
            .filter_map(|pid| pid.parse().ok())
            .collect()
    }

    fn start(&self) -> Server {
        let path = format!(
            "{}:{}",
            self.base.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        support::sanitize_herdr_env_command(&mut command);
        let child = command
            .arg("server")
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("HERDR_SOCKET_PATH", &self.socket)
            .env("SHELL", "/bin/sh")
            .env("PATH", path)
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_STARTUP_CWD")
            .env_remove("HERDR_SESSION")
            .env_remove("HERDR_ENV")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        support::register_spawned_herdr_pid(Some(child.id()));
        support::wait_for_socket(&self.socket, Duration::from_secs(10));
        Server { child, gone: false }
    }

    fn api(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        writeln!(
            stream,
            "{}",
            serde_json::json!({"id": "t", "method": method, "params": params})
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn pane_ids(&self) -> Vec<String> {
        self.api("pane.list", serde_json::json!({}))["result"]["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pane| pane["pane_id"].as_str().unwrap().to_string())
            .collect()
    }

    fn wait_until(&self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; agent log: {:?}; panes: {:?}",
                self.agent_log(),
                self.pane_ids()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// The saved layout: the number of panes in its first tab and the agent session values they hold.
    fn saved_layout(&self) -> (usize, Vec<String>) {
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(self.data_dir.join("session.json")).unwrap()).unwrap();
        let panes = saved["workspaces"][0]["tabs"][0]["panes"]
            .as_object()
            .unwrap();
        let sessions = panes
            .values()
            .filter_map(|pane| pane["agent_session"]["value"].as_str())
            .map(str::to_string)
            .collect();
        (panes.len(), sessions)
    }

    /// What a failed assertion needs to see: the layout on disk and the server log, which names each pane exit and each
    /// agent change with its time.
    fn diagnostics(&self) -> String {
        let session = fs::read_to_string(self.data_dir.join("session.json")).unwrap_or_default();
        let log = fs::read_to_string(self.data_dir.join("herdr-server.log")).unwrap_or_default();
        let interesting: Vec<&str> = log
            .lines()
            .filter(|line| {
                ["pane", "agent", "checkpoint", "session", "shutdown", "stop"]
                    .iter()
                    .any(|word| line.to_lowercase().contains(word))
            })
            .collect();
        format!(
            "saved layout: {session}\nserver log ({} of {} lines):\n{}",
            interesting.len(),
            log.lines().count(),
            interesting.join("\n")
        )
    }

    /// Types `text` and Enter into the pane, as a user would.
    fn type_into(&self, pane_id: &str, text: &str) {
        let sent = self.api(
            "pane.send_text",
            serde_json::json!({"pane_id": pane_id, "text": text}),
        );
        assert!(sent.get("error").is_none(), "{sent}");
        let sent = self.api(
            "pane.send_keys",
            serde_json::json!({"pane_id": pane_id, "keys": ["Enter"]}),
        );
        assert!(sent.get("error").is_none(), "{sent}");
    }
}

impl Drop for World {
    fn drop(&mut self) {
        support::cleanup_test_base(&self.base);
    }
}

struct Server {
    child: Child,
    /// The child has been waited for: its pid may be reused, so it must not be signalled again.
    gone: bool,
}

impl Server {
    /// A live handoff through the API: the server starts a replacement, hands every pane over and ends. Returns when the
    /// replacement answers on the same socket.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn live_handoff(mut self, world: &World) -> HandedOffServer {
        let before =
            support::herdr_server_pids_for_runtime_dir(&world.runtime_dir).unwrap_or_default();
        let moved = world.api("server.live_handoff", serde_json::json!({}));
        assert!(moved.get("error").is_none(), "{moved}");
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "the old server ended with {status:?}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the old server did not end after the handoff"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        let old_pid = self.child.id();
        support::unregister_spawned_herdr_pid(Some(old_pid));
        self.gone = true;
        let deadline = Instant::now() + WAIT;
        let replacement = loop {
            let pids =
                support::herdr_server_pids_for_runtime_dir(&world.runtime_dir).unwrap_or_default();
            if let Some(pid) = pids
                .iter()
                .copied()
                .find(|pid| *pid != old_pid && !before.contains(pid))
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "no replacement server appeared; servers: {pids:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        support::wait_for_socket(&world.socket, Duration::from_secs(10));
        HandedOffServer { pid: replacement }
    }

    /// `herdr server stop` through the API, then waits for the process to end.
    fn stop(mut self, world: &World) {
        let stopped = world.api("server.stop", serde_json::json!({}));
        assert!(stopped.get("error").is_none(), "{stopped}");
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{status:?}");
                return;
            }
            assert!(Instant::now() < deadline, "the server did not stop");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.gone {
            return;
        }
        let pid = self.child.id();
        // portable-pty-free: SIGTERM asks the server to stop; the kill is the last resort.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                support::unregister_spawned_herdr_pid(Some(pid));
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        support::unregister_spawned_herdr_pid(Some(pid));
    }
}

/// The server that took over the panes in a live handoff. It is not a child of the test, so it is stopped through the API and
/// waited for by its pid.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct HandedOffServer {
    pid: u32,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl HandedOffServer {
    fn stop(self, world: &World) {
        let stopped = world.api("server.stop", serde_json::json!({}));
        assert!(stopped.get("error").is_none(), "{stopped}");
        let deadline = Instant::now() + WAIT;
        while !process_is_gone(self.pid) {
            assert!(
                Instant::now() < deadline,
                "the replacement server did not stop"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// True when no live process has this pid (a zombie that nobody has reaped yet counts as gone).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn process_is_gone(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return true;
    }
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit(')')
                .next()
                .map(|rest| rest.trim_start().to_string())
        })
        .is_some_and(|rest| rest.starts_with('Z'))
}

/// An agent that stays until it is told to stop, and exits 0 on SIGHUP or SIGTERM (a well-behaved agent), writing which.
const WELL_BEHAVED_AGENT: &str = "trap 'echo got-term >> \"{LOG}\"; exit 0' TERM\ntrap 'echo got-hup >> \"{LOG}\"; exit 0' HUP\nwhile :; do sleep 0.05; done";

fn world_with_a_well_behaved_agent(setting: Option<bool>) -> World {
    World::new(WELL_BEHAVED_AGENT, setting)
}

/// A server stop must not take a resumed agent out of the saved layout, even when the agent exits 0 on the SIGHUP and SIGTERM
/// the stop sends its pane (so that its shell then runs `exit`). The layout is saved before any pane is signalled; the next
/// start resumes the agent again.
#[test]
fn a_server_stop_keeps_the_resumed_agent_in_the_layout_and_the_next_start_resumes_it() {
    let world = world_with_a_well_behaved_agent(Some(true));
    let server = world.start();
    world.wait_until("the agent to be resumed", |w| w.agent_starts().len() == 1);
    assert_eq!(world.pane_ids().len(), 1);

    server.stop(&world);

    // The agent really got the stop's signal and ended with status 0 (so its shell, typed `&& exit`, ended too).
    let log = world.agent_log();
    assert!(
        log.contains("got-hup") || log.contains("got-term"),
        "the stop must signal the agent, or this test proves nothing: {log:?}"
    );
    let (panes, sessions) = world.saved_layout();
    assert_eq!(panes, 1, "the pane left the saved layout");
    assert_eq!(sessions, vec![world.session_path().display().to_string()]);

    let server = world.start();
    world.wait_until("the agent to be resumed again", |w| {
        w.agent_starts().len() == 2
    });
    server.stop(&world);
}

/// The operating system signals every process when it shuts down: the agent exits 0, its shell runs `exit`, and the server
/// learns the pane ended before it learns it is stopping. The layout it saves last must still hold the pane and its session.
#[test]
fn a_pane_that_ends_just_before_a_stop_is_still_in_the_saved_layout_with_its_session() {
    let world = world_with_a_well_behaved_agent(Some(true));
    let server = world.start();
    world.wait_until("the agent to be resumed", |w| w.agent_starts().len() == 1);
    let agent_pid = world.agent_starts()[0];

    // As the OS does at shutdown: SIGTERM to the agent. It exits 0, the shell exits, the pane closes in the server.
    assert_eq!(
        unsafe { libc::kill(agent_pid as libc::pid_t, libc::SIGTERM) },
        0
    );
    world.wait_until("the pane to close", |w| w.pane_ids().is_empty());
    assert!(world.agent_log().contains("got-term"));
    server.stop(&world);

    let (panes, sessions) = world.saved_layout();
    assert_eq!(
        panes,
        1,
        "the pane that ended at the stop is not in the saved layout\n{}",
        world.diagnostics()
    );
    assert_eq!(
        sessions,
        vec![world.session_path().display().to_string()],
        "the layout saved at the stop lost the agent's session\n{}",
        world.diagnostics()
    );

    // And the next start resumes it.
    let server = world.start();
    world.wait_until("the agent to be resumed again", |w| {
        w.agent_starts().len() == 2
    });
    server.stop(&world);
}

/// A clean end closes the pane with the setting on, and leaves the shell with it off (upstream's behaviour, the control).
#[test]
fn a_clean_agent_end_closes_the_pane_only_when_the_setting_is_on() {
    for setting in [Some(true), None, Some(false)] {
        let world = World::new("exit 0", setting);
        let server = world.start();
        world.wait_until("the agent to be resumed", |w| w.agent_starts().len() == 1);
        if setting == Some(true) {
            world.wait_until("the pane to close", |w| w.pane_ids().is_empty());
        } else {
            // The pane stays and its shell still answers: typed text runs.
            let panes = world.pane_ids();
            assert_eq!(panes.len(), 1);
            let alive = world.base.join("ALIVE");
            world.type_into(&panes[0], &format!("touch '{}'", alive.display()));
            world.wait_until("the shell to answer", |_| alive.exists());
            assert_eq!(world.pane_ids(), panes, "{setting:?}");
        }
        server.stop(&world);
    }
}

/// A crash keeps the pane, with the setting on. An agent binary that crashes on its first render does so in every resumed pane
/// at once; closing those panes would take their sessions out of the layout.
#[test]
fn a_crashed_agent_keeps_its_pane_and_its_shell() {
    let world = World::new("exit 1", Some(true));
    let server = world.start();
    world.wait_until("the agent to be resumed", |w| w.agent_starts().len() == 1);
    let panes = world.pane_ids();
    assert_eq!(panes.len(), 1);
    let alive = world.base.join("ALIVE");
    world.type_into(&panes[0], &format!("touch '{}'", alive.display()));
    world.wait_until("the shell to answer after the crash", |_| alive.exists());
    assert_eq!(world.pane_ids(), panes);
    server.stop(&world);
}

/// A pane that was started with a command (its layout holds a launch command) is resumed by a restart, then handed to a new
/// server in a live handoff. When its agent ends normally on the new server the pane must close with it: the new server must
/// not give the pane a fresh shell the way it does for any other imported pane with a launch command.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_pane_started_with_a_command_closes_with_its_agent_after_a_live_handoff() {
    let world = World::with_launch_command(WELL_BEHAVED_AGENT, Some(true));
    let server = world.start();
    world.wait_until("the agent to be resumed", |w| w.agent_starts().len() == 1);
    // The restart replayed the saved launch command: its flags, then the session. Without that this test would not be about a
    // pane with a launch command.
    let args = world.agent_args();
    assert!(
        args.len() == 1 && args[0].starts_with("--model opus --session "),
        "the launch command was not replayed: {args:?}"
    );
    let agent_pid = world.agent_starts()[0];
    assert_eq!(world.pane_ids().len(), 1);

    let handed = server.live_handoff(&world);
    assert_eq!(world.pane_ids().len(), 1, "the handoff lost the pane");

    // The agent ends normally on the new server: SIGTERM, exit status 0, then the shell's `exit`.
    assert_eq!(
        unsafe { libc::kill(agent_pid as libc::pid_t, libc::SIGTERM) },
        0
    );
    world.wait_until("the pane to close on the new server", |w| {
        w.pane_ids().is_empty()
    });
    assert!(world.agent_log().contains("got-term"));
    handed.stop(&world);
}

/// The flag that a pane closes with its agent crosses a live handoff: the old server typed the exit into the pane's shell, the
/// new server must still treat the pane's end like the old one did. A pane that ends on the new server just before it stops
/// is still in the layout it saves last, with its agent session; the next start resumes it. This fails when either half of
/// the handoff field is missing, the old server's export or the new server's import.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_pane_that_ends_after_a_live_handoff_is_still_in_the_saved_layout_with_its_session() {
    let world = world_with_a_well_behaved_agent(Some(true));
    let server = world.start();
    world.wait_until("the agent to be resumed", |w| w.agent_starts().len() == 1);
    let agent_pid = world.agent_starts()[0];

    let handed = server.live_handoff(&world);
    assert_eq!(world.pane_ids().len(), 1, "the handoff lost the pane");

    assert_eq!(
        unsafe { libc::kill(agent_pid as libc::pid_t, libc::SIGTERM) },
        0
    );
    world.wait_until("the pane to close on the new server", |w| {
        w.pane_ids().is_empty()
    });
    assert!(world.agent_log().contains("got-term"));
    handed.stop(&world);

    let (panes, sessions) = world.saved_layout();
    assert_eq!(
        panes,
        1,
        "the pane that ended after the handoff is not in the saved layout\n{}",
        world.diagnostics()
    );
    assert_eq!(
        sessions,
        vec![world.session_path().display().to_string()],
        "the layout saved after the handoff lost the agent's session\n{}",
        world.diagnostics()
    );

    let server = world.start();
    world.wait_until("the agent to be resumed again", |w| {
        w.agent_starts().len() == 2
    });
    server.stop(&world);
}
