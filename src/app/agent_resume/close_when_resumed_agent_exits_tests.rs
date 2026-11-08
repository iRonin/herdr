//! `[session] close_pane_when_resumed_agent_exits`: what a restart resume types into a pane's shell, and what happens to the
//! pane when the agent ends. Real shells run the typed command; a fake "shell" that records its input shows the exact text.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::time::sleep;

use super::*;
use crate::platform::ChildExitReason;

const WAIT: Duration = Duration::from_secs(30);

fn test_app() -> App {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        crate::app::AppPolicy::TEST,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
    app
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "herdr-close-resumed-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // Resolved, so a path a shell prints equals the one the test names (macOS /var is a link).
    std::fs::canonicalize(dir).unwrap()
}

fn remove_scratch(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn write_executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A workspace with one pane whose terminal waits to resume `argv` in `cwd`; returns the pane and terminal ids.
fn pane_waiting_to_resume(
    app: &mut App,
    cwd: &Path,
    argv: Vec<String>,
) -> (crate::layout::PaneId, crate::terminal::TerminalId) {
    let workspace = crate::workspace::Workspace::test_new("resumed");
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
    app.state.workspaces = vec![workspace];
    app.state.active = Some(0);
    app.state.ensure_test_terminals();
    let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
    terminal.cwd = cwd.to_path_buf();
    terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
        agent: "codex".into(),
        argv,
        dedupe_key: format!("close-resumed-{terminal_id}"),
        reported: false,
    });
    (pane_id, terminal_id)
}

fn shut_down_runtimes(app: &mut App) {
    for (_, runtime) in app.terminal_runtimes.drain() {
        runtime.shutdown();
    }
}

/// A shell named `name` that records what is typed into it, one line per command, instead of running it.
fn recording_shell(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    let shells = dir.join("shells");
    std::fs::create_dir_all(&shells).unwrap();
    let typed = dir.join(format!("typed-by-{name}"));
    let shell = shells.join(name);
    write_executable(
        &shell,
        &format!("#!/bin/sh\nexec cat >> '{}'\n", typed.display()),
    );
    (shell, typed)
}

async fn wait_for_lines(file: &Path, lines: usize) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        if text.lines().count() >= lines {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{} never got {lines} line(s); it holds {text:?}",
            file.display()
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// What typing a resume of `argv` into a shell named `name` records, for a given setting.
async fn typed_for(name: &str, setting: bool, argv: &[&str]) -> (String, bool) {
    let dir = scratch_dir("typed");
    let (shell, typed) = recording_shell(&dir, name);
    let mut app = test_app();
    app.state.default_shell = shell.display().to_string();
    app.state.close_pane_when_resumed_agent_exits = setting;
    let (_pane_id, terminal_id) = pane_waiting_to_resume(
        &mut app,
        &dir,
        argv.iter().map(|part| part.to_string()).collect(),
    );
    assert!(app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true));
    let text = wait_for_lines(&typed, 1).await;
    let flagged = app.state.terminals[&terminal_id].closes_when_resumed_agent_exits;
    shut_down_runtimes(&mut app);
    remove_scratch(&dir);
    (text, flagged)
}

const AGENT_ARGV: [&str; 3] = ["fake-agent", "--session", "/some dir/it's.jsonl"];
const AGENT_COMMAND: &str = "fake-agent --session '/some dir/it'\\''s.jsonl'";

/// The default is upstream's behaviour: the command is typed exactly as it always was, and the terminal carries no flag.
#[tokio::test]
async fn with_the_setting_off_the_resume_command_is_typed_exactly_as_upstream_types_it() {
    for shell in ["sh", "dash", "bash", "zsh", "fish", "nu", "pwsh"] {
        let (typed, flagged) = typed_for(shell, false, &AGENT_ARGV).await;
        assert_eq!(typed, format!("{AGENT_COMMAND}\n"), "{shell}");
        assert!(!flagged, "{shell}");
    }
}

