//! A server that restores a session replays pane screen history written by an older herdr, which carries no layout
//! fingerprint, only when the history's structure fits the saved session. End to end: a real server reads a saved
//! `session.json` and `session-history.json` pair and a real shell pane shows (or does not show) the saved screen.
#![cfg(unix)]

pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FIRST_TAB_SCREEN: &str = "LEGACY-SCREEN-OF-THE-FIRST-TAB";
const SECOND_TAB_SCREEN: &str = "LEGACY-SCREEN-OF-THE-SECOND-TAB";
const FOREIGN_PANE_SCREEN: &str = "LEGACY-SCREEN-OF-A-PANE-THE-SESSION-LACKS";

struct RestoredServer {
    child: std::process::Child,
    base: PathBuf,
    data_dir: PathBuf,
    socket_path: PathBuf,
}

impl Drop for RestoredServer {
    fn drop(&mut self) {
        let pid = self.child.id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        support::unregister_spawned_herdr_pid(Some(pid));
        support::cleanup_test_base(&self.base);
    }
}

impl RestoredServer {
    fn request(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let mut stream = UnixStream::connect(&self.socket_path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        writeln!(
            stream,
            "{}",
            serde_json::json!({"id": "legacy-history", "method": method, "params": params})
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .unwrap_or_else(|err| panic!("{method}: {err}"));
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(response.get("error").is_none(), "{method}: {response}");
        response
    }

    /// (pane id, tab id, the pane's recent text) of every pane, in the order the server lists them.
    fn pane_texts(&self) -> Vec<(String, String, String)> {
        let panes = self.request("pane.list", serde_json::json!({}));
        panes["result"]["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pane| {
                let pane_id = pane["pane_id"].as_str().unwrap().to_string();
                let read = self.request(
                    "pane.read",
                    serde_json::json!({"pane_id": pane_id, "source": "recent", "lines": 200}),
                );
                let text = read["result"]["read"]["text"].as_str().unwrap().to_string();
                (pane_id, pane["tab_id"].as_str().unwrap().to_string(), text)
            })
            .collect()
    }

    /// Types a command whose output exists only if the pane's shell is alive and its screen can be read, and waits for
    /// it. Without this an absence of saved text could only mean the pane was read too early.
    fn wait_until_every_shell_answers(&self) {
        for (pane_id, _, _) in self.pane_texts() {
            self.request(
                "pane.send_text",
                serde_json::json!({"pane_id": pane_id, "text": "echo ALIVE-$((6*7))"}),
            );
            self.request(
                "pane.send_keys",
                serde_json::json!({"pane_id": pane_id, "keys": ["Enter"]}),
            );
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let texts = self.pane_texts();
            if texts.iter().all(|(_, _, text)| text.contains("ALIVE-42")) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "a pane's shell never answered: {texts:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Stops the server through its API and returns the path of the history file it saved on the way out.
    fn stop(&mut self) -> PathBuf {
        self.request("server.stop", serde_json::json!({}));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "the server did not stop");
            std::thread::sleep(Duration::from_millis(25));
        }
        self.data_dir.join("session-history.json")
    }
}

/// Starts a server on a saved session of two workspaces and the given history file.
///
/// Workspace `w1` has a tab of panes 3 and 4 (pane 4 was blank when the history was saved, so the history has no entry
/// for it) and a tab of pane 7. Workspace `w2` has a tab of pane 9, also blank.
fn start_server_with_history(history: &dyn Fn() -> serde_json::Value) -> RestoredServer {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = PathBuf::from(format!(
        "/tmp/herdr-legacy-history-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&base).unwrap();
    let base = fs::canonicalize(base).unwrap();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let data_dir = config_home.join(if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    });
    let work = base.join("work");
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    let pane = |_: u32| serde_json::json!({ "cwd": work });
    let tab = |layout: serde_json::Value, ids: &[u32]| {
        serde_json::json!({
            "layout": layout,
            "panes": ids.iter().map(|id| (id.to_string(), pane(*id))).collect::<serde_json::Map<_, _>>(),
            "zoomed": false, "focused": ids[0], "root_pane": ids[0]
        })
    };
    let split = serde_json::json!({"Split": {
        "direction": "Horizontal", "ratio": 0.5, "first": {"Pane": 3}, "second": {"Pane": 4}
    }});
    let session = serde_json::json!({
        "version": 3, "active": 0, "selected": 0,
        "workspaces": [
            {"id": "w1", "custom_name": "one", "identity_cwd": work,
             "tabs": [tab(split, &[3, 4]), tab(serde_json::json!({"Pane": 7}), &[7])]},
            {"id": "w2", "custom_name": "two", "identity_cwd": work,
             "tabs": [tab(serde_json::json!({"Pane": 9}), &[9])]}
        ]
    });
    fs::write(data_dir.join("session.json"), session.to_string()).unwrap();
    fs::write(data_dir.join("session-history.json"), history().to_string()).unwrap();
    fs::write(
        data_dir.join("config.toml"),
        "onboarding = false\n[terminal]\ndefault_shell = \"/bin/sh\"\n[experimental]\npane_history = true\n",
    )
    .unwrap();

    support::register_runtime_dir(&runtime_dir);
    // A shell inside a herdr pane exports HERDR_* variables (HERDR_SOCKET_PATH names the developer's live server):
    // strip every one, then set the few this test means to pass.
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    support::sanitize_herdr_env_command(&mut command);
    let child = command
        .arg("server")
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("HERDR_SOCKET_PATH", &socket_path)
        .env("SHELL", "/bin/sh")
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
    let server = RestoredServer {
        child,
        base,
        data_dir,
        socket_path: socket_path.clone(),
    };
    support::wait_for_socket(&socket_path, Duration::from_secs(10));
    server
}

fn screen(text: &str) -> serde_json::Value {
    serde_json::json!({"ansi": format!("{text}\r\n"), "lines": 1})
}

/// The history a herdr of the previous line saved for that session: no fingerprint, and no entry for the blank panes.
fn legacy_history() -> serde_json::Value {
    serde_json::json!({
        "version": 3,
        "workspaces": [
            {"tabs": [
                {"panes": {"3": screen(FIRST_TAB_SCREEN)}},
                {"panes": {"7": screen(SECOND_TAB_SCREEN)}}
            ]},
            {"tabs": [{"panes": {}}]}
        ]
    })
}

fn tab_showing<'a>(panes: &'a [(String, String, String)], marker: &str) -> &'a str {
    panes
        .iter()
        .find(|(_, _, text)| text.contains(marker))
        .map(|(_, tab_id, _)| tab_id.as_str())
        .unwrap_or_else(|| panic!("no pane shows {marker}: {panes:?}"))
}

