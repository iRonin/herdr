pub mod support;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::Command;

/// Removes its directory on drop - on success AND on a failing assertion - so
/// a test run leaves nothing behind in `$TMPDIR`.
struct SandboxDir(std::path::PathBuf);

impl Drop for SandboxDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn version_identifies_branded_fork() {
    let mut herdr_command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    support::sanitize_herdr_env_command(&mut herdr_command);
    let output = herdr_command
        .arg("--version")
        .output()
        .expect("herdr --version should run");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("herdr {} (iRonin fork)\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn update_refuses_on_fork_build_and_leaves_the_binary_untouched() {
    // `herdr update` must refuse on the fork build before contacting the
    // update feed, any running server, or the binary on disk: the upstream
    // feed publishes stock Herdr, so a completed update would replace the
    // fork and drop every fork feature.
    //
    // The refusal is exercised on a COPY of the binary so a missing guard
    // cannot touch a real installation, and inside a full sandbox so a
    // missing guard cannot reach anything real either:
    //   * the child env is cleared (no HERDR_* variables - the gate clears
    //     them too, so safety must never depend on them) and HOME/XDG_*
    //     point into the sandbox, so every server socket the updater can
    //     probe resolves inside the sandbox;
    //   * a fake `curl` first on PATH records each network attempt to a log
    //     and fails, so an unguarded run dies at the manifest fetch instead
    //     of downloading a release.
    // A regression therefore fails the assertions below loudly, having
    // touched nothing outside the sandbox. The sandbox removes itself on
    // drop - on success and on a failing assertion alike - so a run leaves
    // nothing in `$TMPDIR`.
    let sandbox =
        std::env::temp_dir().join(format!("herdr-branding-update-{}", std::process::id()));
    let _cleanup = SandboxDir(sandbox.clone());
    let home = sandbox.join("home");
    let config = sandbox.join("config");
    let state = sandbox.join("state");
    let runtime = sandbox.join("run");
    let tmp = sandbox.join("tmp");
    let bin = sandbox.join("bin");
    for dir in [&home, &config, &state, &runtime, &tmp, &bin] {
        std::fs::create_dir_all(dir).expect("create sandbox dir");
    }

    let curl_log = sandbox.join("curl.log");
    let curl = bin.join("curl");
    std::fs::write(
        &curl,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexit 22\n",
            curl_log.display()
        ),
    )
    .expect("write fake curl");
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755))
        .expect("make fake curl executable");

    let binary = sandbox.join("herdr");
    std::fs::copy(env!("CARGO_BIN_EXE_herdr"), &binary).expect("copy herdr into the sandbox");

    let before = std::fs::read(&binary).expect("read binary before update");
    let inode_before = std::fs::metadata(&binary).expect("stat binary").ino();

    for args in [vec!["update"], vec!["update", "--handoff"]] {
        let output = Command::new(&binary)
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("TMPDIR", &tmp)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .args(&args)
            .output()
            .unwrap_or_else(|error| panic!("{args:?} should run: {error}"));

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{args:?} must exit non-zero on the fork build; got {:?} (stderr: {stderr})",
            output.status.code()
        );
        assert!(
            stderr.contains("iRonin fork"),
            "{args:?} stderr must name the fork; got: {stderr}"
        );
        assert!(
            stderr.contains("would replace it with the upstream Herdr release"),
            "{args:?} stderr must say what updating would do; got: {stderr}"
        );
        assert!(
            !stderr.contains("channel for updates"),
            "{args:?} must refuse before the update flow starts; got: {stderr}"
        );
    }

    // No network attempt may happen: the fake curl was never invoked.
    assert!(
        !curl_log.exists(),
        "the refusal must fire before the update feed is contacted; curl saw: {}",
        std::fs::read_to_string(&curl_log).unwrap_or_default()
    );

    // The refusal must not have touched the binary: same inode (nothing was
    // renamed over it) and identical bytes (nothing rewrote it in place).
    let after = std::fs::read(&binary).expect("read binary after update");
    let inode_after = std::fs::metadata(&binary).expect("stat binary").ino();
    assert_eq!(
        inode_before, inode_after,
        "the binary inode must not change"
    );
    assert_eq!(before, after, "the binary bytes must not change");
}