/// The text is known for the shells a test runs and for no other: a path, a login shell's leading dash and a name that only
/// contains a proven name all resolve by the shell's own base name.
#[test]
fn the_exit_text_is_known_for_proven_shells_only() {
    for (shell, expected) in [
        ("/bin/sh", Some(" && exit")),
        ("sh", Some(" && exit")),
        ("/usr/bin/dash", Some(" && exit")),
        ("/opt/homebrew/bin/bash", Some(" && exit")),
        ("-zsh", Some(" && exit")),
        ("/usr/bin/fish", Some("; and exit")),
        ("fish", Some("; and exit")),
        ("nu", None),
        ("/usr/bin/pwsh", None),
        ("powershell.exe", None),
        ("bash.exe", None),
        ("csh", None),
        ("tcsh", None),
        ("ksh", None),
        ("mksh", None),
        ("ash", None),
        ("elvish", None),
        ("xonsh", None),
        ("bash5", None),
        ("/bin/bash-wrapper", None),
        ("", None),
        ("/", None),
    ] {
        assert_eq!(exit_with_command_suffix(shell), expected, "{shell:?}");
    }
}

/// With the setting on, a shell whose spelling is proven gets the text that ends it after a successful command, and the
/// terminal is flagged. The command itself is untouched.
#[tokio::test]
async fn with_the_setting_on_a_supported_shell_is_told_to_exit_after_the_command() {
    for (shell, suffix) in [
        ("sh", " && exit"),
        ("dash", " && exit"),
        ("bash", " && exit"),
        ("zsh", " && exit"),
        ("fish", "; and exit"),
    ] {
        let (typed, flagged) = typed_for(shell, true, &AGENT_ARGV).await;
        assert_eq!(typed, format!("{AGENT_COMMAND}{suffix}\n"), "{shell}");
        assert!(flagged, "{shell}");
    }
}

/// Collects what the logger writes on this thread.
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;
    fn make_writer(&'a self) -> LogCapture {
        self.clone()
    }
}

impl LogCapture {
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .finish(),
        )
    }

    fn lines_naming_an_unapplied_setting(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| line.contains("close_pane_when_resumed_agent_exits is not applied"))
            .map(str::to_string)
            .collect()
    }
}

/// A shell whose spelling is not proven gets the command unchanged and the pane is not flagged: herdr never types syntax a
/// shell might reject.
#[tokio::test]
async fn an_unsupported_shell_gets_the_command_unchanged() {
    for shell in [
        "nu", "pwsh", "csh", "tcsh", "ksh", "elvish", "xonsh", "myshell",
    ] {
        let (typed, flagged) = typed_for(shell, true, &AGENT_ARGV).await;
        assert_eq!(typed, format!("{AGENT_COMMAND}\n"), "{shell}");
        assert!(!flagged, "{shell}");
    }
}

/// The log names an unsupported shell once, however many panes use it, and says nothing for a supported shell or when the
/// setting is off.
#[tokio::test]
async fn an_unsupported_shell_is_named_once_in_the_log() {
    let dir = scratch_dir("once");
    let capture = LogCapture::default();
    let guard = capture.install();
    let mut app = test_app();
    app.state.close_pane_when_resumed_agent_exits = true;
    let mut expected_lines = 0;
    for (round, (name, named)) in [
        ("nu", 1),
        ("nu", 0),
        ("bash", 0),
        ("nu", 0),
        ("pwsh", 1),
        ("pwsh", 0),
    ]
    .into_iter()
    .enumerate()
    {
        let (shell, typed) = recording_shell(&dir, name);
        app.state.default_shell = shell.display().to_string();
        let argv = AGENT_ARGV.iter().map(|part| part.to_string()).collect();
        let (_pane_id, terminal_id) = pane_waiting_to_resume(&mut app, &dir, argv);
        assert!(app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true));
        // `recording_shell` appends to one file per shell name: count the lines that name has so far.
        let so_far = [
            ("nu", 1),
            ("nu", 0),
            ("bash", 0),
            ("nu", 0),
            ("pwsh", 1),
            ("pwsh", 0),
        ][..=round]
            .iter()
            .filter(|(other, _)| *other == name)
            .count();
        wait_for_lines(&typed, so_far).await;
        expected_lines += named;
        assert_eq!(
            capture.lines_naming_an_unapplied_setting().len(),
            expected_lines,
            "after {name} (round {round})"
        );
        shut_down_runtimes(&mut app);
    }
    assert_eq!(expected_lines, 2);

    // Off: nothing is said even for a shell that would not be supported.
    app.state.close_pane_when_resumed_agent_exits = false;
    let (shell, typed) = recording_shell(&dir, "elvish");
    app.state.default_shell = shell.display().to_string();
    let argv = AGENT_ARGV.iter().map(|part| part.to_string()).collect();
    let (_pane_id, terminal_id) = pane_waiting_to_resume(&mut app, &dir, argv);
    assert!(app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true));
    wait_for_lines(&typed, 1).await;
    shut_down_runtimes(&mut app);
    drop(guard);
    assert_eq!(capture.lines_naming_an_unapplied_setting().len(), 2);
    remove_scratch(&dir);
}

