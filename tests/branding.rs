use std::process::Command;

#[test]
fn version_identifies_branded_fork() {
    let output = Command::new(env!("CARGO_BIN_EXE_herdr"))
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
