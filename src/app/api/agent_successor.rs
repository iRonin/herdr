//! A reporter's session start for a pane whose agent exited and was replaced inside one
//! detection tick.
//!
//! The detector polls each pane every 300 ms. When an agent exits and the next one starts in
//! the same pane between two polls, the detector sees the same agent kind in a new process group
//! and never records the exit, so the dead agent's hook authority stays live. The new agent's
//! `startup` session start then looks like one agent taking over another's live session, which
//! the lifecycle state refuses, and the pane keeps the dead agent's last status for good.
//!
//! When that refusal happens, this module asks the operating system what the detector missed.
//! The session start is taken as the successor's only if the holder of the live session (the
//! sender of its last accepted report) is gone, process and process group alike, and the
//! session start comes from the agent in front of the pane: the foreground job's leader,
//! detected as the same agent, or that leader's direct child (a launcher's agent process runs
//! there). herdr then records what the detector would have recorded between the two processes,
//! the holder's exit and then the successor's detection, and routes the session start again,
//! down the path a relaunch at normal speed takes. Anything not proven keeps the refusal.

use tracing::{debug, info};

use crate::app::actions::PaneStateUpdate;
use crate::app::App;
use crate::detect::{Agent, AgentState};
use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::platform::{ForegroundJob, PeerProcess};

/// The process table as it stands when a session start is judged.
pub(crate) trait ProcessFacts {
    fn process_exists(&self, pid: u32) -> bool;
    fn process_group_exists(&self, process_group: u32) -> bool;
    fn parent_of(&self, pid: u32) -> Option<u32>;
    fn foreground_job(&self, shell_pid: Option<u32>) -> Option<ForegroundJob>;
}

struct SystemProcesses;

impl ProcessFacts for SystemProcesses {
    fn process_exists(&self, pid: u32) -> bool {
        crate::platform::process_exists(pid)
    }

    fn process_group_exists(&self, process_group: u32) -> bool {
        crate::platform::process_group_exists(process_group)
    }

    fn parent_of(&self, pid: u32) -> Option<u32> {
        crate::platform::process_parent_id(pid)
    }

    fn foreground_job(&self, shell_pid: Option<u32>) -> Option<ForegroundJob> {
        crate::detect::foreground_job(shell_pid?)
    }
}

/// Why a refused session start is not taken as the successor's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotSuccessor {
    /// The request carried no sender: no peer credentials on this platform or connection.
    NoSender,
    /// The live session's last accepted report carried no sender.
    HolderUnknown,
    /// The holder's process or its process group still exists. A stopped (^Z) or
    /// backgrounded holder is alive, and so is one whose PID or group number was reused.
    HolderAlive,
    /// The sender is not in the pane's foreground job.
    NotInFront,
    /// The foreground job's leader is not detected as the live session's agent.
    LeaderNotAgent,
    /// The sender is below the leader's direct child, such as a helper of the agent's.
    BelowAgent,
}

pub(crate) fn judge_successor(
    agent: Agent,
    holder: Option<PeerProcess>,
    sender: Option<PeerProcess>,
    shell_pid: Option<u32>,
    facts: &impl ProcessFacts,
) -> Result<(), NotSuccessor> {
    let sender = sender.ok_or(NotSuccessor::NoSender)?;
    let holder = holder.ok_or(NotSuccessor::HolderUnknown)?;
    if facts.process_exists(holder.pid) || facts.process_group_exists(holder.process_group) {
        return Err(NotSuccessor::HolderAlive);
    }
    let job = facts
        .foreground_job(shell_pid)
        .ok_or(NotSuccessor::NotInFront)?;
    if job.process_group_id != sender.process_group
        || !job
            .processes
            .iter()
            .any(|process| process.pid == sender.pid)
    {
        return Err(NotSuccessor::NotInFront);
    }
    if !leader_is_agent(&job, agent) {
        return Err(NotSuccessor::LeaderNotAgent);
    }
    // The agent process itself, or its direct child: behind a launcher, the agent runs one level
    // below the job's leader. Anything deeper (a helper the agent runs) is refused. Residual:
    // where the agent itself leads the job, its own children are direct children too, so a
    // headless instance it spawns would pass if it reported; no known reporter does.
    let leader = job.process_group_id;
    if sender.pid != leader && facts.parent_of(sender.pid) != Some(leader) {
        return Err(NotSuccessor::BelowAgent);
    }
    Ok(())
}

/// The detector's own first question about a foreground job: is its leader the agent?
fn leader_is_agent(job: &ForegroundJob, agent: Agent) -> bool {
    let Some(leader) = job
        .processes
        .iter()
        .find(|process| process.pid == job.process_group_id)
    else {
        return false;
    };
    let leader_job = ForegroundJob {
        process_group_id: job.process_group_id,
        processes: vec![leader.clone()],
    };
    crate::platform::process_agent_hint(leader.pid) == Some(agent)
        || crate::detect::identify_agent_in_job(&leader_job)
            .is_some_and(|(found, _)| found == agent)
}