fn assert_no_pane_shows_saved_screen(server: &RestoredServer, why: &str) {
    server.wait_until_every_shell_answers();
    let panes = server.pane_texts();
    assert_eq!(panes.len(), 4, "{why}: {panes:?}");
    for (pane_id, _, text) in &panes {
        for saved in [FIRST_TAB_SCREEN, SECOND_TAB_SCREEN, FOREIGN_PANE_SCREEN] {
            assert!(
                !text.contains(saved),
                "{why}: pane {pane_id} shows a saved screen it must not: {text}"
            );
        }
    }
}

#[test]
fn a_history_file_written_by_an_older_herdr_is_replayed_when_its_structure_matches_the_session() {
    let mut server = start_server_with_history(&legacy_history);

    // The screens come back in the panes they were saved from, and only there.
    let deadline = Instant::now() + Duration::from_secs(20);
    let panes = loop {
        let panes = server.pane_texts();
        let first = panes
            .iter()
            .filter(|(_, _, text)| text.contains(FIRST_TAB_SCREEN))
            .collect::<Vec<_>>();
        let second = panes
            .iter()
            .filter(|(_, _, text)| text.contains(SECOND_TAB_SCREEN))
            .collect::<Vec<_>>();
        if first.len() == 1 && second.len() == 1 {
            break panes;
        }
        assert!(
            Instant::now() < deadline,
            "the saved screens did not come back: {panes:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(panes.len(), 4, "{panes:?}");
    assert_eq!(tab_showing(&panes, FIRST_TAB_SCREEN), "w1:t1");
    assert_eq!(tab_showing(&panes, SECOND_TAB_SCREEN), "w1:t2");

    // Once the server saves again, the history it writes carries the fingerprint, so the next restart needs no exception.
    server.wait_until_every_shell_answers();
    let history_path = server.stop();
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(history_path).unwrap()).unwrap();
    assert!(
        saved["layout_fingerprint"]
            .as_str()
            .is_some_and(|f| f.len() == 64),
        "the history saved after the restore must carry a layout fingerprint: {}",
        saved["layout_fingerprint"]
    );
}

#[test]
fn a_history_file_written_by_an_older_herdr_for_another_structure_is_not_replayed() {
    // Every variant keeps the screen of pane 3 in the file, so a server that skipped the structure check would show it.
    for why in [
        "one workspace more",
        "one workspace fewer",
        "one tab fewer",
        "a pane the tab does not have",
    ] {
        let mut server = start_server_with_history(&|| {
            let mut history = legacy_history();
            match why {
                "one workspace more" => history["workspaces"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({"tabs": []})),
                "one workspace fewer" => {
                    history["workspaces"].as_array_mut().unwrap().pop();
                }
                "one tab fewer" => {
                    history["workspaces"][0]["tabs"]
                        .as_array_mut()
                        .unwrap()
                        .pop();
                }
                _ => {
                    history["workspaces"][0]["tabs"][0]["panes"]["8"] = screen(FOREIGN_PANE_SCREEN)
                }
            }
            history
        });
        assert_no_pane_shows_saved_screen(&server, why);
        server.stop();
    }
}

#[test]
fn a_history_file_with_a_fingerprint_of_another_layout_is_not_replayed_whatever_its_structure() {
    let mut server = start_server_with_history(&|| {
        let mut history = legacy_history();
        history["layout_fingerprint"] = serde_json::json!("0".repeat(64));
        history
    });
    assert_no_pane_shows_saved_screen(&server, "a fingerprint that is not this layout's");
    server.stop();
}
