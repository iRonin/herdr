//! `process_command_line` reads another process's arguments, for log lines. Each test uses a real process,
//! so a function that answered for the caller, or for a fixed name, fails here.

use super::process_command_line;
use std::os::unix::process::CommandExt as _;

#[test]
fn process_command_line_names_another_process_by_its_arguments() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .expect("spawn sleep");
    // Right after spawn the child can still be inside exec: on Linux its argument area reads as empty until the new
    // program's arguments are in place, so wait for the first answer instead of asking once. A process that never
    // answers still fails the assertion below, after the deadline.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut command_line = process_command_line(child.id());
    while command_line.is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
        command_line = process_command_line(child.id());
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(command_line.as_deref(), Some("sleep 30"));
}

#[test]
fn process_command_line_is_none_for_a_process_that_does_not_exist() {
    // Best effort by contract: a sender that is already gone must not fail the log line that asks about it.
    assert_eq!(process_command_line(0x7fff_fffe), None);
}
