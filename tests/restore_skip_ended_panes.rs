//! `[session] skip_ended_agent_panes_on_restore`, end to end with a real server and real shells: a save marks a pane whose agent
//! ended and whose shell is idle, and a start with the setting on leaves marked panes out and keeps everything else.
//!
//! The world has three panes: `p7` and `p9` each hold a resumable agent session (a fake agent named `pi` is resumed in them),
//! and `p8` is a plain shell next to `p7` in the same tab. `p9` is alone in a second workspace.
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

/// A fake agent that runs until a trigger file exists and then ends with status 1, so it leaves its shell behind. It prints a
/// line first: herdr's detector follows up a foreground job only once it has seen output from it, so a silent job is never
/// recognised as the agent and its end is never seen.
const ENDS_ON_TRIGGER: &str =
    "printf 'Working...\\n'\nwhile [ ! -e '{TRIGGER}' ]; do sleep 0.05; done\nexit 1";

/// Like `ENDS_ON_TRIGGER`, but a launch made after the file `{TRIGGER}.again` exists stays running: the agent that a second
/// `agent.start` puts into the pane of an agent that ended.
const ENDS_ON_TRIGGER_AND_STAYS_WHEN_STARTED_AGAIN: &str = "printf 'Working...\\n'\nif [ -e '{TRIGGER}.again' ]; then sleep 600; fi\nwhile [ ! -e '{TRIGGER}' ]; do sleep 0.05; done\nexit 1";

struct World {
    base: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    socket: PathBuf,
    data_dir: PathBuf,
}

/// What the saved session holds, and how the server is set up.
struct Setup {
    /// The line for `[session]` (`None` leaves the key out).
    setting: Option<bool>,
    /// The raw ids (7, 8, 9) of the panes that hold a resumable agent session in the hand-written file.
    sessions: &'static [u32],
    /// The raw ids (7, 8, 9) of panes the hand-written file marks `agent_ended`.
    marked: &'static [u32],
    /// The fake agent's body.
    agent_body: &'static str,
    pane_history: bool,
}

impl Setup {
    fn new(setting: Option<bool>) -> Self {
        Self {
            setting,
            sessions: &[7, 9],
            marked: &[],
            agent_body: ENDS_ON_TRIGGER,
            pane_history: false,
        }
    }
}