#[test]
fn remote_attach_refuses_the_upstream_release_on_a_fork_build_and_fetches_nothing() {
    // Remote attach from a fork build must never download the upstream
    // release for the remote host: the release feed publishes stock Herdr, so
    // the remote would silently run without the fork's features. The remote
    // instead needs a fork build for its platform (built from source), passed
    // with HERDR_REMOTE_BINARY - and no error may advise `herdr update`, which
    // the fork refuses. Same discipline as the update refusal test above:
    // cleared env, HOME/XDG in a temp sandbox, and fake `curl` and `ssh` first
    // on PATH that log every invocation, so a missing guard cannot reach the
    // network or a real host. The fake `ssh` answers only the probes remote
    // attach makes before the refusal point (platform detection and remote
    // binary discovery) and fails everything else.
    let sandbox =
        std::env::temp_dir().join(format!("herdr-branding-remote-{}", std::process::id()));
    let _cleanup = SandboxDir(sandbox.clone());
    let home = sandbox.join("home");
    let config = sandbox.join("config");
    let state = sandbox.join("state");
    let runtime = sandbox.join("run");
    let tmp = sandbox.join("tmp");
    let bin = sandbox.join("bin");
    for dir in [&home, &config, &state, &runtime, &tmp, &bin] {
        std::fs::create_dir_all(dir).expect("create sandbox dir");
    }

    let curl_log = sandbox.join("curl.log");
    let curl = bin.join("curl");
    std::fs::write(
        &curl,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexit 22\n",
            curl_log.display()
        ),
    )
    .expect("write fake curl");
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755))
        .expect("make fake curl executable");

    let ssh_log = sandbox.join("ssh.log");
    let ssh = bin.join("ssh");
    std::fs::write(
        &ssh,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> {log}
input=$(cat 2>/dev/null || true)
case "$* $input" in
  *"uname -s"*)
    printf '\nherdr-remote-output-ready:1\nLinux\nx86_64\n'
    exit 0 ;;
  *"command -v herdr"*)
    exit 1 ;;
  *emit*)
    printf '\nherdr-remote-output-ready:1\n'
    exit 0 ;;
  *)
    exit 1 ;;
esac
"#,
            log = ssh_log.display()
        ),
    )
    .expect("write fake ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755))
        .expect("make fake ssh executable");

    let output = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("TMPDIR", &tmp)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args(["--remote", "remote-fixture"])
        .output()
        .expect("herdr --remote should run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "remote attach must refuse on the fork build; got {:?} (stderr: {stderr})",
        output.status.code()
    );
    assert!(
        !curl_log.exists(),
        "the refusal must fire before any release fetch; curl saw: {}",
        std::fs::read_to_string(&curl_log).unwrap_or_default()
    );
    assert!(
        ssh_log.exists(),
        "the fixture must reach the remote-install decision (the fake ssh saw no probes at all); stderr: {stderr}"
    );
    assert!(
        stderr.contains("iRonin fork"),
        "stderr must name the fork; got: {stderr}"
    );
    assert!(
        stderr.contains("HERDR_REMOTE_BINARY"),
        "stderr must say how to pass a fork build for the remote; got: {stderr}"
    );
    assert!(
        stderr.contains("built from source"),
        "stderr must say where the remote build comes from; got: {stderr}"
    );
    assert!(
        !stderr.contains("herdr update"),
        "no error on a fork build may advise `herdr update`; got: {stderr}"
    );
}

/// Sandbox + fake `ssh`/`curl` for the remote-adoption runs: the fake remote
/// reports Linux x86_64 with one installed herdr (at a non-managed path) whose
/// client status passes the endpoint requirement, so attach ADOPTS it. The
/// `--version` reply is the variant under test (written to
/// `bin/version_reply.sh`, which must print the
/// `herdr-remote-output-ready:1` marker every remote reply carries);
/// everything else answers only what the adoption
/// path needs, and fails the later server-status probe so the run stops right
/// after the adoption with no bridge or client process. The sandbox removes
/// itself on drop, on success and on failure alike.
struct RemoteAdoptionFixture {
    _cleanup: SandboxDir,
    ssh_log: std::path::PathBuf,
    curl_log: std::path::PathBuf,
    output: std::process::Output,
}

