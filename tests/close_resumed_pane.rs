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
    base: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    socket: PathBuf,
    data_dir: PathBuf,
}

impl World {
    fn new(agent_body: &str, setting: Option<bool>) -> Self {
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
                "#!/bin/sh\necho \"start $$\" >> '{log}'\n{body}\n",
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
        let snapshot = serde_json::json!({
            "version": 3, "active": 0, "selected": 0,
            "workspaces": [{
                "id": "w1", "custom_name": "resumed", "identity_cwd": self.base,
                "tabs": [{"layout": {"Pane": 7},
                          "panes": {"7": {"cwd": self.base, "agent_session": {
                              "source": "herdr:pi", "agent": "pi", "kind": "path",
                              "value": self.session_path()
                          }}},
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
        Server { child }
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
}

impl Server {
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

/// A crash keeps the pane, with the setting on. On 29 Sep 2026 the agent binary crashed on its first render in every resumed
/// pane at once; closing those panes would have taken their sessions out of the layout.
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