/// The plan can start long after the restore planned it, so the setting counts as it stands when the command is typed.
#[tokio::test]
async fn the_setting_is_read_when_the_command_is_typed_not_when_the_restore_planned_it() {
    for (at_restore, when_typed) in [(false, true), (true, false)] {
        let dir = scratch_dir("deferred");
        let (shell, typed) = recording_shell(&dir, "bash");
        let mut app = test_app();
        app.state.default_shell = shell.display().to_string();
        app.state.close_pane_when_resumed_agent_exits = at_restore;
        let argv = AGENT_ARGV.iter().map(|part| part.to_string()).collect();
        let (_pane_id, terminal_id) = pane_waiting_to_resume(&mut app, &dir, argv);
        // A config reload between the restore and the resume.
        app.state.close_pane_when_resumed_agent_exits = when_typed;
        assert!(app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true));
        let text = wait_for_lines(&typed, 1).await;
        let expected = if when_typed {
            format!("{AGENT_COMMAND} && exit\n")
        } else {
            format!("{AGENT_COMMAND}\n")
        };
        assert_eq!(text, expected, "restore {at_restore}, typed {when_typed}");
        assert_eq!(
            app.state.terminals[&terminal_id].closes_when_resumed_agent_exits,
            when_typed
        );
        shut_down_runtimes(&mut app);
        remove_scratch(&dir);
    }
}

// ---- real shells --------------------------------------------------------------------------------------------------

const REAL_SHELLS: [&str; 5] = ["sh", "dash", "bash", "zsh", "fish"];

/// The path of a shell on this machine. On the Linux gate image every one of them is installed, so a missing one fails the
/// test there: a skipped proof is not a proof. Elsewhere a missing shell is skipped with a note.
fn find_shell(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    dirs.extend(["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"].map(PathBuf::from));
    let found = dirs
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file());
    if found.is_none() {
        if cfg!(target_os = "linux") {
            panic!("{name} must be installed on the Linux gate image");
        }
        eprintln!(
            "SKIPPED: {name} is not installed on this machine, so its syntax is not proven here"
        );
    }
    found
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// The agent ends with status 0.
    Clean,
    /// The agent ends with status 1: a crash.
    Crash,
    /// The agent kills itself with SIGKILL.
    Killed,
    /// The command does not exist (status 127).
    Missing,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The shell exited with the agent: the server was told the pane's process ended.
    PaneEnded(ChildExitReason),
    /// The shell ran the command typed after the agent, so it was still alive and reading its input.
    ShellStayed,
}

