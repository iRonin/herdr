//! What closing a pane does to the processes of its session (PA-2529).
//!
//! Every test runs a real pane (a PTY and a real shell script) whose shell starts two workers that ignore SIGHUP and
//! SIGTERM, so only SIGKILL ends them: one in the shell's own process group and one in a group of its own, which is
//! what `cmd &` gets in an interactive shell with job control. Both stay in the shell's session, like the helpers
//! the owner's prompt plugin starts. A faithful stand-in is checked first, so none of the tests can pass for want of
//! the processes it is about.

use super::*;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(60);

const PROBE_SCRIPT: &str = r#"D="$1"; MODE="$2"
( trap '' HUP TERM; exec /bin/sleep 3600 ) &
echo $! > "$D/same.pid.tmp"; mv "$D/same.pid.tmp" "$D/same.pid"
set -m
( trap '' HUP TERM; exec /bin/sleep 3601 ) &
echo $! > "$D/own.pid.tmp"; mv "$D/own.pid.tmp" "$D/own.pid"
set +m
echo $$ > "$D/shell.pid.tmp"; mv "$D/shell.pid.tmp" "$D/shell.pid"
case "$MODE" in
  exit) exit 0 ;;
  *) exec /bin/sleep 3602 ;;
esac
"#;

struct Probe {
    dir: PathBuf,
    shell: u32,
    same: u32,
    own: u32,
}