/// The detector never records an exit and the next process's detection at one instant, and a
/// detection at the exit's own instant is dropped as older than the exit.
fn instant_after(earlier: std::time::Instant) -> std::time::Instant {
    loop {
        let now = std::time::Instant::now();
        if now > earlier {
            return now;
        }
        std::hint::spin_loop();
    }
}

impl App {
    /// Applies a reporter's state report with its sender recorded on the pane's terminal, so an
    /// accepted report names the process that holds the session.
    pub(super) fn handle_reported_agent_state(&mut self, event: AppEvent) {
        let pane_id = match &event {
            AppEvent::HookStateReported { pane_id, .. } => *pane_id,
            _ => {
                self.handle_internal_event(event);
                return;
            }
        };
        let sender = self.api_request_sender;
        self.set_pane_report_sender(pane_id, sender);
        self.handle_internal_event(event);
        self.set_pane_report_sender(pane_id, None);
    }

    pub(super) fn handle_reported_agent_session(&mut self, event: AppEvent) {
        let _ = self.handle_reported_agent_session_with(event, &SystemProcesses);
    }

    pub(crate) fn handle_reported_agent_session_with(
        &mut self,
        event: AppEvent,
        facts: &impl ProcessFacts,
    ) -> Vec<PaneStateUpdate> {
        let AppEvent::AgentSessionReported {
            pane_id,
            source,
            agent_label,
            seq,
            session_ref,
            session_start_source,
        } = event
        else {
            return self.handle_internal_event_with_pane_updates(event);
        };
        let session_start = || AppEvent::AgentSessionReported {
            pane_id,
            source: source.clone(),
            agent_label: agent_label.clone(),
            seq,
            session_ref: session_ref.clone(),
            session_start_source: session_start_source.clone(),
        };
        let sender = self.api_request_sender;
        let _ = self.take_pane_live_session_start_refusal(pane_id);
        self.set_pane_report_sender(pane_id, sender);
        let mut updates = self.handle_internal_event_with_pane_updates(session_start());
        if let Some(refusal) = self.take_pane_live_session_start_refusal(pane_id) {
            let holder = refusal.holder;
            match judge_successor(
                refusal.agent,
                holder,
                sender,
                self.pane_shell_pid(pane_id),
                facts,
            ) {
                Ok(()) => {
                    info!(
                        pane = pane_id.raw(),
                        holder = ?holder,
                        sender = ?sender,
                        "session start accepted as the successor of an agent that exited unobserved"
                    );
                    updates.extend(self.replay_unobserved_relaunch(
                        pane_id,
                        refusal.agent,
                        session_start(),
                    ));
                }
                Err(reason) => debug!(
                    pane = pane_id.raw(),
                    holder = ?holder,
                    sender = ?sender,
                    ?reason,
                    "session start for another session refused while the live one is held"
                ),
            }
        }
        self.set_pane_report_sender(pane_id, None);
        updates
    }

    /// What the detector would have published had it looked between the two processes: the
    /// holder's exit (so notifications and wait-agent callers see it finish), then the successor's
    /// detection; then the session start, which now takes the path of a relaunch at normal speed.
    fn replay_unobserved_relaunch(
        &mut self,
        pane_id: PaneId,
        agent: Agent,
        session_start: AppEvent,
    ) -> Vec<PaneStateUpdate> {
        let exited_at = std::time::Instant::now();
        let mut updates = self.handle_internal_event_with_pane_updates(AppEvent::StateChanged {
            pane_id,
            agent: Some(agent),
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at: exited_at,
        });
        updates.extend(self.handle_internal_event_with_pane_updates(
            AppEvent::AgentProcessDetected {
                pane_id,
                agent,
                observed_at: instant_after(exited_at),
            },
        ));
        updates.extend(self.handle_internal_event_with_pane_updates(session_start));
        updates
    }

    fn pane_shell_pid(&self, pane_id: PaneId) -> Option<u32> {
        let (ws_idx, _) = self.find_pane(pane_id)?;
        self.state
            .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)?
            .child_pid()
    }

    fn set_pane_report_sender(&mut self, pane_id: PaneId, sender: Option<PeerProcess>) {
        let Some(terminal_id) = self
            .find_pane(pane_id)
            .map(|(_, pane)| pane.attached_terminal_id.clone())
        else {
            return;
        };
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.set_report_sender(sender);
        }
    }

    fn take_pane_live_session_start_refusal(
        &mut self,
        pane_id: PaneId,
    ) -> Option<crate::terminal::state::LiveSessionStartRefusal> {
        let terminal_id = self
            .find_pane(pane_id)
            .map(|(_, pane)| pane.attached_terminal_id.clone())?;
        self.state
            .terminals
            .get_mut(&terminal_id)?
            .take_live_session_start_refusal()
    }
}
