#![cfg(unix)]

pub mod support;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

// No real SSH connection or server is started. Stop at startup after recording setup actions.
const SSH: &str = r#"#!/bin/sh
for arg do
    last=$arg
    if [ "$arg" = 'StrictHostKeyChecking=yes' ]; then strict_host_key_check=yes; fi
done
if [ "$FAKE_STRICT_HOST_KEY_FAILURE" = yes ] && [ "$strict_host_key_check" = yes ]; then
    echo 'No RSA host key is known for fake-host and you have requested strict checking.' >&2
    echo 'Host key verification failed.' >&2
    exit 255
fi
if [ "$last" = 'command -v herdr' ]; then
    if [ "$FAKE_INSTALLED" != missing ]; then echo /home/remote/.local/bin/herdr; fi
    exit 0
fi
case "$last" in
    'tee '*) cat >/dev/null; exit 0 ;;
esac
case "$last" in
    *'herdr-remote-output-ready:1'*) script=$last ;;
    *) script=$(cat) ;;
esac
printf '\n%s\n' 'herdr-remote-output-ready:1'
case "$script" in
    *'uname -s'*) echo platform >>"$FAKE_ROOT/probes"; uname -s; uname -m ;;
    *'version='*)
        echo candidates >>"$FAKE_ROOT/probes"
        if [ "$FAKE_INSTALLED" != missing ]; then echo /home/remote/.local/bin/herdr; fi ;;
    *'status client --json'*)
        if [ "$FAKE_INSTALLED" = new ] || [ -f "$FAKE_ROOT/installed" ]; then
            printf '%s\n' "$FAKE_CLIENT_STATUS"
        else
            echo '{"version":"0.8.2","protocol":20}'
        fi ;;
    *'status server --json'*)
        if [ -f "$FAKE_ROOT/stopped" ]; then
            echo '{"running":false}'
        elif [ "$FAKE_STRICT_HOST_KEY_FAILURE" = yes ]; then
            echo '{"running":true,"version":"0.8.2","capabilities":{"live_handoff":true,"detached_server_daemon":true,"endpoint_protocol_generation":1,"surface_interest":true,"health_check":true}}'
        else
            echo '{"running":true,"version":"0.8.2","capabilities":{"live_handoff":true,"detached_server_daemon":true}}'
        fi ;;
    *'server live-handoff'*) echo handoff >>"$FAKE_ROOT/actions"; exit 1 ;;
    *'server stop'*) echo stop >>"$FAKE_ROOT/actions"; touch "$FAKE_ROOT/stopped" ;;
    *'session list --json'*)
        echo sessions >>"$FAKE_ROOT/probes"
        if [ "$FAKE_SESSIONS" = failed ]; then echo 'session query denied' >&2; exit 1; fi
        printf '%s\n' "$FAKE_SESSIONS" ;;
    *'remote-client-bridge'*)
        printf '%s\n' "$script" >"$FAKE_ROOT/start-command"
        echo start >>"$FAKE_ROOT/actions"; echo 'test startup failure' >&2; exit 1 ;;
    *'mkdir -p'*) printf '/fake/tmp\000/fake/herdr\000' ;;
    *'chmod 755'*) echo install >>"$FAKE_ROOT/actions"; touch "$FAKE_ROOT/installed" ;;
    *'command -v herdr'*) if [ "$FAKE_INSTALLED" != missing ]; then echo /home/remote/.local/bin/herdr; fi ;;
    *'test -x '*) exit 1 ;;
    *) echo "unexpected fake SSH script: $script" >&2; exit 1 ;;
esac
"#;

struct SetupResult {
    output: String,
    actions: String,
    prompts: usize,
    success: bool,
    probes: String,
    start_command: String,
}

#[derive(Default)]
struct SetupOptions<'a> {
    sessions: Option<&'a str>,
    selection: &'a [u8],
    explicit_session: Option<&'a str>,
    noninteractive: bool,
}

