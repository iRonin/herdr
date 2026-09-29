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
//!
//! It also refuses a report for another session that comes from a process outside the live
//! holder's group while the holder still runs. Holding such a report would freeze the holder's
//! pane and let the other session's start take it over.

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

/// Whether a report for another session would displace a live agent: it comes from a process
/// outside the holder's process group while the holder's process or group still exists. The same
/// process or its group moving to another session (`/new`, `/resume`, `/fork`, or a helper
/// script, which shares the agent's group), an unknown sender, and an unknown or gone holder all
/// keep today's handling. A reused PID or group number makes a gone holder look alive, which
/// fails safe for the holder.
pub(crate) fn report_would_displace_live_holder(
    holder: Option<PeerProcess>,
    sender: Option<PeerProcess>,
    facts: &impl ProcessFacts,
) -> bool {
    let (Some(holder), Some(sender)) = (holder, sender) else {
        return false;
    };
    sender.pid != holder.pid
        && sender.process_group != holder.process_group
        && (facts.process_exists(holder.pid) || facts.process_group_exists(holder.process_group))
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
        let _ = self.handle_reported_agent_state_with(event, &SystemProcesses);
    }

    /// Also refuses a report for another session that would displace a live holder: see
    /// `report_would_displace_live_holder`.
    pub(crate) fn handle_reported_agent_state_with(
        &mut self,
        event: AppEvent,
        facts: &impl ProcessFacts,
    ) -> Vec<PaneStateUpdate> {
        let (pane_id, live_holder) = match &event {
            AppEvent::HookStateReported {
                pane_id,
                source,
                agent_label,
                session_ref,
                ..
            } => (
                *pane_id,
                self.pane_live_other_session_holder(
                    *pane_id,
                    source,
                    agent_label,
                    session_ref.as_ref(),
                ),
            ),
            _ => return self.handle_internal_event_with_pane_updates(event),
        };
        let sender = self.api_request_sender;
        let refuse = live_holder
            .is_some_and(|holder| report_would_displace_live_holder(holder, sender, facts));
        if refuse {
            debug!(
                pane = pane_id.raw(),
                holder = ?live_holder.flatten(),
                sender = ?sender,
                "report for another session refused while the live session's holder runs"
            );
        }
        self.set_pane_report_sender(pane_id, sender);
        self.set_pane_refuse_other_session_report(pane_id, refuse);
        let updates = self.handle_internal_event_with_pane_updates(event);
        self.set_pane_refuse_other_session_report(pane_id, false);
        self.set_pane_report_sender(pane_id, None);
        updates
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

    fn pane_live_other_session_holder(
        &self,
        pane_id: PaneId,
        source: &str,
        agent_label: &str,
        session_ref: Option<&crate::agent_resume::AgentSessionRef>,
    ) -> Option<Option<PeerProcess>> {
        let (_, pane) = self.find_pane(pane_id)?;
        self.state
            .terminals
            .get(&pane.attached_terminal_id)?
            .live_other_session_holder(source, agent_label, session_ref)
    }

    fn set_pane_refuse_other_session_report(&mut self, pane_id: PaneId, refuse: bool) {
        let Some(terminal_id) = self
            .find_pane(pane_id)
            .map(|(_, pane)| pane.attached_terminal_id.clone())
        else {
            return;
        };
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.set_refuse_other_session_report(refuse);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::{AgentSessionRef, PersistedAgentSession};
    use crate::app::Mode;
    use crate::config::Config;
    use crate::platform::ForegroundProcess;
    use crate::workspace::Workspace;
    use std::collections::{HashMap, HashSet};
    use std::time::Instant;

    // Above every pid_max (macOS 99998, Linux 2^22), so no real process can answer for them.
    const HOLDER: PeerProcess = PeerProcess {
        pid: 9_100_001,
        process_group: 9_100_000,
    };
    const LEADER: u32 = 9_200_000;
    const LEADER_CHILD: PeerProcess = PeerProcess {
        pid: 9_200_001,
        process_group: LEADER,
    };
    const OUTSIDER: PeerProcess = PeerProcess {
        pid: 9_300_001,
        process_group: 9_300_000,
    };
    /// Another process in the holder's group, such as a hook script the agent runs.
    const GROUP_HELPER: PeerProcess = PeerProcess {
        pid: 9_100_002,
        process_group: HOLDER.process_group,
    };
    const GRANDCHILD: PeerProcess = PeerProcess {
        pid: 9_200_002,
        process_group: LEADER,
    };

    /// A process table: who is alive, which groups have members, who is whose parent, and the
    /// pane's foreground job.
    #[derive(Default)]
    struct Table {
        alive: HashSet<u32>,
        groups: HashSet<u32>,
        parents: HashMap<u32, u32>,
        front: Option<ForegroundJob>,
    }

    impl ProcessFacts for Table {
        fn process_exists(&self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }
        fn process_group_exists(&self, process_group: u32) -> bool {
            self.groups.contains(&process_group)
        }
        fn parent_of(&self, pid: u32) -> Option<u32> {
            self.parents.get(&pid).copied()
        }
        fn foreground_job(&self, _shell_pid: Option<u32>) -> Option<ForegroundJob> {
            self.front.clone()
        }
    }

    /// The real process table for liveness, `Table` for everything else: if the liveness check
    /// failed to see a live process, every other condition would accept the successor.
    #[cfg(unix)]
    struct RealLiveness(Table);

    #[cfg(unix)]
    impl ProcessFacts for RealLiveness {
        fn process_exists(&self, pid: u32) -> bool {
            SystemProcesses.process_exists(pid)
        }
        fn process_group_exists(&self, process_group: u32) -> bool {
            SystemProcesses.process_group_exists(process_group)
        }
        fn parent_of(&self, pid: u32) -> Option<u32> {
            self.0.parent_of(pid)
        }
        fn foreground_job(&self, shell_pid: Option<u32>) -> Option<ForegroundJob> {
            self.0.foreground_job(shell_pid)
        }
    }

    #[test]
    fn successor_tables_accept_when_every_condition_holds() {
        let facts = successor_in_front("pi");
        assert_eq!(
            judge_successor(Agent::Pi, Some(HOLDER), Some(LEADER_CHILD), None, &facts),
            Ok(())
        );
        let leader = PeerProcess {
            pid: LEADER,
            process_group: LEADER,
        };
        assert_eq!(
            judge_successor(Agent::Pi, Some(HOLDER), Some(leader), None, &facts),
            Ok(())
        );
    }

    fn process(pid: u32, argv0: &str) -> ForegroundProcess {
        ForegroundProcess {
            pid,
            name: argv0.into(),
            argv0: Some(argv0.into()),
            argv: None,
            cmdline: None,
        }
    }

    /// The holder is gone; in front: the successor's leader (detected as `leader_argv0`), its
    /// direct child, and that child's own child.
    fn successor_in_front(leader_argv0: &str) -> Table {
        Table {
            parents: HashMap::from([
                (LEADER_CHILD.pid, LEADER),
                (GRANDCHILD.pid, LEADER_CHILD.pid),
            ]),
            front: Some(ForegroundJob {
                process_group_id: LEADER,
                processes: vec![
                    process(LEADER, leader_argv0),
                    process(LEADER_CHILD.pid, "node"),
                    process(GRANDCHILD.pid, "node"),
                ],
            }),
            ..Table::default()
        }
    }

    fn app_with_pane() -> (App, PaneId) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("agent")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        (app, pane_id)
    }

    fn session(name: &str) -> Option<AgentSessionRef> {
        AgentSessionRef::path(format!("/tmp/herdr-successor-{name}.jsonl"))
    }

    fn session_start(pane_id: PaneId, name: &str, seq: u64, reason: Option<&str>) -> AppEvent {
        AppEvent::AgentSessionReported {
            pane_id,
            source: "herdr:pi".into(),
            agent_label: "pi".into(),
            seq: Some(seq),
            session_ref: session(name),
            session_start_source: reason.map(str::to_string),
        }
    }

    fn report(pane_id: PaneId, name: &str, seq: u64, state: AgentState) -> AppEvent {
        AppEvent::HookStateReported {
            pane_id,
            source: "herdr:pi".into(),
            agent_label: "pi".into(),
            state,
            message: None,
            seq: Some(seq),
            session_ref: session(name),
        }
    }

    fn send_state(app: &mut App, sender: Option<PeerProcess>, event: AppEvent) {
        app.api_request_sender = sender;
        app.handle_reported_agent_state(event);
        app.api_request_sender = None;
    }

    fn send_session_start(
        app: &mut App,
        sender: Option<PeerProcess>,
        event: AppEvent,
        facts: &impl ProcessFacts,
    ) -> Vec<PaneStateUpdate> {
        app.api_request_sender = sender;
        let updates = app.handle_reported_agent_session_with(event, facts);
        app.api_request_sender = None;
        updates
    }

    /// The holder's session is established: detected, session start, then a report. Its last
    /// accepted report came from `holder`.
    fn holder_established(app: &mut App, pane_id: PaneId, holder: Option<PeerProcess>) {
        app.handle_internal_event(AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Pi,
            observed_at: Instant::now(),
        });
        send_session_start(
            app,
            holder,
            session_start(pane_id, "holder", 10, Some("startup")),
            &Table::default(),
        );
        send_state(
            app,
            holder,
            report(pane_id, "holder", 11, AgentState::Working),
        );
        send_state(app, holder, report(pane_id, "holder", 12, AgentState::Idle));
    }

    #[derive(Debug, PartialEq)]
    struct PaneView {
        detected_agent: Option<Agent>,
        state: AgentState,
        authority: Option<(String, AgentState, Option<AgentSessionRef>)>,
        persisted: Option<PersistedAgentSession>,
    }

    fn pane_view(app: &App, pane_id: PaneId) -> PaneView {
        let (_, pane) = app.find_pane(pane_id).unwrap();
        let terminal = &app.state.terminals[&pane.attached_terminal_id];
        PaneView {
            detected_agent: terminal.detected_agent,
            state: terminal.state,
            authority: terminal.hook_authority.as_ref().map(|authority| {
                (
                    authority.source.clone(),
                    authority.state,
                    authority.session_ref.clone(),
                )
            }),
            persisted: terminal.persisted_agent_session.clone(),
        }
    }

    fn holds_session(view: &PaneView, name: &str, state: AgentState) -> bool {
        view.state == state
            && view
                .authority
                .as_ref()
                .is_some_and(|(_, reported, session_ref)| {
                    *reported == state && *session_ref == session(name)
                })
    }

    /// The fast relaunch: the successor's session start for a new session, with the holder's
    /// authority still live because the detector never saw the exit.
    fn assert_refused(
        holder: Option<PeerProcess>,
        sender: Option<PeerProcess>,
        facts: &impl ProcessFacts,
        why: NotSuccessor,
    ) {
        // Every other condition holds in `facts`, so only `why` can refuse it.
        assert_eq!(
            judge_successor(Agent::Pi, holder, sender, None, facts),
            Err(why)
        );
        let (mut app, pane_id) = app_with_pane();
        holder_established(&mut app, pane_id, holder);
        let before = pane_view(&app, pane_id);
        assert!(
            holds_session(&before, "holder", AgentState::Idle),
            "{why:?}: setup"
        );

        let updates = send_session_start(
            &mut app,
            sender,
            session_start(pane_id, "successor", 20, Some("startup")),
            facts,
        );

        assert!(updates.is_empty(), "{why:?}: nothing may be published");
        assert_eq!(
            pane_view(&app, pane_id),
            before,
            "{why:?}: the pane is unchanged"
        );
    }

    #[tokio::test]
    async fn successor_is_refused_without_a_sender() {
        assert_refused(
            Some(HOLDER),
            None,
            &successor_in_front("pi"),
            NotSuccessor::NoSender,
        );
    }

    #[tokio::test]
    async fn successor_is_refused_when_the_holder_is_unknown() {
        assert_refused(
            None,
            Some(LEADER_CHILD),
            &successor_in_front("pi"),
            NotSuccessor::HolderUnknown,
        );
    }

    #[tokio::test]
    async fn successor_is_refused_while_the_holder_process_lives() {
        let mut table = successor_in_front("pi");
        table.alive.insert(HOLDER.pid);
        table.groups.insert(HOLDER.process_group);
        assert_refused(
            Some(HOLDER),
            Some(LEADER_CHILD),
            &table,
            NotSuccessor::HolderAlive,
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successor_is_refused_when_the_holder_pid_now_names_another_process() {
        // Real processes: the holder's group is gone, its PID belongs to a live process
        // (this test). A reused PID looks alive, so the refusal stands.
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let empty_group = gone.id();
        gone.wait().unwrap();
        let holder = PeerProcess {
            pid: std::process::id(),
            process_group: empty_group,
        };
        assert_refused(
            Some(holder),
            Some(LEADER_CHILD),
            &RealLiveness(successor_in_front("pi")),
            NotSuccessor::HolderAlive,
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successor_is_refused_when_the_holder_group_now_has_members() {
        // Real processes: the holder's PID is gone, its group number has live members
        // (this test's group). A reused group looks alive, so the refusal stands.
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = gone.id();
        gone.wait().unwrap();
        let holder = PeerProcess {
            pid: dead_pid,
            process_group: unsafe { libc::getpgid(0) } as u32,
        };
        assert_refused(
            Some(holder),
            Some(LEADER_CHILD),
            &RealLiveness(successor_in_front("pi")),
            NotSuccessor::HolderAlive,
        );
    }

    #[tokio::test]
    async fn successor_is_refused_when_the_sender_is_not_in_front() {
        let outside = PeerProcess {
            pid: 9_300_001,
            process_group: 9_300_000,
        };
        assert_refused(
            Some(HOLDER),
            Some(outside),
            &successor_in_front("pi"),
            NotSuccessor::NotInFront,
        );
    }

    #[tokio::test]
    async fn successor_is_refused_when_the_sender_is_below_the_agents_child() {
        assert_refused(
            Some(HOLDER),
            Some(GRANDCHILD),
            &successor_in_front("pi"),
            NotSuccessor::BelowAgent,
        );
    }

    #[tokio::test]
    async fn successor_is_refused_when_the_process_in_front_is_not_the_agent() {
        assert_refused(
            Some(HOLDER),
            Some(LEADER_CHILD),
            &successor_in_front("zsh"),
            NotSuccessor::LeaderNotAgent,
        );
    }

    /// Mirror of `pi_non_replacement_reports_preserve_full_lifecycle_authority` with senders on
    /// both sides and the holder alive: whatever the start source, the live session is kept and
    /// the new session's report does not apply.
    #[tokio::test]
    async fn live_holder_keeps_its_session_against_a_session_start_from_the_front() {
        for reason in [None, Some("reload"), Some("startup")] {
            let (mut app, pane_id) = app_with_pane();
            holder_established(&mut app, pane_id, Some(HOLDER));
            let mut table = successor_in_front("pi");
            table.alive.insert(HOLDER.pid);
            table.groups.insert(HOLDER.process_group);

            let updates = send_session_start(
                &mut app,
                Some(LEADER_CHILD),
                session_start(pane_id, "unexpected", 20, reason),
                &table,
            );
            send_state(
                &mut app,
                Some(LEADER_CHILD),
                report(pane_id, "unexpected", 21, AgentState::Working),
            );

            assert!(updates.is_empty(), "{reason:?}");
            assert!(
                holds_session(&pane_view(&app, pane_id), "holder", AgentState::Idle),
                "{reason:?} must not replace the live Pi session"
            );
        }
    }

    fn assert_successor_takes_over(sender: PeerProcess) {
        let (mut app, pane_id) = app_with_pane();
        holder_established(&mut app, pane_id, Some(HOLDER));

        let updates = send_session_start(
            &mut app,
            Some(sender),
            session_start(pane_id, "successor", 20, Some("startup")),
            &successor_in_front("pi"),
        );

        let released: Vec<_> = updates
            .iter()
            .filter(|update| update.agent_released)
            .collect();
        assert_eq!(
            released.len(),
            1,
            "the dead holder's exit is published once"
        );
        assert_eq!(released[0].state, AgentState::Idle);
        assert_eq!(
            pane_view(&app, pane_id).persisted,
            Some(PersistedAgentSession {
                source: "herdr:pi".into(),
                agent: "pi".into(),
                session_ref: session("successor").unwrap(),
            }),
            "the successor's session is bound"
        );

        send_state(
            &mut app,
            Some(sender),
            report(pane_id, "successor", 21, AgentState::Working),
        );
        assert!(
            holds_session(&pane_view(&app, pane_id), "successor", AgentState::Working),
            "the successor's report shows at once"
        );
        send_state(
            &mut app,
            Some(HOLDER),
            report(pane_id, "holder", 13, AgentState::Idle),
        );
        assert!(
            holds_session(&pane_view(&app, pane_id), "successor", AgentState::Working),
            "a late report for the old session is rejected"
        );
    }

    #[tokio::test]
    async fn successor_from_the_agents_child_takes_the_pane_once_its_holder_is_gone() {
        assert_successor_takes_over(LEADER_CHILD);
    }

    #[tokio::test]
    async fn successor_from_the_foreground_leader_itself_takes_the_pane() {
        assert_successor_takes_over(PeerProcess {
            pid: LEADER,
            process_group: LEADER,
        });
    }

    /// The replay must leave the pane exactly as a relaunch at normal speed does, where the
    /// detector sees the exit, then the new process, then its session start.
    #[tokio::test]
    async fn unobserved_relaunch_ends_like_a_relaunch_at_normal_speed() {
        let (mut observed, observed_pane) = app_with_pane();
        holder_established(&mut observed, observed_pane, Some(HOLDER));
        let exited_at = Instant::now();
        let mut observed_updates =
            observed.handle_internal_event_with_pane_updates(AppEvent::StateChanged {
                pane_id: observed_pane,
                agent: Some(Agent::Pi),
                state: AgentState::Idle,
                visible_blocker: false,
                visible_working: false,
                process_exited: true,
                observed_at: exited_at,
            });
        observed_updates.extend(observed.handle_internal_event_with_pane_updates(
            AppEvent::AgentProcessDetected {
                pane_id: observed_pane,
                agent: Agent::Pi,
                observed_at: instant_after(exited_at),
            },
        ));
        observed_updates.extend(send_session_start(
            &mut observed,
            Some(LEADER_CHILD),
            session_start(observed_pane, "successor", 20, Some("startup")),
            &Table::default(),
        ));

        let (mut unobserved, unobserved_pane) = app_with_pane();
        holder_established(&mut unobserved, unobserved_pane, Some(HOLDER));
        let unobserved_updates = send_session_start(
            &mut unobserved,
            Some(LEADER_CHILD),
            session_start(unobserved_pane, "successor", 20, Some("startup")),
            &successor_in_front("pi"),
        );

        let published = |updates: &[PaneStateUpdate]| -> Vec<_> {
            updates
                .iter()
                .map(|update| {
                    (
                        update.previous_state,
                        update.state,
                        update.agent_label.clone(),
                        update.agent_released,
                        update.suppress_completion,
                    )
                })
                .collect()
        };
        assert_eq!(published(&unobserved_updates), published(&observed_updates));
        assert_eq!(
            pane_view(&unobserved, unobserved_pane),
            pane_view(&observed, observed_pane)
        );
        let probes = [
            (Some(LEADER_CHILD), "successor", 21, AgentState::Working),
            (Some(HOLDER), "holder", 13, AgentState::Idle),
            (Some(LEADER_CHILD), "successor", 22, AgentState::Blocked),
            (Some(LEADER_CHILD), "successor", 23, AgentState::Idle),
        ];
        for (sender, name, seq, state) in probes {
            send_state(
                &mut observed,
                sender,
                report(observed_pane, name, seq, state),
            );
            send_state(
                &mut unobserved,
                sender,
                report(unobserved_pane, name, seq, state),
            );
            assert_eq!(
                pane_view(&unobserved, unobserved_pane),
                pane_view(&observed, observed_pane),
                "after {name} reports {state:?}"
            );
        }
    }

    fn holder_alive() -> Table {
        let mut table = successor_in_front("pi");
        table.alive.insert(HOLDER.pid);
        table.groups.insert(HOLDER.process_group);
        table
    }

    fn send_state_with(
        app: &mut App,
        sender: Option<PeerProcess>,
        event: AppEvent,
        facts: &impl ProcessFacts,
    ) {
        app.api_request_sender = sender;
        app.handle_reported_agent_state_with(event, facts);
        app.api_request_sender = None;
    }

    #[test]
    fn report_guard_refuses_only_another_process_while_the_holder_lives() {
        let alive = holder_alive();
        let check = |holder, sender, facts: &Table| {
            report_would_displace_live_holder(holder, sender, facts)
        };
        assert!(check(Some(HOLDER), Some(OUTSIDER), &alive));
        assert!(!check(None, Some(OUTSIDER), &alive), "holder unknown");
        assert!(!check(Some(HOLDER), None, &alive), "sender unknown");
        assert!(
            !check(Some(HOLDER), Some(HOLDER), &alive),
            "the holder itself"
        );
        assert!(
            !check(Some(HOLDER), Some(GROUP_HELPER), &alive),
            "its group"
        );
        assert!(
            !check(Some(HOLDER), Some(OUTSIDER), &Table::default()),
            "holder gone"
        );
        let mut group_only = Table::default();
        group_only.groups.insert(HOLDER.process_group);
        assert!(
            check(Some(HOLDER), Some(OUTSIDER), &group_only),
            "a live group"
        );
    }

    #[cfg(unix)]
    #[test]
    fn report_guard_counts_a_reused_holder_pid_or_group_as_alive() {
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let dead = gone.id();
        gone.wait().unwrap();
        let facts = RealLiveness(Table::default());
        let pid_reused = PeerProcess {
            pid: std::process::id(),
            process_group: dead,
        };
        let group_reused = PeerProcess {
            pid: dead,
            process_group: unsafe { libc::getpgid(0) } as u32,
        };
        assert!(report_would_displace_live_holder(
            Some(pid_reused),
            Some(OUTSIDER),
            &facts
        ));
        assert!(report_would_displace_live_holder(
            Some(group_reused),
            Some(OUTSIDER),
            &facts
        ));
    }

    #[tokio::test]
    async fn stray_report_from_another_process_leaves_a_live_holder_untouched() {
        let (mut app, pane_id) = app_with_pane();
        holder_established(&mut app, pane_id, Some(HOLDER));
        let before = pane_view(&app, pane_id);
        let stray = report(pane_id, "stray", 30, AgentState::Working);
        send_state_with(&mut app, Some(OUTSIDER), stray, &holder_alive());
        assert_eq!(
            pane_view(&app, pane_id),
            before,
            "the stray report is refused"
        );

        let next = report(pane_id, "holder", 31, AgentState::Working);
        send_state_with(&mut app, Some(HOLDER), next, &holder_alive());
        assert!(
            holds_session(&pane_view(&app, pane_id), "holder", AgentState::Working),
            "the holder's next report still applies: nothing was recorded"
        );
    }

    #[tokio::test]
    async fn a_live_holder_is_not_taken_over_by_a_second_agent_reporting_first() {
        let (mut app, pane_id) = app_with_pane();
        holder_established(&mut app, pane_id, Some(HOLDER));
        let facts = holder_alive();
        let first = report(pane_id, "second", 31, AgentState::Working);
        send_state_with(&mut app, Some(LEADER_CHILD), first, &facts);
        let start = session_start(pane_id, "second", 30, Some("startup"));
        let updates = send_session_start(&mut app, Some(LEADER_CHILD), start, &facts);

        assert!(updates.is_empty());
        assert!(
            holds_session(&pane_view(&app, pane_id), "holder", AgentState::Idle),
            "the live holder keeps its session"
        );
        let next = report(pane_id, "holder", 32, AgentState::Blocked);
        send_state_with(&mut app, Some(HOLDER), next, &facts);
        assert!(holds_session(
            &pane_view(&app, pane_id),
            "holder",
            AgentState::Blocked
        ));
    }

    #[tokio::test]
    async fn the_holder_switching_its_own_session_report_first_binds_the_new_session() {
        for (sender, who) in [
            (HOLDER, "the holder itself"),
            (GROUP_HELPER, "a helper in its group"),
        ] {
            let (mut app, pane_id) = app_with_pane();
            holder_established(&mut app, pane_id, Some(HOLDER));
            let facts = holder_alive();
            let first = report(pane_id, "next", 31, AgentState::Working);
            send_state_with(&mut app, Some(sender), first, &facts);
            let start = session_start(pane_id, "next", 30, Some("new"));
            send_session_start(&mut app, Some(sender), start, &facts);
            assert!(
                holds_session(&pane_view(&app, pane_id), "next", AgentState::Working),
                "{who}: the new session is bound with its first report"
            );
        }
    }

    #[tokio::test]
    async fn reports_the_guard_cannot_judge_take_todays_path() {
        let cases = [
            (Some(LEADER_CHILD), successor_in_front("pi"), "holder gone"),
            (None, holder_alive(), "sender unknown"),
        ];
        for (sender, facts, who) in cases {
            let (mut app, pane_id) = app_with_pane();
            holder_established(&mut app, pane_id, Some(HOLDER));
            let first = report(pane_id, "second", 31, AgentState::Working);
            send_state_with(&mut app, sender, first, &facts);
            let start = session_start(pane_id, "second", 30, Some("startup"));
            send_session_start(&mut app, sender, start, &facts);
            assert!(
                holds_session(&pane_view(&app, pane_id), "second", AgentState::Working),
                "{who}: the report-first successor still binds, as today"
            );
        }
    }
}
