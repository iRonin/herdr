use std::collections::HashMap;

use crate::detect::AgentState;
use crate::layout::PaneId;
use crate::terminal::{TerminalId, TerminalState};

use super::{Tab, Workspace};

/// Detail info for a single pane, used by the agent detail panel.
pub struct PaneDetail {
    pub pane_id: PaneId,
    pub tab_idx: usize,
    pub agent_kind_label: Option<String>,
    pub state: AgentState,
    pub seen: bool,
    pub last_agent_state_change_seq: Option<u64>,
    pub tokens: HashMap<String, String>,
}

impl Tab {
    fn pane_details(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        tab_idx: usize,
    ) -> Vec<PaneDetail> {
        self.layout
            .pane_ids()
            .iter()
            .filter_map(|id| {
                let pane = self.panes.get(id)?;
                let terminal = terminals.get(&pane.attached_terminal_id)?;
                let agent_kind_label = terminal.effective_agent_label().map(str::to_string);
                if terminal.agent_name.is_none() && agent_kind_label.is_none() {
                    return None;
                }
                Some(PaneDetail {
                    pane_id: *id,
                    tab_idx,
                    agent_kind_label,
                    state: terminal.display_state(),
                    seen: pane.seen,
                    last_agent_state_change_seq: terminal.last_agent_state_change_seq,
                    tokens: terminal.metadata_tokens.values(),
                })
            })
            .collect()
    }
}

/// Rank of one pane's state for the container that holds it, a workspace or a tab: unread blocked >
/// read blocked > unread done > working > read idle > unknown.
///
/// A container shows blocked when any agent in it is blocked, read or not, as upstream does
/// (`(Blocked, _) => 4`); the read flag only splits the two blocked ranks, so the mark stays
/// read-aware (an unread block anywhere wins over a read one). The agent list orders agents by
/// `tab_attention_priority` instead, which sinks a read block below an unread finish; that order
/// is for agents and must not be used to pick what a container shows.
pub(crate) fn container_attention_priority(state: AgentState, seen: bool) -> u8 {
    match (state, seen) {
        (AgentState::Blocked, false) => 5,
        (AgentState::Blocked, true) => 4,
        (AgentState::Idle, false) => 3,
        (AgentState::Working, _) => 2,
        (AgentState::Idle, true) => 1,
        (AgentState::Unknown, _) => 0,
    }
}

impl Workspace {
    pub fn aggregate_state(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
    ) -> (AgentState, bool) {
        self.tabs
            .iter()
            .flat_map(|tab| tab.panes.values())
            .filter_map(|pane| {
                terminals
                    .get(&pane.attached_terminal_id)
                    .map(|terminal| (terminal.display_state(), pane.seen))
            })
            .max_by_key(|(state, seen)| container_attention_priority(*state, *seen))
            .unwrap_or((AgentState::Unknown, true))
    }

