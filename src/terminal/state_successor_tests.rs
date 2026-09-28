//! A session start refused because the same agent holds another live session on the pane:
//! who sent the live session's reports, and what the refusal hands the caller to judge.

use super::*;
use crate::agent_resume::AgentSessionRef;
use crate::platform::PeerProcess;

const HOLDER: PeerProcess = PeerProcess {
    pid: 9_100_001,
    process_group: 9_100_000,
};
const SUCCESSOR: PeerProcess = PeerProcess {
    pid: 9_200_001,
    process_group: 9_200_000,
};

fn path(name: &str) -> Option<AgentSessionRef> {
    AgentSessionRef::path(format!("/tmp/herdr-successor-{name}.jsonl"))
}

fn id(name: &str) -> Option<AgentSessionRef> {
    AgentSessionRef::id(format!("session-{name}"))
}

fn start(
    terminal: &mut TerminalState,
    sender: Option<PeerProcess>,
    session_ref: Option<AgentSessionRef>,
    seq: Option<u64>,
    reason: Option<&str>,
) -> Option<TerminalStateMutation> {
    terminal.set_report_sender(sender);
    let mutation = terminal.set_agent_session_ref_for_session_start(
        "herdr:pi".into(),
        "pi".into(),
        session_ref,
        seq,
        reason.map(str::to_string),
    );
    terminal.set_report_sender(None);
    mutation
}

fn report(
    terminal: &mut TerminalState,
    sender: Option<PeerProcess>,
    session_ref: Option<AgentSessionRef>,
    seq: u64,
    state: AgentState,
) -> Option<TerminalStateMutation> {
    terminal.set_report_sender(sender);
    let mutation = terminal.set_hook_authority_with_session_ref(
        "herdr:pi".into(),
        "pi".into(),
        state,
        None,
        session_ref,
        Some(seq),
    );
    terminal.set_report_sender(None);
    mutation
}

/// Pi detected, its session started and reported by `holder`.
fn live_session(
    holder: Option<PeerProcess>,
    session: fn(&str) -> Option<AgentSessionRef>,
) -> TerminalState {
    let mut terminal = TerminalState::new(TerminalId::alloc(), "/tmp".into());
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Unknown);
    assert!(start(
        &mut terminal,
        holder,
        session("holder"),
        Some(10),
        Some("startup")
    )
    .is_some());
    assert!(report(
        &mut terminal,
        holder,
        session("holder"),
        11,
        AgentState::Idle
    )
    .is_some());
    terminal
}

fn live_session_ref(terminal: &TerminalState) -> Option<AgentSessionRef> {
    terminal
        .hook_authority
        .as_ref()
        .and_then(|authority| authority.session_ref.clone())
}

#[test]
fn an_accepted_report_names_its_sender_and_an_unnamed_one_clears_it() {
    let mut terminal = live_session(Some(HOLDER), path);
    assert_eq!(
        terminal.hook_authority.as_ref().unwrap().sender,
        Some(HOLDER)
    );

    assert!(report(&mut terminal, None, path("holder"), 12, AgentState::Working).is_some());
    assert_eq!(
        terminal.hook_authority.as_ref().unwrap().sender,
        None,
        "a report nobody can vouch for leaves the holder unknown"
    );
}

#[test]
fn a_held_report_keeps_its_sender_until_it_applies() {
    let mut terminal = TerminalState::new(TerminalId::alloc(), "/tmp".into());
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Unknown);
    // The reporter's first state report beats its session start, so it is held ...
    assert!(report(
        &mut terminal,
        Some(HOLDER),
        path("holder"),
        11,
        AgentState::Working
    )
    .is_none());
    assert!(terminal.hook_authority.is_none());
    // ... and applied by the session start, still naming the process that sent it.
    assert!(start(
        &mut terminal,
        None,
        path("holder"),
        Some(10),
        Some("startup")
    )
    .is_some());
    let authority = terminal.hook_authority.as_ref().unwrap();
    assert_eq!(authority.state, AgentState::Working);
    assert_eq!(authority.sender, Some(HOLDER));
}

#[test]
fn a_new_sessions_start_refused_while_the_old_one_is_live_names_the_holder() {
    for session in [path as fn(&str) -> Option<AgentSessionRef>, id] {
        let mut terminal = live_session(Some(HOLDER), session);

        assert!(start(
            &mut terminal,
            Some(SUCCESSOR),
            session("successor"),
            Some(20),
            Some("startup")
        )
        .is_none());

        assert_eq!(
            live_session_ref(&terminal),
            session("holder"),
            "still refused"
        );
        assert_eq!(
            terminal.take_live_session_start_refusal(),
            Some(LiveSessionStartRefusal {
                agent: Agent::Pi,
                holder: Some(HOLDER),
            })
        );
        assert_eq!(
            terminal.take_live_session_start_refusal(),
            None,
            "taken once"
        );
    }
}

#[test]
fn the_refusal_names_no_holder_when_the_live_report_had_no_sender() {
    let mut terminal = live_session(None, path);
    assert!(start(
        &mut terminal,
        Some(SUCCESSOR),
        path("successor"),
        Some(20),
        Some("startup")
    )
    .is_none());
    assert_eq!(
        terminal.take_live_session_start_refusal(),
        Some(LiveSessionStartRefusal {
            agent: Agent::Pi,
            holder: None,
        })
    );
}

#[test]
fn a_start_that_the_normal_relaunch_path_would_also_refuse_is_not_noted() {
    // Unrecognized start sources: even after an observed exit these never bind a session.
    for reason in [None, Some("reload")] {
        let mut terminal = live_session(Some(HOLDER), path);
        assert!(start(
            &mut terminal,
            Some(SUCCESSOR),
            path("successor"),
            Some(20),
            reason
        )
        .is_none());
        assert_eq!(
            terminal.take_live_session_start_refusal(),
            None,
            "{reason:?}"
        );
        assert_eq!(live_session_ref(&terminal), path("holder"));
    }
    // The same session is no takeover, and a start that may replace a session is no refusal.
    for (session, reason) in [(path("holder"), "startup"), (path("successor"), "new")] {
        let mut terminal = live_session(Some(HOLDER), path);
        start(
            &mut terminal,
            Some(SUCCESSOR),
            session.clone(),
            Some(20),
            Some(reason),
        );
        assert_eq!(
            terminal.take_live_session_start_refusal(),
            None,
            "{session:?} {reason}"
        );
    }
}
