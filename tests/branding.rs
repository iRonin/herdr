pub mod support;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::Command;

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
    // touched nothing outside the sandbox.
    let sandbox =
        std::env::temp_dir().join(format!("herdr-branding-update-{}", std::process::id()));
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

    let _ = std::fs::remove_dir_all(&sandbox);
}