    pub fn pane_details(&self, terminals: &HashMap<TerminalId, TerminalState>) -> Vec<PaneDetail> {
        self.tabs
            .iter()
            .enumerate()
            .flat_map(|(tab_idx, tab)| tab.pane_details(terminals, tab_idx))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Direction;

    use super::*;
    use crate::detect::Agent;

    fn terminal_for_pane(ws: &Workspace, pane_id: PaneId) -> TerminalState {
        TerminalState::new(ws.terminal_id(pane_id).unwrap().clone(), "/tmp".into())
    }

    #[test]
    fn container_rank_puts_any_block_first_and_an_unread_block_above_a_read_one() {
        let rank = container_attention_priority;
        assert!(rank(AgentState::Blocked, false) > rank(AgentState::Blocked, true));
        assert!(rank(AgentState::Blocked, true) > rank(AgentState::Idle, false));
        assert!(rank(AgentState::Idle, false) > rank(AgentState::Working, true));
        assert!(rank(AgentState::Working, true) > rank(AgentState::Idle, true));
        assert!(rank(AgentState::Idle, true) > rank(AgentState::Unknown, true));
        // Working outranks idle whatever the read flag says, as upstream has it.
        assert_eq!(
            rank(AgentState::Working, false),
            rank(AgentState::Working, true)
        );
    }

    /// What the workspace aggregate reports for these panes, each given as `(agent state, seen)` and
    /// placed in layout order. `aggregate_state` walks the tab's pane map (a `HashMap`), not the
    /// layout, so a case and its mirror image do NOT control the order the aggregate sees them in:
    /// the mirror cases only add samples. The tie rule itself is pinned by
    /// `container_rank_puts_any_block_first_and_an_unread_block_above_a_read_one`, whose strict ranks
    /// leave no tie between an unread and a read block.
    fn aggregate_of(panes: &[(AgentState, bool)]) -> (AgentState, bool) {
        let mut ws = Workspace::test_new("test");
        for _ in 1..panes.len() {
            ws.test_split(Direction::Horizontal);
        }
        let ids = ws.tabs[0].layout.pane_ids();
        assert_eq!(
            ids.len(),
            panes.len(),
            "the fixture holds one pane per case entry"
        );
        let mut terminals = HashMap::new();
        for (id, (state, seen)) in ids.iter().zip(panes) {
            let mut terminal = terminal_for_pane(&ws, *id);
            terminal.state = *state;
            terminals.insert(terminal.id.clone(), terminal);
            ws.tabs[0].panes.get_mut(id).unwrap().seen = *seen;
        }
        ws.aggregate_state(&terminals)
    }

    /// A space holding an agent that is blocked must show blocked, whether or not that block was
    /// already read and whatever else the space holds. Upstream ranks every blocked pane first;
    /// the fork's read-aware ranking let an unread finished agent (or a working one) hide a block
    /// the user had merely looked at, which is the one state a space has to surface.
    #[test]
    fn aggregate_state_shows_blocked_when_any_agent_is_blocked_read_or_not() {
        let read_blocked = (AgentState::Blocked, true);
        let unread_blocked = (AgentState::Blocked, false);
        for (other, label) in [
            ((AgentState::Idle, false), "unread done"),
            ((AgentState::Working, true), "working"),
            ((AgentState::Idle, true), "read idle"),
            ((AgentState::Unknown, true), "unknown"),
        ] {
            assert_eq!(
                aggregate_of(&[read_blocked, other]),
                read_blocked,
                "read blocked then {label}"
            );
            assert_eq!(
                aggregate_of(&[other, read_blocked]),
                read_blocked,
                "{label} then read blocked"
            );
        }
        // The mark stays read-aware: an unread block anywhere in the space makes it unread. The
        // map order is not controlled here (see `aggregate_of`); the strict rank test above is
        // what proves an unread block wins whichever pane the walk reaches last.
        assert_eq!(
            aggregate_of(&[unread_blocked, read_blocked]),
            unread_blocked
        );
        assert_eq!(
            aggregate_of(&[read_blocked, unread_blocked]),
            unread_blocked
        );
        assert_eq!(
            aggregate_of(&[read_blocked, (AgentState::Idle, false), unread_blocked]),
            unread_blocked
        );
        // Controls: with no blocked agent the order is unchanged (an unread finish still beats
        // work in progress), so the checks above cannot pass by flattening everything.
        assert_eq!(
            aggregate_of(&[(AgentState::Working, true), (AgentState::Idle, false)]),
            (AgentState::Idle, false)
        );
        assert_eq!(
            aggregate_of(&[(AgentState::Idle, true), (AgentState::Working, true)]),
            (AgentState::Working, true)
        );
    }

    #[test]
    fn aggregate_state_all_unknown() {
        let ws = Workspace::test_new("test");
        let mut terminals = HashMap::new();
        let root = ws.tabs[0].root_pane;
        let terminal = terminal_for_pane(&ws, root);
        terminals.insert(terminal.id.clone(), terminal);
        let (state, seen) = ws.aggregate_state(&terminals);
        assert_eq!(state, AgentState::Unknown);
        assert!(seen);
    }

    #[test]
    fn aggregate_state_uses_compact_display_overlay_without_mutating_raw_state() {
        let ws = Workspace::test_new("test");
        let root = ws.tabs[0].root_pane;
        let mut terminal = terminal_for_pane(&ws, root);
        terminal.set_agent_name("worker".into());
        terminal.state = AgentState::Idle;
        let now = std::time::Instant::now();
        assert!(terminal.note_forwarded_input_at(b"/compact\r", now));
        assert_eq!(terminal.state, AgentState::Idle);
        let mut terminals = HashMap::from([(terminal.id.clone(), terminal)]);

        assert_eq!(ws.aggregate_state(&terminals), (AgentState::Working, true));
        assert_eq!(ws.pane_details(&terminals)[0].state, AgentState::Working);

        let deadline = terminals
            .values()
            .next()
            .unwrap()
            .optimistic_working_deadline()
            .unwrap();
        terminals
            .values_mut()
            .next()
            .unwrap()
            .expire_optimistic_working_at(deadline);
        assert_eq!(ws.aggregate_state(&terminals), (AgentState::Idle, true));
    }

    #[test]
    fn aggregate_state_priority() {
        let mut ws = Workspace::test_new("test");
        let id2 = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != id2)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Idle;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut second_terminal = terminal_for_pane(&ws, id2);
        second_terminal.state = AgentState::Working;
        terminals.insert(second_terminal.id.clone(), second_terminal);

        let (state, seen) = ws.aggregate_state(&terminals);

        assert_eq!(state, AgentState::Working);
        assert!(seen);
    }

    #[test]
    fn aggregate_state_done_unseen_beats_working() {
        let mut ws = Workspace::test_new("test");
        let id2 = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != id2)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Idle;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut second_terminal = terminal_for_pane(&ws, id2);
        second_terminal.state = AgentState::Working;
        terminals.insert(second_terminal.id.clone(), second_terminal);
        let root = ws.tabs[0].panes.get_mut(&root_id).unwrap();
        root.seen = false;

        let (state, seen) = ws.aggregate_state(&terminals);

        assert_eq!(state, AgentState::Idle);
        assert!(!seen);
    }

    #[test]
    fn pane_details_use_tab_vector_index_not_stable_public_tab_number() {
        let mut ws = Workspace::test_new("test");
        let removed_tab = ws.test_add_tab(Some("removed"));
        let survivor_tab = ws.test_add_tab(Some("survivor"));
        let survivor_pane = ws.tabs[survivor_tab].root_pane;
        assert!(ws.close_tab(removed_tab));

        let mut terminals = HashMap::new();
        let mut terminal = terminal_for_pane(&ws, survivor_pane);
        terminal.detected_agent = Some(Agent::Codex);
        terminals.insert(terminal.id.clone(), terminal);

        let details = ws.pane_details(&terminals);
        let survivor = details
            .iter()
            .find(|detail| detail.pane_id == survivor_pane)
            .expect("surviving tab agent should be listed");

        assert_eq!(ws.tabs[1].number, 3);
        assert_eq!(survivor.tab_idx, 1);
    }
}