/// Resumes a fake agent that ends as `ending` in a real `shell`, with the setting `setting`, and says what became of the
/// pane. The command typed after the resume (`touch ALIVE`) runs only if the shell is still there when the agent has ended.
async fn resume_and_watch(
    shell: &Path,
    setting: bool,
    ending: Ending,
) -> (Outcome, App, crate::layout::PaneId) {
    let dir = scratch_dir("real");
    let agent = dir.join("fake-agent");
    let body = match ending {
        Ending::Clean => "exit 0",
        Ending::Crash => "exit 1",
        Ending::Killed => "kill -9 $$",
        Ending::Missing => "exit 0",
    };
    write_executable(&agent, &format!("#!/bin/sh\n{body}\n"));
    let program = match ending {
        Ending::Missing => dir.join("no-such-agent"),
        _ => agent,
    };
    let alive = dir.join("ALIVE");
    // A quiet home: zsh would ask its new-user questions in a home without a startup file, and every shell writes history.
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".zshrc"), "").unwrap();

    let mut app = test_app();
    app.state.default_shell = shell.display().to_string();
    app.state.close_pane_when_resumed_agent_exits = setting;
    let (pane_id, terminal_id) =
        pane_waiting_to_resume(&mut app, &dir, vec![program.display().to_string()]);
    {
        let _guard = crate::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved: Vec<_> = ["HOME", "ZDOTDIR", "HISTFILE", "XDG_DATA_HOME"]
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        std::env::set_var("HOME", &home);
        std::env::set_var("ZDOTDIR", &home);
        std::env::set_var("HISTFILE", home.join("history"));
        std::env::set_var("XDG_DATA_HOME", home.join("data"));
        let started = app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true);
        for (key, value) in saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        assert!(started, "the resume did not start in {}", shell.display());
    }
    // Typed ahead: it runs only when the shell goes on reading after the resume command. A shell that exited with the
    // agent never reads it.
    let runtime = app.terminal_runtimes.get(&terminal_id).unwrap();
    runtime
        .try_send_bytes(Bytes::from(format!("touch '{}'\r", alive.display())))
        .unwrap();

    let deadline = Instant::now() + WAIT;
    let outcome = loop {
        if alive.exists() {
            break Outcome::ShellStayed;
        }
        match app.event_rx.try_recv() {
            Ok(crate::events::AppEvent::PaneDied {
                pane_id: died,
                exit_reason,
            }) if died == pane_id => break Outcome::PaneEnded(exit_reason),
            Ok(_) => continue,
            Err(_) => {}
        }
        assert!(
            Instant::now() < deadline,
            "{} with {ending:?}: neither the shell ended nor ran the next command",
            shell.display()
        );
        sleep(Duration::from_millis(20)).await;
    };
    remove_scratch(&dir);
    (outcome, app, pane_id)
}

/// The pane closes when the resumed agent ends normally, in every shell whose spelling is proven; with the setting off the
/// same agent ending the same way leaves the shell (the control: the same harness sees a pane that stays).
#[tokio::test]
async fn a_clean_agent_end_closes_the_pane_and_only_when_the_setting_is_on() {
    for name in REAL_SHELLS {
        let Some(shell) = find_shell(name) else {
            continue;
        };
        let (outcome, mut app, pane_id) = resume_and_watch(&shell, true, Ending::Clean).await;
        assert_eq!(
            outcome,
            Outcome::PaneEnded(ChildExitReason::Exited),
            "{name} on"
        );
        // The server's own handling of that event removes the pane.
        assert!(app.find_pane(pane_id).is_some());
        app.handle_internal_event(crate::events::AppEvent::PaneDied {
            pane_id,
            exit_reason: ChildExitReason::Exited,
        });
        assert!(app.find_pane(pane_id).is_none(), "{name}: the pane stayed");
        shut_down_runtimes(&mut app);

        let (outcome, mut app, pane_id) = resume_and_watch(&shell, false, Ending::Clean).await;
        assert_eq!(outcome, Outcome::ShellStayed, "{name} off");
        assert!(app.find_pane(pane_id).is_some());
        shut_down_runtimes(&mut app);
    }
}

/// A crash, a kill and a missing command keep the pane. An agent binary that crashes on its first render does so in every
/// resumed pane at once: closing those panes would take their saved sessions out of the layout.
#[tokio::test]
async fn an_agent_that_does_not_end_normally_keeps_its_pane() {
    for name in REAL_SHELLS {
        let Some(shell) = find_shell(name) else {
            continue;
        };
        for ending in [Ending::Crash, Ending::Killed, Ending::Missing] {
            let (outcome, mut app, pane_id) = resume_and_watch(&shell, true, ending).await;
            assert_eq!(outcome, Outcome::ShellStayed, "{name} {ending:?}");
            assert!(app.find_pane(pane_id).is_some(), "{name} {ending:?}");
            shut_down_runtimes(&mut app);
        }
    }
}

// ---- the saved layout ---------------------------------------------------------------------------------------------

fn pi_session() -> crate::agent_resume::PersistedAgentSession {
    crate::agent_resume::PersistedAgentSession {
        source: "herdr:pi".into(),
        agent: "pi".into(),
        session_ref: crate::agent_resume::AgentSessionRef::path("/sessions/agent.jsonl").unwrap(),
    }
}

