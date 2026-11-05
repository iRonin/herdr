//! Ending the processes of a pane's session when the pane goes away.
//!
//! A pane shell leads its own session, so the shell's pid names the session for as long as any process is in it,
//! even after the shell has exited and been reaped. Everything in that session is ended in steps: SIGHUP, then
//! SIGTERM after 250 ms and SIGKILL after 500 ms to whatever still runs, and a last look after 750 ms. Each step
//! re-reads the session, so a process started since the last one is treated the same.
//!
//! A pid is only ever signalled as part of a session just read from the process table. When the shell is gone and a
//! new process has been given its pid, the session id may now name that process's own session (a new pane shell leads
//! one), so then nothing is signalled and the pane is left alone.
//!
//! All of it happens on one background thread. The thread that closes a pane (the server's own) only queues the pane
//! and never sleeps through a grace period, and shutting down many panes costs one escalation, not one per pane. A
//! server that is about to exit calls [`finish_before_exit`] first, so the escalation finishes before the process ends,
//! for at most the length of the escalation plus a margin.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::layout::PaneId;
use crate::platform::Signal;

const TERMINATE_AFTER: Duration = Duration::from_millis(250);
const KILL_AFTER: Duration = Duration::from_millis(500);
const GIVE_UP_AFTER: Duration = Duration::from_millis(750);
/// What the exiting server allows beyond the escalation itself, for a busy machine.
const EXIT_MARGIN: Duration = Duration::from_millis(1500);
/// How often the panes being ended are looked at again, so a session that is already gone is released at once
/// instead of at the end of its grace period.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Nothing sent yet: find the session and hang it up.
    Hangup,
    Terminate,
    Kill,
    GiveUp,
}

struct Pending {
    ticket: u64,
    pane_id: PaneId,
    child_pid: u32,
    /// The shell's start time when it was spawned; 0 when it was not known.
    child_start_token: u64,
    child_wait_completed: Option<Arc<AtomicBool>>,
    pids: Vec<u32>,
    hung_up_at: Instant,
    step: Step,
    last_signal: Option<Signal>,
}

/// One look at the process table, taken the first time a pane needs it in a pass of the background thread however
/// many panes do, so a server stop with a hundred panes reads the table once per step, not once per pane.
#[derive(Default)]
struct Processes {
    sessions: Option<Vec<(u32, u32)>>,
}

impl Processes {
    fn table(&mut self) -> &[(u32, u32)] {
        self.sessions
            .get_or_insert_with(crate::platform::process_sessions)
    }
}

/// What to signal for a pane, from one look at the process table.
#[derive(Debug, PartialEq, Eq)]
enum SessionTargets {
    /// These processes are in the pane's session.
    Members(Vec<u32>),
    /// A process that is not the pane's shell now has the shell's pid and leads a session of its own, so the session
    /// id is no longer the pane's to signal.
    IdTaken,
}

/// The processes in the session of the shell `child_pid` (its pid is the session id). `table` is every process with
/// its session id. `reaped` says the shell has been waited for, so whoever has its pid now (`exists`) is somebody
/// else; otherwise a process with that pid is the shell unless its start time (`start_of`) differs from
/// `child_start_token`.
fn session_targets(
    child_pid: u32,
    reaped: bool,
    child_start_token: u64,
    table: &[(u32, u32)],
    start_of: impl Fn(u32) -> Option<u64>,
    exists: impl Fn(u32) -> bool,
) -> SessionTargets {
    // The table was read a moment ago. An entry with the shell's pid is the shell itself, exited but not yet waited
    // for, when the shell has been reaped since: only a process that still has the pid now can have taken it over.
    let holder = table
        .iter()
        .find(|(pid, _)| *pid == child_pid)
        .filter(|_| !reaped || exists(child_pid));
    let holder_is_the_shell = !reaped
        && match (child_start_token, start_of(child_pid)) {
            (0, _) | (_, None) => true,
            (recorded, Some(now)) => recorded == now,
        };
    if let Some(&(_, holder_session)) = holder {
        if holder_session == child_pid && !holder_is_the_shell {
            return SessionTargets::IdTaken;
        }
    }
    let mut members: Vec<u32> = table
        .iter()
        .filter(|(pid, session)| {
            *session == child_pid && (*pid != child_pid || holder_is_the_shell)
        })
        .map(|(pid, _)| *pid)
        .collect();
    if members.is_empty() && holder_is_the_shell {
        // A pane child that leads no session of its own.
        members.push(child_pid);
    }
    SessionTargets::Members(members)
}