impl Probe {
    fn pids(&self) -> [u32; 3] {
        [self.shell, self.same, self.own]
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // Only the pids this test started, and only if they are still running.
        for pid in self.pids() {
            if pid_running(pid) {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Alive and not a zombie: a killed worker whose parent never reaps it (no init in a container) is gone for our purposes.
fn pid_running(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
    {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat
                .rfind(')')
                .and_then(|close| stat.get(close + 2..))
                .and_then(|rest| rest.chars().next())
                .is_some_and(|state| state != 'Z' && state != 'X'),
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("run ps");
        let stat = String::from_utf8_lossy(&output.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }
}

fn read_pid(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_until_gone(pids: &[u32], timeout: Duration) -> bool {
    wait_until(timeout, || pids.iter().all(|pid| !pid_running(*pid)))
}

fn survivors(pids: &[u32]) -> Vec<u32> {
    pids.iter()
        .copied()
        .filter(|pid| pid_running(*pid))
        .collect()
}

/// Starts a pane whose shell runs the probe script. `mode` is `stay` (the shell stays up) or `exit` (it exits as
/// soon as the workers run, like a user typing `exit`).
async fn spawn_probe(mode: &str) -> (PaneRuntime, Probe, mpsc::Receiver<AppEvent>) {
    let dir = std::env::temp_dir().join(format!(
        "herdr-pane-shutdown-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("probe.sh"), PROBE_SCRIPT).unwrap();
    let command = format!(
        "exec /bin/sh '{dir}/probe.sh' '{dir}' {mode}",
        dir = dir.display()
    );
    let (events, rx) = mpsc::channel(64);
    let runtime = PaneRuntime::spawn_shell_command(
        PaneId::from_raw(42),
        24,
        80,
        dir.clone(),
        &command,
        &PaneLaunchEnv::default(),
        AgentDetection::Disabled,
        0,
        crate::terminal_theme::TerminalTheme::default(),
        None,
        events,
        Arc::new(Notify::new()),
        Arc::new(RenderSignal::new()),
    )
    .unwrap();

    let mut ready = None;
    assert!(
        wait_until(READY_TIMEOUT, || {
            let pids =
                ["shell", "same", "own"].map(|name| read_pid(&dir.join(format!("{name}.pid"))));
            if let [Some(shell), Some(same), Some(own)] = pids {
                ready = Some((shell, same, own));
                true
            } else {
                false
            }
        }),
        "the probe shell did not start its workers"
    );
    let (shell, same, own) = ready.unwrap();
    let probe = Probe {
        dir,
        shell,
        same,
        own,
    };

    // The stand-in must be faithful, or the tests below could pass for want of what they are about.
    let session = |pid: u32| unsafe { libc::getsid(pid as libc::pid_t) };
    let group = |pid: u32| unsafe { libc::getpgid(pid as libc::pid_t) };
    assert_eq!(
        runtime.child_pid(),
        Some(probe.shell),
        "pane child is the probe shell"
    );
    assert_eq!(
        session(probe.same),
        probe.shell as libc::pid_t,
        "worker `same` is in the shell's session"
    );
    assert_eq!(
        group(probe.same),
        probe.shell as libc::pid_t,
        "worker `same` is in the shell's group"
    );
    assert_eq!(
        session(probe.own),
        probe.shell as libc::pid_t,
        "worker `own` is in the shell's session"
    );
    assert_ne!(
        group(probe.own),
        probe.shell as libc::pid_t,
        "worker `own` must have a process group of its own"
    );
    (runtime, probe, rx)
}

async fn wait_for_pane_died(rx: &mut mpsc::Receiver<AppEvent>) {
    tokio::time::timeout(READY_TIMEOUT, async {
        while let Some(event) = rx.recv().await {
            if matches!(event, AppEvent::PaneDied { .. }) {
                return;
            }
        }
        panic!("event channel closed before PaneDied");
    })
    .await
    .expect("the pane shell should have exited");
}

/// A pane closed while its shell is alive ends the shell and everything else in its session, including a worker
/// that ignores SIGHUP and SIGTERM and one in its own process group.
#[tokio::test]
async fn closing_a_pane_ends_its_whole_session() {
    let (runtime, probe, _rx) = spawn_probe("stay").await;

    runtime.shutdown();

    assert!(
        wait_until_gone(&probe.pids(), Duration::from_secs(10)),
        "still running after the pane was closed: {:?}",
        survivors(&probe.pids())
    );
}

/// The shell ends first (the user typed `exit`), the server removes the pane and shuts its runtime down. The shell
/// is already reaped then, so the session cannot be found through it; the rest of the session must still be ended.
#[tokio::test]
async fn closing_a_pane_whose_shell_already_exited_ends_the_rest_of_its_session() {
    let (runtime, probe, mut rx) = spawn_probe("exit").await;
    wait_for_pane_died(&mut rx).await;
    assert!(
        wait_until_gone(&[probe.shell], Duration::from_secs(10)),
        "the shell should have exited"
    );
    assert_eq!(
        survivors(&[probe.same, probe.own]),
        vec![probe.same, probe.own],
        "both workers outlive their shell before the pane is closed"
    );

    runtime.shutdown();

    assert!(
        wait_until_gone(&[probe.same, probe.own], Duration::from_secs(10)),
        "workers left behind by a pane whose shell exited first: {:?}",
        survivors(&[probe.same, probe.own])
    );
}

/// The same, through the other way a runtime ends: it is dropped (a server shutdown drops every runtime).
#[tokio::test]
async fn dropping_the_runtime_of_a_pane_whose_shell_already_exited_ends_the_rest_of_its_session() {
    let (runtime, probe, mut rx) = spawn_probe("exit").await;
    wait_for_pane_died(&mut rx).await;

    drop(runtime);

    assert!(
        wait_until_gone(&[probe.same, probe.own], Duration::from_secs(10)),
        "workers left behind by a dropped runtime of a pane whose shell exited first: {:?}",
        survivors(&[probe.same, probe.own])
    );
}

/// Closing a pane must not hold the thread that closes it for the grace periods. A worker that ignores SIGHUP and
/// SIGTERM lives until the SIGKILL, at least half a second after the hangup, so it is still running when the close
/// returns if the escalation happens in the background.
#[tokio::test]
async fn closing_a_pane_does_not_wait_out_the_grace_periods() {
    for attempt in 1..=3 {
        let (runtime, probe, _rx) = spawn_probe("stay").await;
        let started = Instant::now();
        runtime.shutdown();
        let returned = Instant::now();
        let stubborn_worker_running = pid_running(probe.same);
        let check_delay = returned.elapsed();
        if stubborn_worker_running {
            assert!(
                wait_until_gone(&probe.pids(), Duration::from_secs(10)),
                "still running after the grace periods: {:?}",
                survivors(&probe.pids())
            );
            return;
        }
        // Only a stall of this thread between the return and the check (an overloaded machine) earns another try.
        assert!(
            check_delay > Duration::from_millis(150) && attempt < 3,
            "the close returned only after the stubborn worker was killed ({:?}): the thread that closes a pane was held that long",
            returned - started
        );
    }
}

/// Shutting down many panes takes one escalation, not one per pane: a server stop with 140 panes must not take
/// 140 times the grace periods.
#[tokio::test]
async fn shutting_down_many_panes_shares_one_grace_period() {
    const PANES: usize = 8;
    let mut runtimes = Vec::new();
    let mut probes = Vec::new();
    let mut receivers = Vec::new();
    for _ in 0..PANES {
        let (runtime, probe, rx) = spawn_probe("stay").await;
        runtimes.push(runtime);
        probes.push(probe);
        receivers.push(rx);
    }
    let pids: Vec<u32> = probes.iter().flat_map(Probe::pids).collect();

    let started = Instant::now();
    for runtime in runtimes {
        runtime.shutdown();
    }
    assert!(
        wait_until_gone(&pids, Duration::from_secs(30)),
        "still running: {:?}",
        survivors(&pids)
    );
    let took = started.elapsed();

    // One after another each pane waits out the hangup and terminate periods (500 ms) before the kill: 4 s for 8.
    assert!(
        took < Duration::from_millis(3000),
        "{PANES} panes took {took:?}; shut down one after another they would need over 4 s"
    );
}

/// A live handoff moves the panes to the new server: the old server must not end their processes when it lets go.
#[tokio::test]
async fn a_live_handoff_does_not_end_the_pane_processes() {
    let (runtime, probe, _rx) = spawn_probe("stay").await;
    // The old server pauses the pane's reader, the new server receives a duplicate of the PTY master, and only then
    // does the old server let go of the pane.
    runtime
        .pause_handoff_reader(Duration::from_secs(2))
        .expect("pause the PTY reader");
    let duplicate = runtime
        .duplicate_handoff_fd()
        .expect("duplicate the PTY master");
    let _new_server_master = unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicate) };

    runtime.preserve_for_handoff();

    // Longer than the hangup, terminate and kill periods together.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        survivors(&probe.pids()),
        probe.pids().to_vec(),
        "the shell and workers must outlive the old server's runtime after a handoff"
    );
}

/// A server that exits right after shutting its panes down must not leave the escalation unfinished: once the exit
/// wait returns, the workers that only SIGKILL ends are gone, with no further waiting.
#[tokio::test]
async fn the_server_exit_wait_returns_only_after_the_whole_session_is_gone() {
    let (runtime, probe, _rx) = spawn_probe("stay").await;
    runtime.shutdown();

    finish_pane_shutdowns_before_exit();

    assert_eq!(
        survivors(&probe.pids()),
        Vec::<u32>::new(),
        "the exit wait returned while these were still running"
    );
}

/// The exit wait is bounded: asked for a short wait while a worker holds the pane for half a second, it returns on
/// time and names the pane that is still being ended, instead of holding the exit open.
#[tokio::test]
async fn waiting_for_the_escalation_is_bounded_and_names_what_is_still_ending() {
    for attempt in 1..=3 {
        let (runtime, probe, _rx) = spawn_probe("stay").await;
        runtime.shutdown();
        let started = Instant::now();
        let still_ending = super::shutdown::wait_for_all(Duration::from_millis(50));
        let waited = started.elapsed();
        finish_pane_shutdowns_before_exit();
        if !still_ending.is_empty() {
            assert!(
                waited < Duration::from_millis(400),
                "a 50 ms wait took {waited:?}"
            );
            assert!(
                still_ending.contains(&(42, probe.shell)),
                "pane 42 and shell {} should be named, got {still_ending:?}",
                probe.shell
            );
            return;
        }
        // Only a stall of this thread past the kill step (an overloaded machine) earns another try.
        assert!(
            waited >= Duration::from_millis(450) && attempt < 3,
            "the wait returned in {waited:?} with nothing left, but the worker lives until the kill step"
        );
    }
}