/// An app with one two-pane workspace; pane one is the resumed agent's, with its session and a detected agent, as it is in a
/// running server, and `flagged` says whether its shell carries the exit text.
fn app_with_resumed_agent(flagged: bool) -> (App, crate::layout::PaneId, PathBuf) {
    let config_home = scratch_dir("layout");
    std::env::set_var("XDG_CONFIG_HOME", &config_home);
    std::env::remove_var(crate::session::SESSION_ENV_VAR);
    let mut app = test_app();
    app.policy.persist_session = true;
    let mut workspace = crate::workspace::Workspace::test_new("resumed");
    let agent_pane = workspace.tabs[0].root_pane;
    let _other_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
    let terminal_id = workspace.terminal_id(agent_pane).cloned().unwrap();
    app.state.workspaces = vec![workspace];
    app.state.active = Some(0);
    app.state.ensure_test_terminals();
    let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
    terminal.persisted_agent_session = Some(pi_session());
    terminal.set_detected_state(
        Some(crate::detect::Agent::Pi),
        crate::detect::AgentState::Idle,
    );
    terminal.closes_when_resumed_agent_exits = flagged;
    (app, agent_pane, config_home)
}

/// The session value saved with the layout's first workspace's first tab's panes: `None` when no layout is saved, `Some(None)`
/// when no pane in it holds an agent session.
fn saved_agent_session_value() -> Option<Option<String>> {
    let snapshot = crate::persist::load()?;
    let tab = &snapshot.workspaces.first()?.tabs[0];
    Some(
        tab.panes
            .values()
            .find_map(|pane| pane.agent_session.as_ref())
            .map(|session| session.value.clone()),
    )
}

/// The operating system signals every process when it shuts down: the agent exits 0, the shell runs `exit`, and the server
/// can get the pane's end before it knows it is stopping. The layout it saves last must still hold the pane and the agent's
/// session. The checkpoint is taken before the pane's agent exit is applied, which clears the session from the pane.
#[tokio::test]
async fn a_resumed_pane_that_ends_while_the_server_stops_stays_in_the_saved_layout_with_its_session(
) {
    let _guard = crate::config::test_config_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, agent_pane, config_home) = app_with_resumed_agent(true);

    app.handle_internal_event(crate::events::AppEvent::PaneDied {
        pane_id: agent_pane,
        exit_reason: ChildExitReason::Exited,
    });
    assert!(
        app.find_pane(agent_pane).is_none(),
        "the pane closes in the server"
    );
    app.save_session_on_shutdown();

    let saved = saved_agent_session_value()
        .expect("the layout saved at stop must still exist")
        .expect("the saved layout must keep the agent's session");
    assert_eq!(saved, "/sessions/agent.jsonl");
    let snapshot = crate::persist::load().unwrap();
    assert_eq!(
        snapshot.workspaces[0].tabs[0].panes.len(),
        2,
        "both panes are in the saved layout"
    );

    std::env::remove_var("XDG_CONFIG_HOME");
    remove_scratch(&config_home);
}

/// The control of the test above: a pane without the exit text that ends the same way is removed from the layout the server
/// saves last (upstream's behaviour), so the test above can fail.
#[tokio::test]
async fn an_ordinary_pane_that_ends_while_the_server_stops_is_removed_from_the_saved_layout() {
    let _guard = crate::config::test_config_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, agent_pane, config_home) = app_with_resumed_agent(false);

    app.handle_internal_event(crate::events::AppEvent::PaneDied {
        pane_id: agent_pane,
        exit_reason: ChildExitReason::Exited,
    });
    app.save_session_on_shutdown();

    let snapshot = crate::persist::load().expect("the other pane is left");
    assert_eq!(snapshot.workspaces[0].tabs[0].panes.len(), 1);
    assert!(
        snapshot.workspaces[0].tabs[0]
            .panes
            .values()
            .all(|pane| pane.agent_session.is_none()),
        "an unflagged pane's end is saved as it happened"
    );

    std::env::remove_var("XDG_CONFIG_HOME");
    remove_scratch(&config_home);
}

/// The checkpoint is replaced by the next ordinary autosave: a pane that ended normally does not come back for ever.
#[tokio::test]
async fn the_next_autosave_removes_a_resumed_pane_that_ended() {
    let _guard = crate::config::test_config_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, agent_pane, config_home) = app_with_resumed_agent(true);

    app.handle_internal_event(crate::events::AppEvent::PaneDied {
        pane_id: agent_pane,
        exit_reason: ChildExitReason::Exited,
    });
    app.start_background_session_save();
    if let Some(thread) = app.session_save_thread.take() {
        thread.join().unwrap();
    }
    app.save_session_on_shutdown();

    let snapshot = crate::persist::load().expect("the other pane is left");
    assert_eq!(snapshot.workspaces[0].tabs[0].panes.len(), 1);

    std::env::remove_var("XDG_CONFIG_HOME");
    remove_scratch(&config_home);
}