fn setup(installed: &str, answer: &str, handoff: bool) -> SetupResult {
    setup_with_strict_host_key_failure(installed, answer, handoff, false)
}

fn setup_with_strict_host_key_failure(
    installed: &str,
    answer: &str,
    handoff: bool,
    strict_host_key_failure: bool,
) -> SetupResult {
    setup_options(
        installed,
        answer,
        handoff,
        strict_host_key_failure,
        SetupOptions::default(),
    )
}

fn setup_options(
    installed: &str,
    answer: &str,
    handoff: bool,
    strict_host_key_failure: bool,
    options: SetupOptions<'_>,
) -> SetupResult {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/herdr-machine-setup-{}-{}-{}-{}-{}-{}",
        std::process::id(),
        installed,
        answer.trim().is_empty(),
        handoff,
        strict_host_key_failure,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let app = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("config").join(app)).unwrap();
    fs::write(root.join("bin/ssh"), SSH).unwrap();
    fs::set_permissions(root.join("bin/ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        root.join("config").join(app).join("config.toml"),
        "onboarding = false\n[remote]\nmanage_ssh_config = false\n",
    )
    .unwrap();
    let mut herdr_command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    support::sanitize_herdr_env_command(&mut herdr_command);
    let status = herdr_command
        .args(["status", "client", "--json"])
        .output()
        .unwrap();
    assert!(status.status.success());

    let pair = native_pty_system().openpty(PtySize::default()).unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    support::sanitize_herdr_env(&mut command);
    if handoff {
        command.args(["--remote", "fake-host", "--handoff"]);
    } else {
        command.args(["machine", "add", "fake-host", "--label", "VPS"]);
        if let Some(session) = options.explicit_session {
            command.args(["--remote-session", session]);
        }
    }
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", root.join("bin").display()),
    );
    command.env("HOME", &root);
    command.env("XDG_CONFIG_HOME", root.join("config"));
    command.env("XDG_STATE_HOME", root.join("state"));
    command.env("XDG_RUNTIME_DIR", &root);
    command.env("FAKE_ROOT", &root);
    command.env("FAKE_INSTALLED", installed);
    command.env(
        "FAKE_SESSIONS",
        options
            .sessions
            .unwrap_or(r#"{"sessions":[{"name":"default","running":true}]}"#),
    );
    command.env(
        "FAKE_STRICT_HOST_KEY_FAILURE",
        if strict_host_key_failure { "yes" } else { "no" },
    );
    command.env(
        "FAKE_CLIENT_STATUS",
        String::from_utf8(status.stdout).unwrap(),
    );
    for name in [
        "HERDR_ENV",
        "HERDR_SESSION",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_REMOTE_BINARY",
        "HERDR_CONFIG_PATH",
    ] {
        command.env_remove(name);
    }
    if options.noninteractive {
        // A redirected stdin makes machine add non-interactive while retaining
        // the same isolated SSH fixture and captured terminal diagnostics.
        let mut shell = CommandBuilder::new("/bin/sh");
        shell.args(["-c", "exec \"$@\" </dev/null", "sh"]);
        shell.args(command.get_argv());
        shell.env_clear();
        for (key, value) in command.iter_full_env_as_str() {
            shell.env(key, value);
        }
        command = shell;
    }
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (tx, rx) = mpsc::channel();
    let reading = std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(len) = reader.read(&mut buffer) {
            if len == 0 || tx.send(buffer[..len].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut output = String::new();
    let mut prompts = 0;
    let mut timed_out = false;
    let mut picked = false;
    loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(bytes) => {
                output.push_str(&String::from_utf8_lossy(&bytes));
                if !picked && output.contains("Esc cancel") {
                    writer.write_all(options.selection).unwrap();
                    writer.flush().unwrap();
                    picked = true;
                }
                let count = output.matches("[y/N]").count() + output.matches("[Y/n]").count();
                if count > prompts {
                    writer
                        .write_all(if prompts == 0 {
                            answer.as_bytes()
                        } else {
                            b"n\n"
                        })
                        .unwrap();
                    writer.flush().unwrap();
                    prompts = count;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out = true;
                child.kill().unwrap();
                break;
            }
        }
    }
    let status = child.wait().unwrap();
    drop(writer);
    drop(pair.master);
    reading.join().unwrap();
    let actions = fs::read_to_string(root.join("actions")).unwrap_or_default();
    let probes = fs::read_to_string(root.join("probes")).unwrap_or_default();
    let start_command = fs::read_to_string(root.join("start-command")).unwrap_or_default();
    // Every scenario either cancels or reaches the intentionally failed startup.
    let saved = root
        .join("state")
        .join(app)
        .join("client/endpoints.json")
        .exists();
    fs::remove_dir_all(root).unwrap();
    assert!(
        !saved,
        "failed or cancelled setup saved the machine: {output}"
    );
    assert!(!timed_out, "setup timed out: {output}");
    SetupResult {
        output,
        actions,
        prompts,
        success: status.success(),
        probes,
        start_command,
    }
}

#[test]
fn machine_add_accepts_help_argument_order() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "herdr-machine-add-order-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&root).unwrap();
    let app = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    fs::create_dir_all(root.join("config").join(app)).unwrap();
    fs::write(
        root.join("config").join(app).join("config.toml"),
        "onboarding = false\n[remote]\nmanage_ssh_config = false\n",
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    support::sanitize_herdr_env_command(&mut command);
    command.args(["machine", "add", "--label", "coder", "workstation.coder"]);
    // Reach remote preparation, but never execute SSH or start a server.
    command.env("PATH", root.join("no-executables"));
    command.env("HOME", &root);
    command.env("XDG_CONFIG_HOME", root.join("config"));
    command.env("XDG_STATE_HOME", root.join("state"));
    command.env("XDG_RUNTIME_DIR", &root);
    for name in [
        "HERDR_ENV",
        "HERDR_SESSION",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_REMOTE_BINARY",
        "HERDR_CONFIG_PATH",
    ] {
        command.env_remove(name);
    }
    let output = command.output().unwrap();
    let saved = root
        .join("state")
        .join(app)
        .join("client/endpoints.json")
        .exists();
    fs::remove_dir_all(root).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("machine was not saved"), "{stderr}");
    assert!(!saved, "failed preparation must not save a machine");
}