#[derive(Default)]
struct State {
    queue: Vec<Pending>,
    /// Queued or being worked on, as (ticket, pane, shell pid). [`wait_for_all`] waits for it to empty.
    ending: Vec<(u64, u32, u32)>,
    next_ticket: u64,
}

struct Reaper {
    state: Mutex<State>,
    work: Condvar,
    idle: Condvar,
    /// The background thread could not be started, so panes are ended on the thread that closes them instead.
    no_thread: AtomicBool,
}

fn reaper() -> &'static Reaper {
    static REAPER: OnceLock<&'static Reaper> = OnceLock::new();
    REAPER.get_or_init(|| {
        let reaper: &'static Reaper = Box::leak(Box::new(Reaper {
            state: Mutex::new(State::default()),
            work: Condvar::new(),
            idle: Condvar::new(),
            no_thread: AtomicBool::new(false),
        }));
        if let Err(err) = std::thread::Builder::new()
            .name("pane-shutdown".into())
            .spawn(move || reaper.run())
        {
            warn!(%err, "failed to start the pane shutdown thread; panes are ended on the closing thread");
            reaper.no_thread.store(true, Ordering::Release);
        }
        reaper
    })
}

/// Starts ending the session of the pane whose shell is `child_pid`, and returns at once.
pub(super) fn begin(
    pane_id: PaneId,
    child_pid: u32,
    child_start_token: u64,
    child_wait_completed: Option<Arc<AtomicBool>>,
) {
    if child_pid == 0 {
        return;
    }
    let reaper = reaper();
    let mut pending = Pending {
        ticket: 0,
        pane_id,
        child_pid,
        child_start_token,
        child_wait_completed,
        pids: Vec::new(),
        hung_up_at: Instant::now(),
        step: Step::Hangup,
        last_signal: None,
    };
    if reaper.no_thread.load(Ordering::Acquire) {
        end_on_this_thread(pending);
        return;
    }
    let mut state = reaper.state.lock().unwrap_or_else(|p| p.into_inner());
    state.next_ticket += 1;
    pending.ticket = state.next_ticket;
    state
        .ending
        .push((pending.ticket, pane_id.raw(), child_pid));
    state.queue.push(pending);
    drop(state);
    reaper.work.notify_one();
}

/// Waits until every pane shutdown begun so far has finished its last step, for at most `timeout`. Returns the
/// panes (pane id, shell pid) that are still being ended; empty when all are done.
pub(crate) fn wait_for_all(timeout: Duration) -> Vec<(u32, u32)> {
    let reaper = reaper();
    let deadline = Instant::now() + timeout;
    let mut state = reaper.state.lock().unwrap_or_else(|p| p.into_inner());
    while !state.ending.is_empty() {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        state = reaper
            .idle
            .wait_timeout(state, left)
            .unwrap_or_else(|p| p.into_inner())
            .0;
    }
    state
        .ending
        .iter()
        .map(|(_, pane, pid)| (*pane, *pid))
        .collect()
}

/// For a server that is about to exit: lets the escalation of every pane shut down so far finish, for at most its own
/// length plus a margin, and logs the panes that are still being ended at that bound.
pub(crate) fn finish_before_exit() {
    let left = wait_for_all(GIVE_UP_AFTER + EXIT_MARGIN);
    if !left.is_empty() {
        warn!(
            panes = ?left,
            "pane sessions were still being ended when the server exited"
        );
    }
}