fn session_history_file(config_home: &Path) -> PathBuf {
    config_home
        .join(if cfg!(debug_assertions) {
            "herdr-dev"
        } else {
            "herdr"
        })
        .join("session-history.json")
}

/// The checkpoint of a resumed agent's pane runs on the server loop, once per agent exit in a burst of them, and with
/// `[experimental] pane_history` on a full save takes seconds for a large layout (3.9 s for 250 panes with 200 KB of history
/// each; 9 ms without the history). It writes the layout only and leaves the history file exactly as it is: not rewritten, not
/// cleared. Control: the checkpoint upstream takes for an ordinary pane killed by a signal is the full save and does rewrite
/// the history file, so this test can tell the two apart.
#[tokio::test]
async fn the_checkpoint_of_a_resumed_agents_pane_writes_the_layout_and_leaves_the_pane_history_file_alone(
) {
    let _guard = crate::config::test_config_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    const OLDER_SAVE: &str = r#"{"version":3,"layout_fingerprint":"older-save","workspaces":[]}"#;
    for flagged in [true, false] {
        let (mut app, agent_pane, config_home) = app_with_resumed_agent(flagged);
        app.persist_pane_history = true;
        let history = session_history_file(&config_home);
        std::fs::create_dir_all(history.parent().unwrap()).unwrap();
        std::fs::write(&history, OLDER_SAVE).unwrap();

        if flagged {
            assert!(app.checkpoint_session_before_resumed_agent_pane_exit(agent_pane));
        } else {
            app.handle_internal_event(crate::events::AppEvent::PaneDied {
                pane_id: agent_pane,
                exit_reason: ChildExitReason::Interrupted,
            });
        }

        let snapshot = crate::persist::load().expect("the checkpoint must write the layout");
        assert_eq!(
            snapshot.workspaces[0].tabs[0].panes.len(),
            2,
            "flagged={flagged}: the checkpoint is taken while both panes are in the layout"
        );
        let after = std::fs::read_to_string(&history)
            .unwrap_or_else(|_| panic!("flagged={flagged}: the history file was removed"));
        if flagged {
            assert_eq!(
                after, OLDER_SAVE,
                "the checkpoint of a resumed agent's pane must not rewrite the pane history file"
            );
        } else {
            assert_ne!(
                after, OLDER_SAVE,
                "control: upstream's full checkpoint rewrites the pane history file"
            );
        }

        std::env::remove_var("XDG_CONFIG_HOME");
        remove_scratch(&config_home);
    }
}

/// While a background save is writing it holds the session writer. The checkpoint must not wait for it: a wait would stall the
/// server loop for the rest of that save (seconds with a large pane history). It relies on the layout the running save writes,
/// which was captured before this exit and still holds the pane and its agent session, and it stays pending on that file.
#[tokio::test]
async fn the_checkpoint_of_a_resumed_agents_pane_does_not_wait_for_a_background_save() {
    let _guard = crate::config::test_config_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, agent_pane, config_home) = app_with_resumed_agent(true);
    let writer = app.session_writer.clone();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let background_save = std::thread::spawn(move || {
        let _held = writer.lock().unwrap();
        locked_tx.send(()).unwrap();
        // Held until the test says so, and for ten seconds at most: a checkpoint that waits fails the test instead of hanging it.
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });
    locked_rx.recv().unwrap();

    let started = Instant::now();
    assert!(app.checkpoint_session_before_resumed_agent_pane_exit(agent_pane));
    let waited = started.elapsed();
    // A checkpoint that waited outlived the ten seconds, and the holder is gone: report the wait below, not a send error.
    let _ = release_tx.send(());
    background_save.join().unwrap();

    assert!(
        waited < Duration::from_secs(3),
        "the checkpoint waited {waited:?} for a background save"
    );
    assert!(
        app.pane_exit_checkpoint_pending,
        "the checkpoint is pending on the layout the running save writes"
    );
    assert!(
        crate::persist::load().is_none(),
        "the checkpoint wrote nothing itself while the writer was busy"
    );

    std::env::remove_var("XDG_CONFIG_HOME");
    remove_scratch(&config_home);
}

