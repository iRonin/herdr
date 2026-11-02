//! Pushes each pane's read state to API subscribers.
//!
//! `pane.read_state` reports a pane's `seen` flag and its displayed
//! `agent_status`. Both change in many places: agent state transitions, viewing
//! a tab, forwarding input to a blocked pane, an `auto_read` token, the `/compact`
//! display timeout. This module does not watch those places. It compares each
//! pane's current value with the last one it observed and emits one
//! `pane.read_state_changed` event for every pane whose value differs, so a new
//! place that changes the value, here or upstream, is covered without edits.
//!
//! The comparison runs once per server loop turn, after every source of change
//! for that turn has run, and before a `pane.read_state` request is answered.
//! Several changes inside one turn are reported as their net result: a pane that
//! goes unseen and seen again within one turn produces no event.

use std::collections::HashSet;
use std::time::Instant;

use crate::api::schema::{AgentStatus, EventData, EventEnvelope, EventKind, PaneReadState};
use crate::layout::PaneId;

use super::api_helpers::pane_agent_status;
use super::App;

/// The part of a pane's read state that can change: the ids in
/// [`PaneReadState`] are looked up again when an event is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObservedReadState {
    seen: bool,
    agent_status: AgentStatus,
}

impl App {
    /// The value `pane.read_state` returns for a pane, and the body of
    /// `pane.read_state_changed`. `None` when the pane has no terminal or no public id.
    pub(crate) fn pane_read_state_at(
        &self,
        ws_idx: usize,
        pane_id: PaneId,
        now: Instant,
    ) -> Option<PaneReadState> {
        let pane = self.state.workspaces.get(ws_idx)?.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
        Some(PaneReadState {
            pane_id: self.public_pane_id(ws_idx, pane_id)?,
            workspace_id: self.public_workspace_id(ws_idx),
            seen: pane.seen,
            agent_status: pane_agent_status(terminal.display_state_at(now), pane.seen),
        })
    }