impl RemoteAdoptionFixture {
    fn run(name: &str, version_reply_script: &str) -> Self {
        Self::run_with_env(name, version_reply_script, &[])
    }

    fn run_with_env(
        name: &str,
        version_reply_script: &str,
        extra_env: &[(&str, &std::ffi::OsStr)],
    ) -> Self {
        let sandbox = std::env::temp_dir().join(format!(
            "herdr-branding-adopt-{name}-{}",
            std::process::id()
        ));
        let home = sandbox.join("home");
        let config = sandbox.join("config");
        let state = sandbox.join("state");
        let runtime = sandbox.join("run");
        let tmp = sandbox.join("tmp");
        let bin = sandbox.join("bin");
        for dir in [&home, &config, &state, &runtime, &tmp, &bin] {
            std::fs::create_dir_all(dir).expect("create sandbox dir");
        }

        let curl_log = sandbox.join("curl.log");
        let curl = bin.join("curl");
        std::fs::write(
            &curl,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexit 22\n",
                curl_log.display()
            ),
        )
        .expect("write fake curl");
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755))
            .expect("make fake curl executable");

        let version_reply = bin.join("version_reply.sh");
        std::fs::write(&version_reply, version_reply_script).expect("write version reply");
        std::fs::set_permissions(&version_reply, std::fs::Permissions::from_mode(0o755))
            .expect("make version reply executable");

        let ssh_log = sandbox.join("ssh.log");
        let ssh = bin.join("ssh");
        std::fs::write(
            &ssh,
            format!(
                r#"#!/bin/sh
input=$(cat 2>/dev/null || true)
printf 'ARGS %s | STDIN %s\n' "$*" "$input" >> {log}
case "$* $input" in
  *"uname -s"*)
    printf '\nherdr-remote-output-ready:1\nLinux\nx86_64\n'
    exit 0 ;;
  *--version*)
    exec /bin/sh {reply} ;;
  *"status client --json"*)
    printf '\nherdr-remote-output-ready:1\n%s\n' '{{"version":"0.9.1","protocol":22,"endpoint_protocol_generation":1}}'
    exit 0 ;;
  *"command -v herdr"*)
    printf '\nherdr-remote-output-ready:1\n/home/fixture/.local/bin/herdr\n'
    exit 0 ;;
  *emit*)
    printf '\nherdr-remote-output-ready:1\n'
    exit 0 ;;
  *)
    exit 1 ;;
esac
"#,
                log = ssh_log.display(),
                reply = version_reply.display()
            ),
        )
        .expect("write fake ssh");
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755))
            .expect("make fake ssh executable");

        let cleanup = SandboxDir(sandbox.clone());

        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("TMPDIR", &tmp)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let output = command
            .args(["--remote", "remote-fixture"])
            .output()
            .expect("herdr --remote should run");

        Self {
            _cleanup: cleanup,
            ssh_log,
            curl_log,
            output,
        }
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    fn ssh_log(&self) -> String {
        std::fs::read_to_string(&self.ssh_log).unwrap_or_default()
    }

    fn version_command_count(&self) -> usize {
        // The remote command travels on the fake ssh's stdin (attach runs it
        // through `/bin/sh -s`), so the log records stdin too.
        self.ssh_log()
            .lines()
            .filter(|line| line.contains("--version"))
            .count()
    }
}