fn end_on_this_thread(mut pending: Pending) {
    loop {
        match pending.advance(Instant::now(), &mut Processes::default()) {
            Some(next) => pending = next,
            None => return,
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

impl Reaper {
    fn run(&'static self) {
        loop {
            let batch = {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                while state.queue.is_empty() {
                    state = self.work.wait(state).unwrap_or_else(|p| p.into_inner());
                }
                std::mem::take(&mut state.queue)
            };
            let mut processes = Processes::default();
            let mut still_ending = Vec::with_capacity(batch.len());
            let mut finished = Vec::new();
            for pending in batch {
                let ticket = pending.ticket;
                match pending.advance(Instant::now(), &mut processes) {
                    Some(pending) => still_ending.push(pending),
                    None => finished.push(ticket),
                }
            }
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state
                .ending
                .retain(|(ticket, _, _)| !finished.contains(ticket));
            let queued_meanwhile = !state.queue.is_empty();
            state.queue.extend(still_ending);
            if state.ending.is_empty() {
                self.idle.notify_all();
            }
            if !queued_meanwhile && !state.queue.is_empty() {
                // Panes queued from now on are picked up at once; the rest are looked at again shortly.
                drop(
                    self.work
                        .wait_timeout(state, POLL_INTERVAL)
                        .unwrap_or_else(|p| p.into_inner()),
                );
            }
        }
    }
}

impl Pending {
    /// Takes the pane one step further. `None` when its session is gone, was left alone, or the last step is done.
    fn advance(mut self, now: Instant, processes: &mut Processes) -> Option<Self> {
        if self.step == Step::Hangup {
            self.pids = self.targets(processes)?;
            if self.pids.is_empty() {
                info!(
                    pane = self.pane_id.raw(),
                    pid = self.child_pid,
                    "pane session already gone"
                );
                return None;
            }
            crate::platform::signal_processes(&self.pids, Signal::Hangup);
            self.hung_up_at = Instant::now();
            self.last_signal = Some(Signal::Hangup);
            self.step = Step::Terminate;
            return Some(self);
        }

        let waited = now.saturating_duration_since(self.hung_up_at);
        let due = match self.step {
            Step::Terminate => (waited >= TERMINATE_AFTER).then_some(Signal::Terminate),
            Step::Kill => (waited >= KILL_AFTER).then_some(Signal::Kill),
            Step::GiveUp | Step::Hangup => None,
        };
        let give_up = self.step == Step::GiveUp && waited >= GIVE_UP_AFTER;
        let mut known = std::mem::take(&mut self.pids);
        known.retain(|pid| self.is_running(*pid));
        self.pids = known;
        if self.pids.is_empty() || due.is_some() || give_up {
            // Look at the session afresh: only processes read from it just now are ever signalled, and whatever it
            // started since the last step is treated the same.
            self.pids = self.targets(processes)?;
            if self.pids.is_empty() {
                info!(
                    pane = self.pane_id.raw(),
                    pid = self.child_pid,
                    signal = ?self.last_signal,
                    "pane session terminated"
                );
                return None;
            }
        }
        if give_up {
            warn!(
                pane = self.pane_id.raw(),
                pid = self.child_pid,
                pids = ?self.pids,
                "pane session still alive after forced shutdown"
            );
            return None;
        }
        if let Some(signal) = due {
            crate::platform::signal_processes(&self.pids, signal);
            self.last_signal = Some(signal);
            self.step = if signal == Signal::Terminate {
                Step::Kill
            } else {
                Step::GiveUp
            };
        }
        Some(self)
    }

    fn reaped(&self) -> bool {
        self.child_wait_completed
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    /// The running processes of the pane's session, or `None` (after logging it) when the session id has been taken
    /// over by another process and the pane must be left alone.
    fn targets(&self, processes: &mut Processes) -> Option<Vec<u32>> {
        let table = processes.table();
        let members = match session_targets(
            self.child_pid,
            self.reaped(),
            self.child_start_token,
            table,
            crate::platform::process_start_token,
            crate::platform::process_exists,
        ) {
            SessionTargets::Members(members) => members,
            SessionTargets::IdTaken => {
                warn!(
                    pane = self.pane_id.raw(),
                    pid = self.child_pid,
                    "the pane shell's pid now belongs to another session leader; leaving it alone"
                );
                return None;
            }
        };
        let mut pids: Vec<u32> = members
            .into_iter()
            .filter(|pid| self.is_running(*pid))
            .collect();
        pids.sort_unstable();
        pids.dedup();
        Some(pids)
    }

    fn is_running(&self, pid: u32) -> bool {
        super::process_alive_for_shutdown(pid, self.child_pid, self.reaped(), |pid| {
            crate::platform::process_exists(pid) && !crate::platform::process_is_zombie(pid)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(child_pid: u32, reaped: bool) -> Pending {
        Pending {
            ticket: 0,
            pane_id: PaneId::from_raw(1),
            child_pid,
            child_start_token: 0,
            child_wait_completed: Some(Arc::new(AtomicBool::new(reaped))),
            pids: Vec::new(),
            hung_up_at: Instant::now(),
            step: Step::Hangup,
            last_signal: None,
        }
    }

    const SHELL: u32 = 500;
    const START: u64 = 1_000_000;

    fn members(targets: SessionTargets) -> Vec<u32> {
        match targets {
            SessionTargets::Members(mut members) => {
                members.sort_unstable();
                members
            }
            SessionTargets::IdTaken => panic!("expected the session members, got IdTaken"),
        }
    }

    #[test]
    fn the_session_of_an_exited_shell_is_found_by_its_id() {
        // The shell (500) is gone; two helpers remain in its session, and an unrelated shell has its own.
        let table = [(501, SHELL), (502, SHELL), (900, 900), (901, 900)];
        let found = session_targets(SHELL, true, START, &table, |_| None, |_| false);
        assert_eq!(members(found), vec![501, 502]);
    }

    #[test]
    fn a_live_shell_and_its_session_are_all_targets() {
        let table = [(SHELL, SHELL), (501, SHELL), (900, 900)];
        let found = session_targets(SHELL, false, START, &table, |_| Some(START), |_| true);
        assert_eq!(members(found), vec![SHELL, 501]);
    }

    #[test]
    fn the_reaped_shell_itself_in_a_table_read_just_before_is_not_a_new_session_leader() {
        // The table was read while the shell was a zombie, then the shell was reaped. The entry is the shell, not
        // somebody who took its pid: nothing has the pid now, so the helpers must still be signalled.
        let table = [(SHELL, SHELL), (501, SHELL), (502, SHELL)];
        let found = session_targets(SHELL, true, START, &table, |_| None, |_| false);
        assert_eq!(members(found), vec![501, 502]);
        // The same table with a process that really has the pid now is a different matter.
        assert_eq!(
            session_targets(SHELL, true, START, &table, |_| Some(START + 99), |_| true),
            SessionTargets::IdTaken
        );
    }

    #[test]
    fn a_new_session_leader_with_the_reaped_shells_pid_is_left_alone() {
        // The shell was reaped and its pid went to a new pane shell, which leads a session of the same id.
        let table = [(SHELL, SHELL), (777, SHELL)];
        assert_eq!(
            session_targets(SHELL, true, START, &table, |_| Some(START + 99), |_| true),
            SessionTargets::IdTaken
        );
    }

    #[test]
    fn a_pid_reused_by_a_different_process_is_told_from_the_shell_by_its_start_time() {
        // Not reaped as far as we know (an imported pane), but the process with the pid started later than the shell.
        let table = [(SHELL, SHELL), (777, SHELL)];
        assert_eq!(
            session_targets(SHELL, false, START, &table, |_| Some(START + 99), |_| true),
            SessionTargets::IdTaken
        );
        // Same table, same start time: it is the shell.
        assert_eq!(
            members(session_targets(
                SHELL,
                false,
                START,
                &table,
                |_| Some(START),
                |_| true
            )),
            vec![SHELL, 777]
        );
    }

    #[test]
    fn a_start_time_that_cannot_be_read_is_not_a_reason_to_leave_the_pane_alone() {
        let table = [(SHELL, SHELL), (501, SHELL)];
        for (recorded, now) in [(0, Some(START)), (START, None)] {
            assert_eq!(
                members(session_targets(
                    SHELL,
                    false,
                    recorded,
                    &table,
                    |_| now,
                    |_| true
                )),
                vec![SHELL, 501]
            );
        }
    }

    #[test]
    fn a_process_that_took_the_pid_without_leading_a_session_is_never_signalled() {
        // The pid went to a process that is not a session leader; the old session's helpers are still found.
        let table = [(SHELL, 100), (501, SHELL)];
        let found = session_targets(SHELL, true, START, &table, |_| Some(START + 99), |_| true);
        assert_eq!(members(found), vec![501]);
        let found = session_targets(SHELL, false, START, &table, |_| Some(START + 99), |_| true);
        assert_eq!(members(found), vec![501]);
        // With no helpers left there is nothing to signal, not even the process that holds the pid.
        let table = [(SHELL, 100)];
        assert_eq!(
            members(session_targets(
                SHELL,
                false,
                START,
                &table,
                |_| Some(START + 99),
                |_| true
            )),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn a_reaped_shells_pid_is_never_running_even_if_another_process_has_it_now() {
        // This test process stands in for a process that was given the pid of a shell that is long gone.
        let someone_else = std::process::id();
        assert!(pending(someone_else, false).is_running(someone_else));
        assert!(!pending(someone_else, true).is_running(someone_else));
    }

    #[test]
    fn a_zombie_does_not_count_as_running() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        // Exited, not yet waited for: a zombie.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !crate::platform::process_is_zombie(pid) {
            assert!(Instant::now() < deadline, "the child never became a zombie");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(crate::platform::process_exists(pid));
        assert!(!pending(pid, false).is_running(pid));
        child.wait().unwrap();
    }

    #[test]
    fn waiting_for_nothing_returns_at_once_and_a_wait_is_bounded() {
        let started = Instant::now();
        assert!(wait_for_all(Duration::from_secs(30)).is_empty());
        assert!(started.elapsed() < Duration::from_secs(30));
    }
}