    /// Emits `pane.read_state_changed` for every pane whose read state differs from
    /// the last one observed, and records the new value. A pane seen for the first
    /// time is recorded without an event: there is no earlier value to differ from.
    pub(crate) fn sync_read_state_events(&mut self) {
        let now = Instant::now();
        let mut live_panes = 0_usize;
        let mut observations: Vec<(usize, PaneId, ObservedReadState, bool)> = Vec::new();
        for (ws_idx, workspace) in self.state.workspaces.iter().enumerate() {
            for tab in &workspace.tabs {
                for (&pane_id, pane) in &tab.panes {
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    live_panes += 1;
                    let current = ObservedReadState {
                        seen: pane.seen,
                        agent_status: pane_agent_status(terminal.display_state_at(now), pane.seen),
                    };
                    match self.read_state_observed.get(&pane_id) {
                        Some(previous) if *previous == current => {}
                        Some(_) => observations.push((ws_idx, pane_id, current, true)),
                        None => observations.push((ws_idx, pane_id, current, false)),
                    }
                }
            }
        }

        for (ws_idx, pane_id, current, announce) in observations {
            if announce {
                // Built from the same inputs as `current`, so the event body is the value
                // `pane.read_state` returns at this moment. Without a public id there is
                // nothing to announce; keep the old observation and try again next turn.
                let Some(read_state) = self.pane_read_state_at(ws_idx, pane_id, now) else {
                    continue;
                };
                self.emit_event(EventEnvelope {
                    event: EventKind::PaneReadStateChanged,
                    data: EventData::PaneReadStateChanged { read_state },
                });
            }
            self.read_state_observed.insert(pane_id, current);
        }

        if self.read_state_observed.len() > live_panes {
            let live: HashSet<PaneId> = self
                .state
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .flat_map(|tab| tab.panes.iter())
                .filter(|(_, pane)| {
                    self.state
                        .terminals
                        .contains_key(&pane.attached_terminal_id)
                })
                .map(|(&pane_id, _)| pane_id)
                .collect();
            self.read_state_observed
                .retain(|pane_id, _| live.contains(pane_id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::Request;
    use crate::config::Config;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;
    use serde_json::json;

    /// One workspace with a focused pane in the active tab (`front`) and a pane in a
    /// background tab (`back`). The outer terminal has focus, so a pane in the active
    /// tab is read when it changes state and a pane in a background tab is not.
    struct Fixture {
        app: App,
        front: PaneId,
        back: PaneId,
        back_tab_id: String,
    }

    fn fixture() -> Fixture {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("one")];
        let back_tab = app.state.workspaces[0].test_add_tab(Some("background"));
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.workspaces[0].switch_tab(0);
        app.state.outer_terminal_focus = Some(true);
        let front = app.state.workspaces[0].tabs[0].root_pane;
        let back = app.state.workspaces[0].tabs[back_tab].root_pane;
        let back_tab_id = app.public_tab_id(0, back_tab).unwrap();
        // First observation of every pane: recorded, no event.
        app.sync_read_state_events();
        Fixture {
            app,
            front,
            back,
            back_tab_id,
        }
    }

    fn state_changed(app: &mut App, pane_id: PaneId, state: AgentState) {
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            state,
            visible_blocker: false,
            visible_working: false,
            process_exited: false,
            observed_at: Instant::now(),
        });
    }

    fn call(app: &mut App, method: &str, params: serde_json::Value) -> serde_json::Value {
        let request: Request = serde_json::from_value(json!({
            "id": "read-state-test",
            "method": method,
            "params": params,
        }))
        .unwrap_or_else(|err| panic!("{method} request must parse: {err}"));
        serde_json::from_str(&app.handle_api_request(request)).unwrap()
    }

    fn call_ok(app: &mut App, method: &str, params: serde_json::Value) {
        let response = call(app, method, params);
        assert!(
            response.get("error").is_none(),
            "{method} must succeed: {response}"
        );
    }

    fn events_after(app: &App, sequence: u64) -> Vec<PaneReadState> {
        app.event_hub
            .events_after(sequence)
            .into_iter()
            .filter_map(|(_, event)| match event.data {
                EventData::PaneReadStateChanged { read_state } => Some(read_state),
                _ => None,
            })
            .collect()
    }

    /// The value `pane.read_state` returns for `pane_id` right now.
    fn handler_value(app: &mut App, ws_idx: usize, pane_id: PaneId) -> PaneReadState {
        let public = app.public_pane_id(ws_idx, pane_id).unwrap();
        let response = call(app, "pane.read_state", json!({ "pane_id": public }));
        serde_json::from_value(response["result"]["read_state"].clone())
            .unwrap_or_else(|_| panic!("pane.read_state must succeed: {response}"))
    }

    fn value(
        app: &App,
        ws_idx: usize,
        pane_id: PaneId,
        seen: bool,
        agent_status: AgentStatus,
    ) -> PaneReadState {
        PaneReadState {
            pane_id: app.public_pane_id(ws_idx, pane_id).unwrap(),
            workspace_id: app.public_workspace_id(ws_idx),
            seen,
            agent_status,
        }
    }

    /// Runs the comparison once and returns the events it pushed.
    fn sync(app: &mut App) -> Vec<PaneReadState> {
        let sequence = app.event_hub.current_sequence();
        app.sync_read_state_events();
        events_after(app, sequence)
    }

    /// Runs the comparison and asserts it pushed exactly one event, with exactly the body
    /// `pane.read_state` returns for that pane now.
    #[track_caller]
    fn assert_reports(
        app: &mut App,
        ws_idx: usize,
        pane_id: PaneId,
        seen: bool,
        agent_status: AgentStatus,
    ) {
        let events = sync(app);
        let expected = value(app, ws_idx, pane_id, seen, agent_status);
        assert_eq!(events, vec![expected.clone()]);
        assert_eq!(
            handler_value(app, ws_idx, pane_id),
            expected,
            "the event body must be what pane.read_state returns"
        );
    }

    /// Puts `back` in the blocked-and-unread state, reporting each step.
    fn block_back_pane(f: &mut Fixture) {
        state_changed(&mut f.app, f.back, AgentState::Working);
        assert_reports(&mut f.app, 0, f.back, true, AgentStatus::Working);
        state_changed(&mut f.app, f.back, AgentState::Blocked);
        assert_reports(&mut f.app, 0, f.back, false, AgentStatus::Blocked);
    }

    // ---- status-driven changes -------------------------------------------------------

    #[test]
    fn a_working_pane_is_reported_seen() {
        let mut f = fixture();
        state_changed(&mut f.app, f.back, AgentState::Working);
        assert_reports(&mut f.app, 0, f.back, true, AgentStatus::Working);
    }

    #[test]
    fn blocking_in_a_background_tab_is_reported_unread() {
        let mut f = fixture();
        block_back_pane(&mut f);
    }

    #[test]
    fn blocking_in_the_active_tab_is_reported_seen() {
        let mut f = fixture();
        state_changed(&mut f.app, f.front, AgentState::Working);
        assert_reports(&mut f.app, 0, f.front, true, AgentStatus::Working);
        state_changed(&mut f.app, f.front, AgentState::Blocked);
        assert_reports(&mut f.app, 0, f.front, true, AgentStatus::Blocked);
    }

    #[test]
    fn completing_in_a_background_tab_is_reported_done_and_unread() {
        let mut f = fixture();
        state_changed(&mut f.app, f.back, AgentState::Working);
        assert_reports(&mut f.app, 0, f.back, true, AgentStatus::Working);
        state_changed(&mut f.app, f.back, AgentState::Idle);
        assert_reports(&mut f.app, 0, f.back, false, AgentStatus::Done);
    }

    #[test]
    fn a_state_reported_through_the_api_is_reported() {
        let mut f = fixture();
        let public = f.app.public_pane_id(0, f.back).unwrap();
        for (seq, state) in [(1, "working"), (2, "blocked")] {
            call_ok(
                &mut f.app,
                "pane.report_agent",
                json!({
                    "pane_id": public,
                    "source": "custom:read-state-test",
                    "agent": "probe",
                    "state": state,
                    "seq": seq,
                }),
            );
        }
        let events = sync(&mut f.app);
        // The two reports land in one turn: only the net result is reported.
        assert_eq!(
            events,
            vec![value(&f.app, 0, f.back, false, AgentStatus::Blocked)]
        );
        assert_eq!(handler_value(&mut f.app, 0, f.back), events[0]);
    }

    #[test]
    fn the_compact_display_timeout_is_reported_without_any_other_change() {
        let mut f = fixture();
        let terminal_id = f.app.state.terminal_id_for_pane(0, f.front).unwrap();
        let now = Instant::now();
        let terminal = f.app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("worker".into());
        terminal.state = AgentState::Idle;
        assert_reports(&mut f.app, 0, f.front, true, AgentStatus::Idle);
        let terminal = f.app.state.terminals.get_mut(&terminal_id).unwrap();
        assert!(terminal.note_forwarded_input_at(b"/compact\r", now));
        let deadline = terminal.optimistic_working_deadline().unwrap();
        assert_reports(&mut f.app, 0, f.front, true, AgentStatus::Working);

        f.app.expire_due_metadata(deadline);
        assert_reports(&mut f.app, 0, f.front, true, AgentStatus::Idle);
    }

    // ---- seen-only changes: the agent status does not move ---------------------------

    #[test]
    fn viewing_a_tab_reports_its_blocked_pane_read_though_the_status_stays_blocked() {
        let mut f = fixture();
        block_back_pane(&mut f);
        let sequence = f.app.event_hub.current_sequence();

        let tab_id = f.back_tab_id.clone();
        call_ok(&mut f.app, "tab.focus", json!({ "tab_id": tab_id }));
        assert_reports(&mut f.app, 0, f.back, true, AgentStatus::Blocked);

        // The gap this event closes: no other event says that the pane was read.
        assert!(
            f.app
                .event_hub
                .events_after(sequence)
                .iter()
                .all(|(_, event)| event.event != EventKind::PaneAgentStatusChanged),
            "the status did not change, so no status event is emitted"
        );
    }

    #[test]
    fn focusing_a_pane_reports_it_read() {
        let mut f = fixture();
        block_back_pane(&mut f);
        let public = f.app.public_pane_id(0, f.back).unwrap();
        call_ok(&mut f.app, "pane.focus", json!({ "pane_id": public }));
        assert_reports(&mut f.app, 0, f.back, true, AgentStatus::Blocked);
    }

    #[test]
    fn focusing_an_agent_reports_it_read() {
        let mut f = fixture();
        block_back_pane(&mut f);
        let public = f.app.public_pane_id(0, f.back).unwrap();
        call_ok(&mut f.app, "agent.focus", json!({ "target": public }));
        assert_reports(&mut f.app, 0, f.back, true, AgentStatus::Blocked);
    }

    #[test]
    fn focusing_a_workspace_reports_the_panes_of_its_active_tab_read() {
        let mut f = fixture();
        f.app.state.workspaces.push(Workspace::test_new("two"));
        f.app.state.ensure_test_terminals();
        let other = f.app.state.workspaces[1].tabs[0].root_pane;
        f.app.sync_read_state_events();
        state_changed(&mut f.app, other, AgentState::Working);
        assert_reports(&mut f.app, 1, other, true, AgentStatus::Working);
        state_changed(&mut f.app, other, AgentState::Blocked);
        assert_reports(&mut f.app, 1, other, false, AgentStatus::Blocked);

        let workspace_id = f.app.public_workspace_id(1);
        call_ok(
            &mut f.app,
            "workspace.focus",
            json!({ "workspace_id": workspace_id }),
        );
        assert_reports(&mut f.app, 1, other, true, AgentStatus::Blocked);
    }

    #[test]
    fn the_window_regaining_focus_reports_the_active_tab_read() {
        let mut f = fixture();
        f.app.state.outer_terminal_focus = Some(false);
        state_changed(&mut f.app, f.front, AgentState::Working);
        assert_reports(&mut f.app, 0, f.front, true, AgentStatus::Working);
        state_changed(&mut f.app, f.front, AgentState::Blocked);
        assert_reports(&mut f.app, 0, f.front, false, AgentStatus::Blocked);

        // What the server does when the foreground client reports focus.
        f.app.state.outer_terminal_focus = Some(true);
        assert!(f.app.state.mark_active_tab_seen());
        assert_reports(&mut f.app, 0, f.front, true, AgentStatus::Blocked);
    }

    #[tokio::test]
    async fn input_forwarded_to_a_blocked_pane_reports_it_read_in_a_background_tab() {
        let mut f = fixture();
        block_back_pane(&mut f);
        let (runtime, _rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 4);
        f.app.state.insert_test_runtime(f.back, runtime);
        let public = f.app.public_pane_id(0, f.back).unwrap();

        call_ok(
            &mut f.app,
            "pane.send_text",
            json!({ "pane_id": public, "text": "yes" }),
        );

        assert!(
            !f.app.state.pane_is_in_active_tab(0, f.back),
            "the pane must still be in a background tab"
        );
        assert_reports(&mut f.app, 0, f.back, true, AgentStatus::Blocked);
    }

    #[test]
    fn a_late_auto_read_token_reports_a_finished_pane_read() {
        let mut f = fixture();
        state_changed(&mut f.app, f.back, AgentState::Working);
        assert_reports(&mut f.app, 0, f.back, true, AgentStatus::Working);
        state_changed(&mut f.app, f.back, AgentState::Idle);
        assert_reports(&mut f.app, 0, f.back, false, AgentStatus::Done);

        let public = f.app.public_pane_id(0, f.back).unwrap();
        call_ok(
            &mut f.app,
            "pane.report_metadata",
            json!({
                "pane_id": public,
                "source": "user:read-state-test",
                "tokens": { "auto_read": "1" },
            }),
        );
        assert_reports(&mut f.app, 0, f.back, true, AgentStatus::Idle);
    }

    #[test]
    fn a_completion_with_an_auto_read_token_is_reported_read() {
        let mut f = fixture();
        let public = f.app.public_pane_id(0, f.back).unwrap();
        call_ok(
            &mut f.app,
            "pane.report_metadata",
            json!({
                "pane_id": public,
                "source": "user:read-state-test",
                "tokens": { "auto_read": "1" },
            }),
        );
        state_changed(&mut f.app, f.back, AgentState::Working);
        assert_reports(&mut f.app, 0, f.back, true, AgentStatus::Working);
        state_changed(&mut f.app, f.back, AgentState::Idle);
        assert_reports(&mut f.app, 0, f.back, true, AgentStatus::Idle);
    }

    // ---- nothing to report -----------------------------------------------------------

    #[test]
    fn a_second_comparison_without_a_change_reports_nothing() {
        let mut f = fixture();
        block_back_pane(&mut f);
        assert!(sync(&mut f.app).is_empty());
        assert!(sync(&mut f.app).is_empty());
    }

    #[test]
    fn a_new_pane_is_recorded_without_an_event_and_reports_its_first_change() {
        let mut f = fixture();
        let created = f.app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        f.app.state.ensure_test_terminals();
        assert!(
            sync(&mut f.app).is_empty(),
            "a pane's first observation has nothing to differ from"
        );
        state_changed(&mut f.app, created, AgentState::Working);
        assert_reports(&mut f.app, 0, created, true, AgentStatus::Working);
    }

    #[test]
    fn a_report_that_leaves_the_value_unchanged_reports_nothing() {
        let mut f = fixture();
        block_back_pane(&mut f);
        // Blocked again: an unchanged blocked state keeps the unread flag.
        state_changed(&mut f.app, f.back, AgentState::Blocked);
        assert!(sync(&mut f.app).is_empty());
        // Viewing a pane that is already read changes nothing.
        let front_public = f.app.public_pane_id(0, f.front).unwrap();
        call_ok(&mut f.app, "pane.focus", json!({ "pane_id": front_public }));
        assert!(sync(&mut f.app).is_empty());
    }

    #[test]
    fn a_change_that_reverts_within_one_turn_reports_nothing() {
        let mut f = fixture();
        block_back_pane(&mut f);
        let tab_id = f.back_tab_id.clone();
        call_ok(&mut f.app, "tab.focus", json!({ "tab_id": tab_id }));
        f.app.state.workspaces[0]
            .pane_state_mut(f.back)
            .unwrap()
            .seen = false;
        assert!(
            sync(&mut f.app).is_empty(),
            "only the net result of a turn is reported"
        );
    }

    #[test]
    fn only_the_pane_that_changed_is_reported() {
        let mut f = fixture();
        block_back_pane(&mut f);
        state_changed(&mut f.app, f.front, AgentState::Working);
        let events = sync(&mut f.app);
        assert_eq!(
            events,
            vec![value(&f.app, 0, f.front, true, AgentStatus::Working)]
        );
    }

    #[test]
    fn a_closed_pane_is_forgotten_and_reports_nothing() {
        let mut f = fixture();
        block_back_pane(&mut f);
        assert_eq!(f.app.read_state_observed.len(), 2);
        let tab_id = f.back_tab_id.clone();
        call_ok(&mut f.app, "tab.close", json!({ "tab_id": tab_id }));
        assert!(sync(&mut f.app).is_empty());
        assert_eq!(f.app.read_state_observed.len(), 1);
        assert!(f.app.read_state_observed.contains_key(&f.front));
    }
}