#[test]
fn remote_attach_warns_when_it_adopts_a_stock_remote_herdr() {
    // A fork build adopting a stock upstream herdr on the remote must say so:
    // a stock remote runs without any fork feature, and it is exactly what
    // older fork clients installed through the release download. The notice
    // must name the remote's version and the remedy the code really supports -
    // HERDR_REMOTE_BINARY forces an install over the managed remote binary -
    // and the version check must cost at most one extra remote command.
    let fixture = RemoteAdoptionFixture::run(
        "stock",
        "#!/bin/sh\nprintf '\\nherdr-remote-output-ready:1\\nherdr 0.9.1\\n'\nexit 0\n",
    );
    let stderr = fixture.stderr();

    assert!(
        stderr.contains("upstream Herdr 0.9.1"),
        "the notice must name the remote's stock version; got: {stderr}"
    );
    assert!(
        stderr.contains("features are missing"),
        "the notice must say the fork's features are missing there; got: {stderr}"
    );
    assert!(
        stderr.contains("built from source"),
        "the notice must say where the remote build comes from; got: {stderr}"
    );
    assert!(
        stderr.contains("HERDR_REMOTE_BINARY"),
        "the notice must name the supported remedy; got: {stderr}"
    );
    assert!(
        !stderr.contains("herdr update"),
        "no fork-build message may advise `herdr update`; got: {stderr}"
    );
    assert!(
        !fixture.curl_log.exists(),
        "adoption must not fetch a release; curl saw: {}",
        std::fs::read_to_string(&fixture.curl_log).unwrap_or_default()
    );
    assert_eq!(
        fixture.version_command_count(),
        1,
        "the stock check must cost exactly one remote command; ssh log:\n{}",
        fixture.ssh_log()
    );
}

#[test]
fn remote_attach_stays_quiet_when_the_remote_herdr_carries_the_fork_banner() {
    // A remote already running a fork build needs no notice: it carries the
    // banner in its `--version` output.
    let fixture = RemoteAdoptionFixture::run(
        "banner",
        "#!/bin/sh\nprintf '\\nherdr-remote-output-ready:1\\nherdr 0.9.1 (iRonin fork)\\n'\nexit 0\n",
    );
    let stderr = fixture.stderr();

    assert_eq!(
        fixture.version_command_count(),
        1,
        "the check must still run (positive control); ssh log:\n{}",
        fixture.ssh_log()
    );
    assert!(
        !stderr.contains("upstream Herdr"),
        "a banner-carrying remote must stay quiet; got: {stderr}"
    );
}

#[test]
fn remote_attach_with_override_reaches_the_install_flow_instead_of_the_fork_refusal() {
    // HERDR_REMOTE_BINARY is the documented remedy and must bypass the fork
    // refusal before the install prompts: the given binary is what the user
    // wants on the remote, fork or not. With a non-tty stdin the run then
    // stops at the install approval - which is the proof it got past the
    // refusal. The env is passed to the spawned process, never set_var.
    let fixture = RemoteAdoptionFixture::run_with_env(
        "override",
        "#!/bin/sh\nexit 1\n",
        &[(
            "HERDR_REMOTE_BINARY",
            std::path::Path::new(env!("CARGO_BIN_EXE_herdr")).as_os_str(),
        )],
    );
    let stderr = fixture.stderr();

    assert!(
        !stderr.contains("iRonin fork"),
        "the override must bypass the fork refusal; got: {stderr}"
    );
    assert!(
        !stderr.contains("get the fork's own release"),
        "the override must bypass the fork refusal; got: {stderr}"
    );
    assert!(
        stderr.contains("could not inspect the running remote herdr server"),
        "the run must reach the install approval after the refusal point; got: {stderr}"
    );
    assert!(
        !fixture.curl_log.exists(),
        "no release fetch may happen; curl saw: {}",
        std::fs::read_to_string(&fixture.curl_log).unwrap_or_default()
    );
}

#[test]
fn remote_attach_stays_quiet_when_the_remote_version_check_fails() {
    // An unclear answer is not evidence: if `--version` fails, print nothing
    // rather than guess that the remote is stock.
    let fixture = RemoteAdoptionFixture::run("nofail", "#!/bin/sh\nexit 1\n");
    let stderr = fixture.stderr();

    assert_eq!(
        fixture.version_command_count(),
        1,
        "the check must still run (positive control); ssh log:\n{}",
        fixture.ssh_log()
    );
    assert!(
        !stderr.contains("upstream Herdr"),
        "a failed version check must stay quiet; got: {stderr}"
    );
}