/// Holds the session writer from another thread, as a background save does while it writes, until the returned sender fires
/// (or ten seconds pass, so that a checkpoint that waits fails its test instead of hanging it).
fn hold_the_session_writer(
    app: &App,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let writer = app.session_writer.clone();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _held = writer.lock().unwrap();
        locked_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });
    locked_rx.recv().unwrap();
    (release_tx, holder)
}

/// A checkpoint that finds the session writer busy writes nothing and says the pane is covered. The layout the running save
/// writes was captured before the changes made since, so a stop that trusts it drops them: a new pane is missing from the
/// saved layout and its agent is not resumed. The stop must write the layout the checkpoint captured, after the running save
/// ends, and that layout holds both the changes and the pane that ended, with the session it is resumed from.
#[tokio::test]
async fn a_stop_after_a_checkpoint_that_found_the_writer_busy_still_saves_what_changed_since_the_background_save_began(
) {
    let _guard = crate::config::test_config_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, agent_pane, config_home) = app_with_resumed_agent(true);
    let (release, holder) = hold_the_session_writer(&app);

    // The background save captures its layout now and waits for the writer.
    app.start_background_session_save();
    assert!(app.session_save_thread.is_some());

    // A change after that capture: a new workspace.
    app.state
        .workspaces
        .push(crate::workspace::Workspace::test_new("added"));
    app.state.ensure_test_terminals();
    app.state.mark_session_dirty();
    app.sync_session_save_schedule();

    // The flagged agent's pane ends while the writer is busy.
    app.handle_internal_event(crate::events::AppEvent::PaneDied {
        pane_id: agent_pane,
        exit_reason: ChildExitReason::Exited,
    });
    assert!(app.find_pane(agent_pane).is_none());

    let _ = release.send(());
    holder.join().unwrap();
    app.save_session_on_shutdown();
    if let Some(thread) = app.session_save_thread.take() {
        thread.join().unwrap();
    }

    let snapshot = crate::persist::load().expect("the stop must leave a saved layout");
    assert_eq!(
        snapshot.workspaces.len(),
        2,
        "the workspace added after the background save began is not in the saved layout"
    );
    assert_eq!(
        snapshot.workspaces[0].tabs[0].panes.len(),
        2,
        "both panes of the first workspace are in the saved layout"
    );
    assert_eq!(
        snapshot.workspaces[0].tabs[0]
            .panes
            .values()
            .find_map(|pane| pane.agent_session.as_ref())
            .map(|session| session.value.as_str()),
        Some("/sessions/agent.jsonl"),
        "the pane that ended lost the session it is resumed from"
    );

    std::env::remove_var("XDG_CONFIG_HOME");
    remove_scratch(&config_home);
}

/// The layout a busy checkpoint kept is replaced by the next checkpoint that gets the writer: that one writes a newer layout, and
/// a stop must not write the older one over it. (This passes before the kept layout exists; a mutant that keeps the old one
/// shows what it guards.)
#[tokio::test]
async fn a_later_checkpoint_that_gets_the_writer_replaces_the_layout_an_earlier_busy_one_kept() {
    let _guard = crate::config::test_config_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, _agent_pane, config_home) = app_with_resumed_agent(true);

    let (release, holder) = hold_the_session_writer(&app);
    app.checkpoint_session_layout_before_pane_exit();
    let _ = release.send(());
    holder.join().unwrap();

    app.state
        .workspaces
        .push(crate::workspace::Workspace::test_new("added later"));
    app.state.ensure_test_terminals();
    app.state.mark_session_dirty();
    app.checkpoint_session_layout_before_pane_exit();
    app.save_session_on_shutdown();

    let snapshot = crate::persist::load().expect("the later checkpoint wrote a layout");
    assert_eq!(
        snapshot.workspaces.len(),
        2,
        "a stop wrote the older layout that the busy checkpoint kept over the newer one"
    );

    std::env::remove_var("XDG_CONFIG_HOME");
    remove_scratch(&config_home);
}