#[test]
fn machine_add_fresh_host_offers_installation_and_cancellation_saves_nothing() {
    let result = setup("missing", "n\n", false);
    assert_eq!(result.probes, "platform\ncandidates\n", "{}", result.output);
    assert_eq!(result.prompts, 1, "{}", result.output);
    assert!(
        result.output.contains("installation cancelled"),
        "{}",
        result.output
    );
    assert!(result.actions.is_empty(), "{}", result.output);
}

#[test]
fn machine_add_discovers_sessions_once_and_prepares_the_selected_session() {
    for (sessions, selection, expected) in [
        (r#"{"sessions":[]}"#, &b""[..], "default"),
        (
            r#"{"extra":1,"sessions":[{"name":"stopped","running":false},{"name":"bad/name","running":true},{"name":"agents","running":true,"future":true}]}"#,
            &b""[..],
            "agents",
        ),
        (
            r#"{"sessions":[{"name":"default","running":true},{"name":"agents","running":true}]}"#,
            &b"\x1b[B\r"[..],
            "agents",
        ),
    ] {
        let result = setup_options(
            "new",
            "y\n",
            false,
            false,
            SetupOptions {
                sessions: Some(sessions),
                selection,
                ..Default::default()
            },
        );
        assert_eq!(
            result.probes, "platform\ncandidates\nsessions\n",
            "{}",
            result.output
        );
        assert_eq!(result.actions, "stop\nstart\n", "{}", result.output);
        if expected == "default" {
            assert!(
                !result.start_command.contains("--session"),
                "{}",
                result.start_command
            );
        } else {
            assert!(
                result.start_command.contains("--session agents"),
                "{}",
                result.start_command
            );
        }
        if !selection.is_empty() {
            let picker = result
                .output
                .split("Running sessions on fake-host:")
                .nth(1)
                .unwrap();
            assert!(
                picker.starts_with("\r\n"),
                "picker did not return to column zero: {picker:?}"
            );
            assert!(picker.contains("default\u{1b}[0m\r\n"), "{picker:?}");
            assert!(picker.contains("  agents\r\n"), "{picker:?}");
        }
    }
}

#[test]
fn machine_add_failed_discovery_and_cancel_never_prepare_or_save() {
    for (sessions, selection, expected) in [
        ("failed", &b""[..], "session query denied"),
        ("not json", &b""[..], "--remote-session"),
        (
            r#"{"sessions":[{"name":"agents"}]}"#,
            &b""[..],
            "--remote-session",
        ),
        (
            r#"{"sessions":[{"name":"default","running":true},{"name":"agents","running":true}]}"#,
            &b"\x1b"[..],
            "selection cancelled",
        ),
    ] {
        let result = setup_options(
            "new",
            "y\n",
            false,
            false,
            SetupOptions {
                sessions: Some(sessions),
                selection,
                ..Default::default()
            },
        );
        assert!(!result.success, "{}", result.output);
        assert!(result.actions.is_empty(), "{}", result.output);
        assert!(result.output.contains(expected), "{}", result.output);
        assert!(
            result.output.contains("machine was not saved"),
            "{}",
            result.output
        );
    }
}

#[test]
fn machine_add_skips_discovery_for_scripts_and_explicit_sessions() {
    for (noninteractive, explicit_session) in [
        (true, None),
        (true, Some("agents")),
        (false, Some("agents")),
    ] {
        let result = setup_options(
            "new",
            "y\n",
            false,
            false,
            SetupOptions {
                noninteractive,
                explicit_session,
                sessions: Some("failed"),
                ..Default::default()
            },
        );
        assert_eq!(result.probes, "platform\ncandidates\n", "{}", result.output);
        assert!(
            result.output.contains(if explicit_session.is_some() {
                "session agents"
            } else {
                "session default"
            }),
            "{}",
            result.output
        );
        if noninteractive {
            assert!(result.actions.is_empty(), "{}", result.output);
        } else {
            assert_eq!(result.actions, "stop\nstart\n", "{}", result.output);
        }
    }
}

#[test]
fn machine_add_reports_strict_host_key_failure() {
    let result = setup_with_strict_host_key_failure("new", "", false, true);
    assert!(!result.success, "{}", result.output);
    assert_eq!(result.prompts, 0, "{}", result.output);
    assert!(result.actions.is_empty(), "{}", result.output);
    assert!(
        result.output.contains("Host key verification failed"),
        "{}",
        result.output
    );
    assert!(
        result
            .output
            .contains("saved machines use strict host-key checking"),
        "{}",
        result.output
    );
    assert!(
        !result.output.contains("lost connection to server"),
        "{}",
        result.output
    );
}

#[test]
fn machine_setup_old_install_requires_one_explicit_stop_approval() {
    let result = setup("old", "y\n", false);
    assert_eq!(result.prompts, 1, "{}", result.output);
    assert!(
        result
            .output
            .contains("This stops active remote pane processes"),
        "{}",
        result.output
    );
    assert_eq!(
        result.actions, "install\nstop\nstart\n",
        "{}",
        result.output
    );
}

#[test]
fn machine_setup_old_install_enter_cancels_without_remote_changes() {
    let result = setup("old", "\n", false);
    assert!(result.actions.is_empty(), "{}", result.output);
    assert!(
        result.output.contains("machine was not saved"),
        "{}",
        result.output
    );
}

#[test]
fn machine_setup_new_install_old_server_enter_does_not_stop_or_handoff() {
    let result = setup("new", "\n", false);
    assert!(result.actions.is_empty(), "{}", result.output);
    assert!(
        result
            .output
            .contains("This stops active remote pane processes"),
        "{}",
        result.output
    );
}

#[test]
fn machine_setup_new_install_old_server_approval_stops_then_starts_without_installing() {
    let result = setup("new", "y\n", false);
    assert_eq!(result.prompts, 1, "{}", result.output);
    assert_eq!(result.actions, "stop\nstart\n", "{}", result.output);
}

#[test]
fn machine_setup_explicit_remote_handoff_remains_available_but_restart_fallback_defaults_to_no() {
    let result = setup("new", "\n", true);
    assert_eq!(result.actions, "handoff\n", "{}", result.output);
    assert!(
        result
            .output
            .contains("This stops active remote pane processes"),
        "{}",
        result.output
    );
}

// `herdr machine add` runs ssh once more for the endpoint probe, with strict host-key checking. That
// call must read the same extra ssh config files as every other call of the setup. `fake-host` is
// defined only in `$FAKE_EXTRA_CONFIG`, so the fake resolves it only when the call's `-F` config
// includes that file, and fails the way real ssh does when it cannot.
const PROBE_SSH: &str = r#"#!/bin/sh
{ printf 'ssh'; for arg do printf ' [%s]' "$arg"; done; printf '\n'; } >>"$FAKE_ROOT/ssh-calls"
cfg=; prev=; probe=no
for arg do
    if [ "$prev" = -F ]; then cfg=$arg; fi
    if [ "$arg" = 'StrictHostKeyChecking=yes' ]; then probe=yes; fi
    if [ "$arg" = -O ]; then exit 0; fi
    prev=$arg
done
if [ "$probe" = yes ]; then
    printf '%s\n' "$@" >"$FAKE_ROOT/probe-argv"
    if [ -n "$cfg" ]; then cat "$cfg" >"$FAKE_ROOT/probe-config"; fi
    if [ -n "$cfg" ] && grep -qF "$FAKE_EXTRA_CONFIG" "$cfg"; then
        echo 'ssh: probe reached fake-host through the extra config' >&2
    else
        echo 'ssh: Could not resolve hostname fake-host: Name or service not known' >&2
    fi
    exit 255
fi
exec "$FAKE_ROOT/bin/ssh-main" "$@"
"#;

struct ProbeRun {
    output: String,
    ssh_calls: String,
    probe_argv: Vec<String>,
    probe_config: Option<String>,
    extra_config: std::path::PathBuf,
}

impl ProbeRun {
    fn context(&self) -> String {
        format!(
            "output: {}\nssh calls:\n{}\nprobe config: {:?}",
            self.output, self.ssh_calls, self.probe_config
        )
    }

    fn probe_config_arg(&self) -> Option<&str> {
        self.probe_argv
            .windows(2)
            .find(|pair| pair[0] == "-F")
            .map(|pair| pair[1].as_str())
    }
}

/// Runs `herdr machine add` against the fake ssh above, with the `[remote]` section `remote_toml`
/// builds from the path of the extra ssh config file.
fn machine_add_probe(tag: &str, remote_toml: impl Fn(&std::path::Path) -> String) -> ProbeRun {
    use std::process::Stdio;

    let root = std::path::PathBuf::from(format!(
        "/var/tmp/herdr-sshprobe-{}-{tag}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let app = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("tmp")).unwrap();
    fs::create_dir_all(root.join("config").join(app)).unwrap();
    for (name, script) in [("ssh", PROBE_SSH), ("ssh-main", SSH)] {
        fs::write(root.join("bin").join(name), script).unwrap();
        fs::set_permissions(
            root.join("bin").join(name),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    let extra_config = root.join("extra_ssh_config");
    fs::write(
        &extra_config,
        "Host fake-host\n  HostName extra.example.invalid\n  Port 2222\n",
    )
    .unwrap();
    fs::write(
        root.join("config").join(app).join("config.toml"),
        format!("onboarding = false\n{}", remote_toml(&extra_config)),
    )
    .unwrap();

    let mut status_command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    support::sanitize_herdr_env_command(&mut status_command);
    let status = status_command
        .args(["status", "client", "--json"])
        .output()
        .unwrap();
    assert!(status.status.success());

    let log = fs::File::create(root.join("herdr-output")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    support::sanitize_herdr_env_command(&mut command);
    command
        .args(["machine", "add", "fake-host", "--label", "VPS"])
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .env("HOME", &root)
        .env("TMPDIR", root.join("tmp"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", &root)
        .env("FAKE_ROOT", &root)
        .env("FAKE_EXTRA_CONFIG", &extra_config)
        .env("FAKE_INSTALLED", "new")
        // Makes the fake server report the capabilities that make setup probe its endpoint.
        .env("FAKE_STRICT_HOST_KEY_FAILURE", "yes")
        .env(
            "FAKE_CLIENT_STATUS",
            String::from_utf8(status.stdout).unwrap(),
        )
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            let _ = fs::remove_dir_all(&root);
            panic!("machine add timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let read = |name: &str| fs::read_to_string(root.join(name)).ok();
    let run = ProbeRun {
        output: read("herdr-output").unwrap_or_default(),
        ssh_calls: read("ssh-calls").unwrap_or_default(),
        probe_argv: read("probe-argv")
            .map(|argv| argv.lines().map(str::to_owned).collect())
            .unwrap_or_default(),
        probe_config: read("probe-config"),
        extra_config,
    };
    let saved = root
        .join("state")
        .join(app)
        .join("client/endpoints.json")
        .exists();
    fs::remove_dir_all(&root).unwrap();
    assert!(!status.success(), "the fake probe always fails");
    assert!(
        !saved,
        "a failed probe saved the machine: {}",
        run.context()
    );
    assert!(
        run.probe_argv
            .iter()
            .any(|arg| arg == "StrictHostKeyChecking=yes"),
        "setup never reached the endpoint probe: {}",
        run.context()
    );
    run
}

#[test]
fn machine_add_probe_ssh_reads_the_configured_extra_ssh_config() {
    let run = machine_add_probe("extra", |extra| {
        format!("[remote]\nssh_config_files = [\"{}\"]\n", extra.display())
    });
    let context = run.context();

    let config_path = run
        .probe_config_arg()
        .unwrap_or_else(|| panic!("the probe's ssh has no -F: {:?}\n{context}", run.probe_argv));
    assert!(
        run.probe_config.as_deref().is_some_and(
            |config| config.contains(&format!("Include \"{}\"", run.extra_config.display()))
        ),
        "probe config {config_path} does not include the extra file\n{context}"
    );
    assert!(
        !run.output.contains("Could not resolve hostname"),
        "{context}"
    );
    assert!(
        run.output
            .contains("probe reached fake-host through the extra config"),
        "{context}"
    );
}

#[test]
fn machine_add_probe_ssh_ignores_the_extra_file_unless_configured() {
    // Default-preserving: with no extra files the probe's managed config does not include the
    // extra file, so a host defined only there does not resolve; with extras switched off the
    // probe uses plain ssh, as upstream does.
    let default = machine_add_probe("default", |_| "[remote]\n".to_owned());
    assert!(
        default
            .probe_config
            .as_deref()
            .is_some_and(|config| !config.contains(&default.extra_config.display().to_string())),
        "{}",
        default.context()
    );
    assert!(
        default.output.contains("Could not resolve hostname"),
        "{}",
        default.context()
    );

    let unmanaged = machine_add_probe("unmanaged", |extra| {
        format!(
            "[remote]\nmanage_ssh_config = false\nssh_config_files = [\"{}\"]\n",
            extra.display()
        )
    });
    assert_eq!(
        unmanaged.probe_config_arg(),
        None,
        "{}",
        unmanaged.context()
    );
}

// ---------------------------------------------------------------------------------------------
// Saved machines limited to local sessions: `machine add --local-session`, `machine scope`,
// `machine list`. No ssh and no server: they only read and write the catalog in a sandbox.
// ---------------------------------------------------------------------------------------------

const ALPHA: &str = "0123456789abcdef0123456789abcdef";
const BETA: &str = "11111111111111111111111111111111";

// The bytes the catalog writer produces for two machines nobody limited to a local session.
const SEEDED_UNSCOPED_CATALOG: &str = r#"{
  "version": 1,
  "ssh": [
    {
      "id": "0123456789abcdef0123456789abcdef",
      "label": "Alpha",
      "target": "alpha.example",
      "session": "agents",
      "enabled": true
    },
    {
      "id": "11111111111111111111111111111111",
      "label": "Beta",
      "target": "beta.example",
      "session": "main",
      "enabled": false
    }
  ]
}"#;

struct MachineSandbox {
    root: std::path::PathBuf,
    catalog: std::path::PathBuf,
}

impl MachineSandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "herdr-machine-scope-{}-{tag}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let app = if cfg!(debug_assertions) {
            "herdr-dev"
        } else {
            "herdr"
        };
        let catalog = root.join("state").join(app).join("client/endpoints.json");
        fs::create_dir_all(catalog.parent().unwrap()).unwrap();
        fs::create_dir_all(root.join("config").join(app)).unwrap();
        fs::write(
            root.join("config").join(app).join("config.toml"),
            "onboarding = false\n[remote]\nmanage_ssh_config = false\n",
        )
        .unwrap();
        fs::write(&catalog, SEEDED_UNSCOPED_CATALOG).unwrap();
        Self { root, catalog }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        support::sanitize_herdr_env_command(&mut command);
        command
            .args(args)
            // Reaching ssh would fail with exit 1 and "machine was not saved", not these codes.
            .env("PATH", self.root.join("no-executables"))
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", &self.root)
            .stdin(std::process::Stdio::null());
        for name in [
            "HERDR_ENV",
            "HERDR_SESSION",
            "HERDR_SOCKET_PATH",
            "HERDR_CLIENT_SOCKET_PATH",
            "HERDR_REMOTE_BINARY",
            "HERDR_CONFIG_PATH",
        ] {
            command.env_remove(name);
        }
        command.output().unwrap()
    }

    fn list_json(&self) -> serde_json::Value {
        let output = self.run(&["machine", "list", "--json"]);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for MachineSandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn machine_scope_limits_a_machine_to_local_sessions_and_list_shows_it() {
    let sandbox = MachineSandbox::new("scope");

    // Before: nothing is limited, and the listing says so.
    let before = sandbox.list_json();
    assert_eq!(
        before[0]["local_sessions"],
        serde_json::json!([]),
        "{before}"
    );
    assert_eq!(
        before[1]["local_sessions"],
        serde_json::json!([]),
        "{before}"
    );

    let output = sandbox.run(&[
        "machine",
        "scope",
        ALPHA,
        "--local-session",
        "default",
        "--local-session=legal",
        "--local-session",
        "default",
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let after = sandbox.list_json();
    assert_eq!(after[0]["id"], ALPHA);
    assert_eq!(
        after[0]["local_sessions"],
        serde_json::json!(["default", "legal"]),
        "{after}"
    );
    assert_eq!(after[1]["local_sessions"], serde_json::json!([]), "{after}");
    // Only the scoped machine's entry changed in the file.
    let stored = fs::read_to_string(&sandbox.catalog).unwrap();
    assert_eq!(stored.matches("local_sessions").count(), 1, "{stored}");

    // The plain listing keeps its five columns for an unscoped machine and adds a sixth only
    // for the scoped one.
    let listing = stdout(&sandbox.run(&["machine", "list"]));
    let lines = listing.lines().collect::<Vec<_>>();
    assert_eq!(
        lines[0],
        format!("{ALPHA}\tAlpha\talpha.example\tagents\tenabled\tlocal sessions: default,legal")
    );
    assert_eq!(
        lines[1],
        format!("{BETA}\tBeta\tbeta.example\tmain\tdisabled")
    );

    // Back to every local session: the file is byte-identical to the one nobody scoped.
    let output = sandbox.run(&["machine", "scope", ALPHA, "--all-local-sessions"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        fs::read_to_string(&sandbox.catalog).unwrap(),
        SEEDED_UNSCOPED_CATALOG
    );
    assert_eq!(sandbox.list_json(), before);
}

#[test]
fn machine_scope_refuses_bad_input_and_leaves_the_catalog_alone() {
    let sandbox = MachineSandbox::new("scope-refusals");
    for (args, code, needle) in [
        // An invalid session name, as the rest of the CLI judges names.
        (
            vec!["machine", "scope", ALPHA, "--local-session", "bad/name"],
            2,
            "invalid local session",
        ),
        // A machine that does not exist.
        (
            vec![
                "machine",
                "scope",
                "fedcba9876543210fedcba9876543210",
                "--local-session",
                "legal",
            ],
            1,
            "was not found",
        ),
        // Neither a scope nor --all-local-sessions, or both at once.
        (
            vec!["machine", "scope", ALPHA],
            2,
            "usage: herdr machine scope",
        ),
        (
            vec![
                "machine",
                "scope",
                ALPHA,
                "--all-local-sessions",
                "--local-session",
                "legal",
            ],
            2,
            "usage: herdr machine scope",
        ),
        // A target is not a profile id.
        (
            vec![
                "machine",
                "scope",
                "alpha.example",
                "--local-session",
                "legal",
            ],
            2,
            "error:",
        ),
    ] {
        let output = sandbox.run(&args);
        assert_eq!(output.status.code(), Some(code), "{args:?}: {output:?}");
        assert!(stderr(&output).contains(needle), "{args:?}: {output:?}");
        assert_eq!(
            fs::read_to_string(&sandbox.catalog).unwrap(),
            SEEDED_UNSCOPED_CATALOG,
            "{args:?} changed the catalog"
        );
    }
}

#[test]
fn machine_add_checks_local_sessions_before_it_connects_to_anything() {
    let sandbox = MachineSandbox::new("add-scope");
    // A bad name stops the command with a usage-class exit; ssh is never reached.
    let output = sandbox.run(&[
        "machine",
        "add",
        "fake-host",
        "--label",
        "Scoped",
        "--local-session",
        "bad/name",
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stderr(&output).contains("invalid local session"),
        "{output:?}"
    );

    // A good one, in either spelling and any order, gets as far as ssh, which this sandbox
    // does not have: "machine was not saved" (exit 1), and nothing is written.
    for args in [
        vec![
            "machine",
            "add",
            "fake-host",
            "--label",
            "Scoped",
            "--local-session",
            "default",
        ],
        vec![
            "machine",
            "add",
            "--local-session=default",
            "--local-session",
            "legal",
            "fake-host",
            "--label",
            "Scoped",
        ],
    ] {
        let output = sandbox.run(&args);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        assert!(
            stderr(&output).contains("machine was not saved"),
            "{output:?}"
        );
    }
    assert_eq!(
        fs::read_to_string(&sandbox.catalog).unwrap(),
        SEEDED_UNSCOPED_CATALOG
    );
}

#[test]
fn machine_help_documents_the_local_session_options() {
    let sandbox = MachineSandbox::new("help");
    let help = stdout(&sandbox.run(&["machine", "help"]));
    for needle in [
        "--local-session",
        "herdr machine scope",
        "--all-local-sessions",
    ] {
        assert!(help.contains(needle), "help lacks {needle}: {help}");
    }
}