impl World {
    fn new(setup: &Setup) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = PathBuf::from(format!(
            "/tmp/herdr-skipended-{}-{nonce}",
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
        for name in ["p7", "p8", "p9"] {
            fs::create_dir_all(base.join(name)).unwrap();
        }
        support::register_runtime_dir(&runtime_dir);

        let log = base.join("agent.log");
        let agent = base.join("bin/pi");
        let body = setup
            .agent_body
            .replace("{TRIGGER}", &base.join("TRIGGER").display().to_string());
        fs::write(
            &agent,
            format!(
                "#!/bin/sh\necho \"start $$\" >> '{}'\n{body}\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();

        let world = Self {
            base,
            config_home,
            runtime_dir,
            socket,
            data_dir,
        };
        world.write_config(setup);
        world.save_session(setup);
        world
    }

    fn write_config(&self, setup: &Setup) {
        let setting_line = match setup.setting {
            Some(value) => format!("skip_ended_agent_panes_on_restore = {value}\n"),
            None => String::new(),
        };
        let history = if setup.pane_history {
            "[experimental]\npane_history = true\n"
        } else {
            ""
        };
        fs::write(
            self.data_dir.join("config.toml"),
            format!(
                "onboarding = false\n[terminal]\ndefault_shell = \"/bin/sh\"\nshell_mode = \"non_login\"\n[session]\nstartup_per_agent_delay_ms = 0\n{setting_line}{history}"
            ),
        )
        .unwrap();
    }

    fn save_session(&self, setup: &Setup) {
        let pane = |raw: u32, name: &str| {
            let mut pane = serde_json::json!({"cwd": self.base.join(name)});
            if setup.sessions.contains(&raw) {
                pane["agent_session"] = serde_json::json!({
                    "source": "herdr:pi", "agent": "pi", "kind": "path",
                    "value": self.base.join(format!("{name}.jsonl"))
                });
            }
            if setup.marked.contains(&raw) {
                pane["agent_ended"] = serde_json::json!(true);
            }
            pane
        };
        let snapshot = serde_json::json!({
            "version": 3, "active": 0, "selected": 0,
            "workspaces": [
                {"id": "w1", "custom_name": "one", "identity_cwd": self.base.join("p7"),
                 "tabs": [{
                    "layout": {"Split": {"direction": "Horizontal", "ratio": 0.5,
                                         "first": {"Pane": 7}, "second": {"Pane": 8}}},
                    "panes": {"7": pane(7, "p7"), "8": pane(8, "p8")},
                    "zoomed": false, "focused": 7, "root_pane": 7}]},
                {"id": "w2", "custom_name": "two", "identity_cwd": self.base.join("p9"),
                 "tabs": [{"layout": {"Pane": 9}, "panes": {"9": pane(9, "p9")},
                           "zoomed": false, "focused": 9, "root_pane": 9}]}
            ]
        });
        fs::write(self.session_file(), snapshot.to_string()).unwrap();
    }

    fn session_file(&self) -> PathBuf {
        self.data_dir.join("session.json")
    }

    fn trigger(&self) {
        fs::write(self.base.join("TRIGGER"), "").unwrap();
    }

    fn agent_starts(&self) -> usize {
        fs::read_to_string(self.base.join("agent.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("start "))
            .count()
    }

    fn start(&self) -> Server {
        let path = format!(
            "{}:{}",
            self.base.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        // A shell inside a herdr pane exports HERDR_* variables (HERDR_SOCKET_PATH names the developer's live server):
        // strip every one, then set the few this test means to pass.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("HERDR_") {
                command.env_remove(name);
            }
        }
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

    /// Every pane the server lists: (the last part of its saved directory, its pane id, the agent it shows).
    fn panes(&self) -> Vec<(String, String, Option<String>)> {
        let mut panes: Vec<_> = self.api("pane.list", serde_json::json!({}))["result"]["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pane| {
                let cwd = pane["cwd"].as_str().unwrap_or_default();
                (
                    cwd.rsplit('/').next().unwrap_or_default().to_string(),
                    pane["pane_id"].as_str().unwrap().to_string(),
                    pane["agent"].as_str().map(str::to_string),
                )
            })
            .collect();
        panes.sort();
        panes
    }

    fn pane_names(&self) -> Vec<String> {
        self.panes().into_iter().map(|(name, _, _)| name).collect()
    }

    fn pane_id_of(&self, name: &str) -> String {
        self.panes()
            .into_iter()
            .find(|(pane_name, _, _)| pane_name == name)
            .unwrap_or_else(|| panic!("no pane {name}: {:?}", self.panes()))
            .1
    }

    fn workspace_count(&self) -> usize {
        self.api("workspace.list", serde_json::json!({}))["result"]["workspaces"]
            .as_array()
            .unwrap()
            .len()
    }

    fn wait_until(&self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; panes: {:?}; agent starts: {}\n{}",
                self.panes(),
                self.agent_starts(),
                self.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// How many "agent changed" lines the server log holds that also contain every one of `needles`. A needle starts with a space
    /// so that it names the line's `agent=` field and not the `previous_agent=` field before it.
    fn agent_changes(&self, needles: &[&str]) -> usize {
        fs::read_to_string(self.data_dir.join("herdr-server.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| {
                line.contains("agent changed") && needles.iter().all(|needle| line.contains(needle))
            })
            .count()
    }

    /// Waits until herdr's own detector has recognised the fake agent in each of the `count` panes. The pane list cannot tell:
    /// a resumed pane lists the agent `pi` from the moment it is restored, before any process was seen.
    fn wait_for_agents_detected(&self, count: usize) {
        self.wait_until("the detector to recognise the resumed agents", |w| {
            w.agent_changes(&[" agent=Some(Pi) "]) >= count
        });
    }

    /// Waits until herdr's detector has seen each of the `count` agents end (the foreground job is the shell again).
    fn wait_for_agents_ended(&self, count: usize) {
        self.wait_until("the detector to see the agents end", |w| {
            w.agent_changes(&[" agent=None "]) >= count
        });
    }

    fn saved(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.session_file()).unwrap()).unwrap()
    }

    /// Every pane of the saved layout: (the last part of its directory, whether it carries the mark).
    fn saved_panes(&self) -> Vec<(String, bool)> {
        let saved = self.saved();
        let mut panes = Vec::new();
        for workspace in saved["workspaces"].as_array().unwrap() {
            for tab in workspace["tabs"].as_array().unwrap() {
                for pane in tab["panes"].as_object().unwrap().values() {
                    let cwd = pane["cwd"].as_str().unwrap();
                    panes.push((
                        cwd.rsplit('/').next().unwrap().to_string(),
                        pane.get("agent_ended") == Some(&serde_json::json!(true)),
                    ));
                }
            }
        }
        panes.sort();
        panes
    }

    fn saved_text(&self) -> String {
        fs::read_to_string(self.session_file()).unwrap()
    }

    fn backups(&self) -> Vec<PathBuf> {
        match fs::read_dir(self.data_dir.join("session-backups")) {
            Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn diagnostics(&self) -> String {
        let log = fs::read_to_string(self.data_dir.join("herdr-server.log")).unwrap_or_default();
        let interesting: Vec<&str> = log
            .lines()
            .filter(|line| {
                ["pane", "agent", "session", "persist", "stop"]
                    .iter()
                    .any(|word| line.to_lowercase().contains(word))
            })
            .collect();
        format!(
            "saved layout: {}\nserver log ({} of {} lines):\n{}",
            fs::read_to_string(self.session_file()).unwrap_or_default(),
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

    /// Sends `text` and a line end as ONE text input, with no key press: the only way in is the pane's text input.
    fn type_line_as_text(&self, pane_id: &str, text: &str) {
        let sent = self.api(
            "pane.send_text",
            serde_json::json!({"pane_id": pane_id, "text": format!("{text}\n")}),
        );
        assert!(sent.get("error").is_none(), "{sent}");
    }

    fn pane_text(&self, pane_id: &str) -> String {
        let read = self.api(
            "pane.read",
            serde_json::json!({"pane_id": pane_id, "source": "recent", "lines": 200}),
        );
        read["result"]["read"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
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
    /// A live handoff through the API: the server starts a replacement, hands every pane over and ends.
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

    /// A stop through the API, then waits for the process to end.
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

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| name.to_string()).collect()
}

/// A file that marks two of its three panes, neither holding an agent session, read with the setting on: the marked panes are
/// not brought back, the tab that held both a marked and a plain pane keeps the plain one, the workspace whose only pane was
/// marked is gone, and the first save keeps the file it replaced.
#[test]
fn a_start_with_the_setting_on_leaves_out_the_marked_panes_and_keeps_a_copy_of_the_old_file() {
    let setup = Setup {
        marked: &[7, 9],
        sessions: &[],
        ..Setup::new(Some(true))
    };
    let world = World::new(&setup);
    let original = fs::read(world.session_file()).unwrap();
    let server = world.start();
    world.wait_until("the plain shell to be listed", |w| {
        !w.pane_names().is_empty()
    });
    assert_eq!(
        world.pane_names(),
        names(&["p8"]),
        "{}",
        world.diagnostics()
    );
    assert_eq!(world.workspace_count(), 1);
    server.stop(&world);

    assert_eq!(
        world.saved_panes(),
        vec![("p8".to_string(), false)],
        "{}",
        world.diagnostics()
    );
    assert!(!world.saved_text().contains("agent_ended"));
    let backups = world.backups();
    assert_eq!(
        backups.len(),
        1,
        "the first save after a start that left panes out must keep the file it replaced: {backups:?}"
    );
    assert_eq!(
        fs::read(&backups[0]).unwrap(),
        original,
        "the copy is not the file that was read at the start"
    );
}

/// The same file with the setting off or absent: every pane comes back as before, nothing is copied aside, and the layout saved
/// at the stop carries no mark anywhere. The marked panes hold no session, so they are the ones the setting would leave out.
#[test]
fn with_the_setting_off_or_absent_every_pane_comes_back_whatever_the_file_says() {
    for setting in [None, Some(false)] {
        let setup = Setup {
            marked: &[7, 9],
            sessions: &[],
            ..Setup::new(setting)
        };
        let world = World::new(&setup);
        let server = world.start();
        world.wait_until("the panes to be listed", |w| w.pane_names().len() == 3);
        assert_eq!(
            world.pane_names(),
            names(&["p7", "p8", "p9"]),
            "{setting:?}"
        );
        assert_eq!(world.workspace_count(), 2, "{setting:?}");
        server.stop(&world);
        assert!(
            !world.saved_text().contains("agent_ended"),
            "{setting:?}: a save with the setting off wrote the mark"
        );
        assert!(
            world.backups().is_empty(),
            "{setting:?}: nothing was left out, so nothing is copied aside"
        );
    }
}

/// Nothing marked, the setting on: a plain shell and agent panes alike come back, and nothing is copied aside. This is the
/// the user's own shell tabs: a pane is never left out for being a shell. And the stop that follows, with both agents running
/// and detected, marks nothing: a stop that marked the fleet would drop every agent pane at the next start.
#[test]
fn a_file_without_marks_restores_every_pane_with_the_setting_on_and_a_stop_with_running_agents_marks_none(
) {
    let world = World::new(&Setup::new(Some(true)));
    let server = world.start();
    world.wait_until("both agents to be resumed", |w| w.agent_starts() == 2);
    assert_eq!(world.pane_names(), names(&["p7", "p8", "p9"]));
    assert_eq!(world.workspace_count(), 2);
    world.wait_for_agents_detected(2);
    server.stop(&world);
    assert!(world.backups().is_empty());
    assert_eq!(
        world.saved_panes(),
        vec![
            ("p7".to_string(), false),
            ("p8".to_string(), false),
            ("p9".to_string(), false)
        ],
        "a stop with running agents wrote a mark\n{}",
        world.diagnostics()
    );

    let server = world.start();
    world.wait_until("the panes to be listed", |w| w.pane_names().len() == 3);
    assert_eq!(world.pane_names(), names(&["p7", "p8", "p9"]));
    assert_eq!(world.workspace_count(), 2);
    server.stop(&world);
}

/// A marked pane that still holds an agent session is a resume the start would make, so it is kept and resumed, while a marked
/// pane with nothing to resume goes. The file loses something, so the first save keeps a copy of it.
#[test]
fn a_marked_pane_that_still_holds_a_session_is_kept_and_resumed_while_a_marked_shell_goes() {
    let setup = Setup {
        marked: &[7, 9],
        sessions: &[9],
        ..Setup::new(Some(true))
    };
    let world = World::new(&setup);
    let original = fs::read(world.session_file()).unwrap();
    let server = world.start();
    world.wait_until("the kept agent pane to be resumed", |w| {
        w.agent_starts() == 1
    });
    assert_eq!(
        world.pane_names(),
        names(&["p8", "p9"]),
        "{}",
        world.diagnostics()
    );
    assert_eq!(world.workspace_count(), 2);
    server.stop(&world);

    assert_eq!(
        world.saved_panes(),
        vec![("p8".to_string(), false), ("p9".to_string(), false)],
        "the mark of a pane that was kept stays in the file, or the pane that went is still there\n{}",
        world.diagnostics()
    );
    let backups = world.backups();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(fs::read(&backups[0]).unwrap(), original);
}

/// Every marked pane holds an agent session, so nothing is left out: every pane comes back, both agents are resumed, and no
/// copy of the file is made, because the start did not take anything out of it.
#[test]
fn a_file_whose_marked_panes_all_hold_a_session_leaves_nothing_out_and_copies_nothing_aside() {
    let setup = Setup {
        marked: &[7, 9],
        sessions: &[7, 9],
        ..Setup::new(Some(true))
    };
    let world = World::new(&setup);
    let server = world.start();
    world.wait_until("both agents to be resumed", |w| w.agent_starts() == 2);
    assert_eq!(world.pane_names(), names(&["p7", "p8", "p9"]));
    assert_eq!(world.workspace_count(), 2);
    server.stop(&world);
    assert!(
        world.backups().is_empty(),
        "nothing was left out, so nothing is copied aside"
    );
}

/// The whole path: a resumed agent ends and leaves its shell, the server saves the pane marked, and the next start with the
/// setting on leaves it out. The plain shell beside it is not marked and comes back, and the agent is not resumed again.
#[test]
fn a_pane_whose_agent_ended_is_marked_by_the_server_and_left_out_at_the_next_start() {
    let world = World::new(&Setup::new(Some(true)));
    let server = world.start();
    world.wait_until("both agents to be resumed", |w| w.agent_starts() == 2);
    world.wait_for_agents_detected(2);

    world.trigger();
    world.wait_for_agents_ended(2);
    server.stop(&world);

    assert_eq!(
        world.saved_panes(),
        vec![
            ("p7".to_string(), true),
            ("p8".to_string(), false),
            ("p9".to_string(), true)
        ],
        "{}",
        world.diagnostics()
    );

    let server = world.start();
    world.wait_until("the plain shell to be listed", |w| {
        !w.pane_names().is_empty()
    });
    assert_eq!(
        world.pane_names(),
        names(&["p8"]),
        "{}",
        world.diagnostics()
    );
    assert_eq!(world.workspace_count(), 1);
    server.stop(&world);
}

/// The control of the whole path: the same run with the setting off writes no mark and the next start brings every pane
/// back. An agent that ended has already lost its saved session (as before this setting), so the panes come back as plain
/// shells: the leftovers the setting is for.
#[test]
fn an_agent_that_ends_with_the_setting_off_leaves_no_mark_and_its_pane_comes_back() {
    let world = World::new(&Setup::new(None));
    let server = world.start();
    world.wait_until("both agents to be resumed", |w| w.agent_starts() == 2);
    world.wait_for_agents_detected(2);
    world.trigger();
    world.wait_for_agents_ended(2);
    server.stop(&world);

    assert!(
        !world.saved_text().contains("agent_ended"),
        "{}",
        world.diagnostics()
    );
    assert_eq!(world.saved_panes().len(), 3);

    let server = world.start();
    world.wait_until("the panes to be listed", |w| !w.pane_names().is_empty());
    assert_eq!(
        world.pane_names(),
        names(&["p7", "p8", "p9"]),
        "{}",
        world.diagnostics()
    );
    assert_eq!(world.workspace_count(), 2);
    server.stop(&world);
}

/// Typing into the shell after its agent ended keeps the pane: the user used the shell, so it is not a leftover. The line is
/// sent as text input alone (no key press), so this fails when text input does not remove the mark. The pane that was not
/// typed into is left out as before.
#[test]
fn typing_into_a_pane_after_its_agent_ended_keeps_the_pane() {
    let world = World::new(&Setup::new(Some(true)));
    let server = world.start();
    world.wait_until("both agents to be resumed", |w| w.agent_starts() == 2);
    world.wait_for_agents_detected(2);
    world.trigger();
    world.wait_for_agents_ended(2);

    let alive = world.base.join("ALIVE");
    world.type_line_as_text(
        &world.pane_id_of("p7"),
        &format!("touch '{}'", alive.display()),
    );
    world.wait_until("the shell to run what was typed", |_| alive.exists());
    server.stop(&world);

    assert_eq!(
        world.saved_panes(),
        vec![
            ("p7".to_string(), false),
            ("p8".to_string(), false),
            ("p9".to_string(), true)
        ],
        "{}",
        world.diagnostics()
    );
    let server = world.start();
    world.wait_until("the panes to be listed", |w| !w.pane_names().is_empty());
    assert_eq!(
        world.pane_names(),
        names(&["p7", "p8"]),
        "{}",
        world.diagnostics()
    );
    server.stop(&world);
}

/// `agent.start` (`herdr agent start`) into the pane of an ended agent writes the launch command straight to the pane, so no
/// typed input passes. A save has already found the shell idle and written the mark; the new agent running there must stop the
/// marks, or every later save, the one at the stop included, marks a busy pane and the next start leaves it out. The pane that was
/// left alone is left out as before.
#[test]
fn an_agent_started_in_the_pane_of_an_ended_agent_stops_its_marks() {
    let mut setup = Setup::new(Some(true));
    setup.agent_body = ENDS_ON_TRIGGER_AND_STAYS_WHEN_STARTED_AGAIN;
    let world = World::new(&setup);
    let server = world.start();
    world.wait_until("both agents to be resumed", |w| w.agent_starts() == 2);
    world.wait_for_agents_detected(2);
    world.trigger();
    world.wait_for_agents_ended(2);
    world.wait_until("a save to carry the mark of p7", |w| {
        w.saved_panes().contains(&("p7".to_string(), true))
    });

    fs::write(world.base.join("TRIGGER.again"), "").unwrap();
    let started = world.api(
        "agent.start",
        serde_json::json!({
            "name": "again", "kind": "pi", "pane_id": world.pane_id_of("p7"),
            "args": [], "timeout_ms": 8_000
        }),
    );
    assert_eq!(started["result"]["type"], "agent_started", "{started}");
    world.wait_until("the detector to recognise the second agent in p7", |w| {
        w.agent_starts() == 3 && w.agent_changes(&[" agent=Some(Pi) "]) >= 3
    });
    server.stop(&world);

    assert_eq!(
        world.saved_panes(),
        vec![
            ("p7".to_string(), false),
            ("p8".to_string(), false),
            ("p9".to_string(), true)
        ],
        "{}",
        world.diagnostics()
    );
    let server = world.start();
    world.wait_until("the panes to be listed", |w| !w.pane_names().is_empty());
    assert_eq!(
        world.pane_names(),
        names(&["p7", "p8"]),
        "{}",
        world.diagnostics()
    );
    server.stop(&world);
}

/// The mark crosses a live handoff: the new server imports every pane (nothing is left out of a handoff), still knows which
/// shells are leftovers, and so saves them marked. This fails when the old server does not export the mark, when the new one
/// does not import it, and when a handoff applies the skip (the strict import then loses panes or the handoff fails).
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_mark_crosses_a_live_handoff_and_no_pane_is_left_out_of_it() {
    let world = World::new(&Setup::new(Some(true)));
    let server = world.start();
    world.wait_until("both agents to be resumed", |w| w.agent_starts() == 2);
    world.wait_for_agents_detected(2);
    world.trigger();
    world.wait_for_agents_ended(2);

    let handed = server.live_handoff(&world);
    assert_eq!(
        world.pane_names(),
        names(&["p7", "p8", "p9"]),
        "the handoff lost a pane\n{}",
        world.diagnostics()
    );
    handed.stop(&world);

    assert_eq!(
        world.saved_panes(),
        vec![
            ("p7".to_string(), true),
            ("p8".to_string(), false),
            ("p9".to_string(), true)
        ],
        "the new server forgot which shells are leftovers\n{}",
        world.diagnostics()
    );
}

/// Leaving panes out must not cost the kept panes their saved screens. The server writes the layout and the screens together,
/// the test then marks two panes (a mark takes no part in the layout fingerprint, so the pair still belongs together), and the
/// start with the setting on must show the kept pane's screen: the history is pruned with the layout and stamped with the
/// pruned layout's fingerprint.
#[test]
fn the_kept_pane_keeps_its_saved_screen_when_marked_panes_are_left_out() {
    let setup = Setup {
        sessions: &[],
        pane_history: true,
        ..Setup::new(Some(true))
    };
    let world = World::new(&setup);
    let server = world.start();
    world.wait_until("the panes to be listed", |w| w.pane_names().len() == 3);
    for name in ["p7", "p8", "p9"] {
        let pane_id = world.pane_id_of(name);
        // The word is assembled by the shell, so it is on the screen only as output, never as the typed command.
        world.type_into(&pane_id, &format!("printf 'SCREEN-%s\\n' {name}"));
        world.wait_until("the shell to print", |w| {
            w.pane_text(&pane_id).contains(&format!("SCREEN-{name}"))
        });
    }
    server.stop(&world);

    let history: serde_json::Value =
        serde_json::from_slice(&fs::read(world.data_dir.join("session-history.json")).unwrap())
            .unwrap();
    assert!(
        history["layout_fingerprint"].is_string(),
        "the history carries no fingerprint, so this test would not cover the fingerprint rule: {history}"
    );
    let mut saved = world.saved();
    for workspace in saved["workspaces"].as_array_mut().unwrap() {
        for tab in workspace["tabs"].as_array_mut().unwrap() {
            for pane in tab["panes"].as_object_mut().unwrap().values_mut() {
                let cwd = pane["cwd"].as_str().unwrap().to_string();
                if cwd.ends_with("/p7") || cwd.ends_with("/p9") {
                    pane["agent_ended"] = serde_json::json!(true);
                }
            }
        }
    }
    fs::write(world.session_file(), saved.to_string()).unwrap();

    let server = world.start();
    world.wait_until("the plain shell to be listed", |w| {
        !w.pane_names().is_empty()
    });
    assert_eq!(
        world.pane_names(),
        names(&["p8"]),
        "{}",
        world.diagnostics()
    );
    let pane_id = world.pane_id_of("p8");
    world.wait_until("the saved screen to be replayed", |w| {
        w.pane_text(&pane_id).contains("SCREEN-p8")
    });
    server.stop(&world);
}