/// When the last workspace's panes end, the server creates a replacement workspace on its own, which schedules a save and
/// would clear the checkpoint. The layout a busy checkpoint kept must survive that, as upstream's checkpoint does.
#[tokio::test]
async fn the_layout_a_busy_checkpoint_kept_survives_the_automatic_workspace_that_replaces_the_last_one(
) {
    let _guard = crate::config::test_config_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, agent_pane, config_home) = app_with_resumed_agent(true);
    app.state.default_shell = "/bin/sh".into();
    let other_pane = app.state.workspaces[0].tabs[0]
        .panes
        .keys()
        .copied()
        .find(|id| *id != agent_pane)
        .unwrap();
    let (release, holder) = hold_the_session_writer(&app);

    app.handle_internal_event(crate::events::AppEvent::PaneDied {
        pane_id: agent_pane,
        exit_reason: ChildExitReason::Exited,
    });
    // The operating system signals every process: the other pane ends too, and the workspace is gone.
    app.handle_internal_event(crate::events::AppEvent::PaneDied {
        pane_id: other_pane,
        exit_reason: ChildExitReason::Interrupted,
    });
    assert!(app.state.workspaces.is_empty());
    assert!(app.ensure_default_workspace());

    let _ = release.send(());
    holder.join().unwrap();
    app.save_session_on_shutdown();

    let snapshot = crate::persist::load().expect("the stop must leave a saved layout");
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(
        snapshot.workspaces[0].tabs[0].panes.len(),
        2,
        "both panes are in the saved layout"
    );
    assert_eq!(
        snapshot.workspaces[0].tabs[0]
            .panes
            .values()
            .find_map(|pane| pane.agent_session.as_ref())
            .map(|session| session.value.as_str()),
        Some("/sessions/agent.jsonl")
    );

    shut_down_runtimes(&mut app);
    std::env::remove_var("XDG_CONFIG_HOME");
    remove_scratch(&config_home);
}

/// A background save captures its layout on the server loop and then starts its thread, which takes the writer only when it
/// runs. A checkpoint that comes in between finds the writer free, but the save that is about to write is still older than
/// the changes made since its capture: it would write its layout over the checkpoint's, and a stop that trusts the checkpoint
/// would drop those changes. A running save thread must count as busy, as a held writer does.
#[tokio::test]
async fn a_checkpoint_before_a_started_background_save_takes_the_writer_still_keeps_its_layout_for_the_stop(
) {
    let _guard = crate::config::test_config_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, agent_pane, config_home) = app_with_resumed_agent(true);

    // A background save whose layout was captured now and whose thread has not yet taken the writer.
    let captured = crate::persist::capture(
        &app.state.workspaces,
        &app.state.terminals,
        &app.terminal_runtimes,
        app.state.active,
        app.state.selected,
    );
    let writer = app.session_writer.clone();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    app.session_save_thread = Some(std::thread::spawn(move || {
        let _ = wait.recv_timeout(Duration::from_secs(10));
        writer.lock().unwrap().save(&captured, None);
    }));

    // A change after that capture: a new workspace.
    app.state
        .workspaces
        .push(crate::workspace::Workspace::test_new("added"));
    app.state.ensure_test_terminals();
    app.state.mark_session_dirty();
    app.sync_session_save_schedule();

    // The flagged agent's pane ends before the save thread takes the writer.
    app.handle_internal_event(crate::events::AppEvent::PaneDied {
        pane_id: agent_pane,
        exit_reason: ChildExitReason::Exited,
    });
    assert!(app.find_pane(agent_pane).is_none());

    // The save thread now writes its older layout; then the server stops.
    let _ = go.send(());
    app.save_session_on_shutdown();
    if let Some(thread) = app.session_save_thread.take() {
        thread.join().unwrap();
    }

    let snapshot = crate::persist::load().expect("the stop must leave a saved layout");
    assert_eq!(
        snapshot.workspaces.len(),
        2,
        "the older background save wrote over the checkpoint: the workspace added after its capture is not in the saved layout"
    );
    assert_eq!(
        snapshot.workspaces[0].tabs[0]
            .panes
            .values()
            .find_map(|pane| pane.agent_session.as_ref())
            .map(|session| session.value.as_str()),
        Some("/sessions/agent.jsonl"),
        "the pane that ended lost the session it is resumed from"
    );

    shut_down_runtimes(&mut app);
    std::env::remove_var("XDG_CONFIG_HOME");
    remove_scratch(&config_home);
}
