use std::collections::HashMap;

use crate::api::schema::AgentStatus;
use crate::protocol::{ClientShellAgent, ClientShellSnapshot, PaneSurfaceFrame};

#[derive(Clone, Debug, Default)]
pub(super) struct EndpointAgentPresentation {
    boot_id: Option<String>,
    acknowledged: HashMap<String, u64>,
}

impl EndpointAgentPresentation {
    pub(super) fn project_snapshot(&mut self, snapshot: &mut ClientShellSnapshot) {
        if self.boot_id.as_deref() != Some(snapshot.boot_id.as_str()) {
            self.boot_id = Some(snapshot.boot_id.clone());
            self.acknowledged.clear();
            self.acknowledged.extend(
                snapshot
                    .agents
                    .iter()
                    .filter(|agent| {
                        agent.agent_status != AgentStatus::Blocked
                            || super::agent_is_blocked_read(agent)
                    })
                    .map(|agent| (agent.pane_id.clone(), agent.state_change_seq)),
            );
        }
        self.acknowledged.retain(|pane_id, _| {
            snapshot
                .agents
                .iter()
                .any(|agent| &agent.pane_id == pane_id)
        });
        for agent in snapshot
            .agents
            .iter()
            .filter(|agent| super::agent_is_blocked_read(agent))
        {
            self.acknowledged
                .entry(agent.pane_id.clone())
                .and_modify(|seq| *seq = (*seq).max(agent.state_change_seq))
                .or_insert(agent.state_change_seq);
        }
        for agent in &mut snapshot.agents {
            self.project_agent(agent);
        }
        project_aggregate_status(snapshot);
    }

    pub(super) fn acknowledge_surface(
        &mut self,
        snapshot: &mut ClientShellSnapshot,
        surface: &PaneSurfaceFrame,
        outer_focused: Option<bool>,
    ) -> bool {
        if outer_focused == Some(false)
            || self.boot_id.as_deref() != Some(surface.boot_id.as_str())
            || snapshot.boot_id != surface.boot_id
            || snapshot.revision != surface.projection_revision
        {
            return false;
        }

        let mut changed = false;
        for pane in &surface.panes {
            let Some(agent) = snapshot
                .agents
                .iter()
                .find(|agent| agent.pane_id == pane.pane_id)
            else {
                continue;
            };
            let acknowledged = self.acknowledged.entry(agent.pane_id.clone()).or_default();
            if *acknowledged < agent.state_change_seq {
                *acknowledged = agent.state_change_seq;
                changed = true;
            }
        }
        if changed {
            for agent in &mut snapshot.agents {
                self.project_agent(agent);
            }
            project_aggregate_status(snapshot);
        }
        changed
    }

    pub(super) fn seen(&self, agent: &ClientShellAgent) -> bool {
        self.acknowledged
            .get(&agent.pane_id)
            .is_some_and(|sequence| *sequence >= agent.state_change_seq)
    }

    fn projected_status(&self, agent: &ClientShellAgent) -> AgentStatus {
        match agent.agent_status {
            AgentStatus::Idle | AgentStatus::Done => {
                if self.seen(agent) {
                    AgentStatus::Idle
                } else {
                    AgentStatus::Done
                }
            }
            status => status,
        }
    }

    fn project_agent(&self, agent: &mut ClientShellAgent) {
        let blocked_read = agent.agent_status == AgentStatus::Blocked
            && (super::agent_is_blocked_read(agent) || self.seen(agent));
        agent.agent_status = self.projected_status(agent);
        agent
            .tokens
            .retain(|(key, _)| key != crate::protocol::CLIENT_SHELL_BLOCKED_READ_TOKEN);
        if blocked_read {
            agent.tokens.push((
                crate::protocol::CLIENT_SHELL_BLOCKED_READ_TOKEN.into(),
                "1".into(),
            ));
            agent.tokens.sort_by(|left, right| left.0.cmp(&right.0));
        }
    }
}

fn project_aggregate_status(snapshot: &mut ClientShellSnapshot) {
    for tab in &mut snapshot.tabs {
        if let Some(agent) = snapshot
            .agents
            .iter()
            .filter(|agent| agent.tab_id == tab.tab_id)
            .max_by_key(|agent| super::agent_status_priority(agent))
        {
            tab.agent_status = agent.agent_status;
        }
    }
    for workspace in &mut snapshot.workspaces {
        if let Some(agent) = snapshot
            .agents
            .iter()
            .filter(|agent| agent.workspace_id == workspace.workspace_id)
            .max_by_key(|agent| super::agent_status_priority(agent))
        {
            workspace.agent_status = agent.agent_status;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{FrameData, PaneSurfacePane, SurfaceRect};

    fn agent(status: AgentStatus, sequence: u64) -> ClientShellAgent {
        ClientShellAgent {
            pane_id: "agent-pane".into(),
            workspace_id: "workspace".into(),
            tab_id: "tab".into(),
            name: None,
            display_agent: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: status,
            state_change_seq: sequence,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: true,
        }
    }

    fn snapshot(status: AgentStatus, sequence: u64, revision: u64) -> ClientShellSnapshot {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.boot_id = "endpoint-boot".into();
        snapshot.revision = revision;
        snapshot.agents = vec![agent(status, sequence)];
        snapshot
    }

    fn surface(revision: u64) -> PaneSurfaceFrame {
        let rect = SurfaceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        PaneSurfaceFrame {
            boot_id: "endpoint-boot".into(),
            projection_revision: revision,
            surface_revision: 1,
            frame: FrameData {
                cells: Vec::new(),
                width: 0,
                height: 0,
                cursor: None,
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
            },
            panes: vec![PaneSurfacePane {
                pane_id: "agent-pane".into(),
                content_revision: 1,
                rect,
                inner_rect: rect,
                scrollbar_rect: None,
                scroll: None,
                focused: true,
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                alternate_screen_active: false,
                pixel_width: 0,
                pixel_height: 0,
            }],
            splits: Vec::new(),
            popup: None,
            graphics: Default::default(),
        }
    }

    #[test]
    fn first_snapshot_does_not_acknowledge_unread_blocked() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut snapshot = snapshot(AgentStatus::Blocked, 4, 1);

        presentation.project_snapshot(&mut snapshot);

        assert!(!presentation.seen(&snapshot.agents[0]));
        assert!(!super::super::agent_is_blocked_read(&snapshot.agents[0]));
    }

    #[test]
    fn server_blocked_read_marker_updates_client_acknowledgment_state() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut unread = snapshot(AgentStatus::Blocked, 4, 1);
        presentation.project_snapshot(&mut unread);
        assert!(!presentation.seen(&unread.agents[0]));

        let mut read = snapshot(AgentStatus::Blocked, 4, 2);
        read.agents[0].tokens.push((
            crate::protocol::CLIENT_SHELL_BLOCKED_READ_TOKEN.into(),
            "1".into(),
        ));
        presentation.project_snapshot(&mut read);

        assert!(presentation.seen(&read.agents[0]));
        assert!(super::super::agent_is_blocked_read(&read.agents[0]));
    }

    #[test]
    fn coherent_surface_acknowledges_blocked_without_changing_wire_status() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut snapshot = snapshot(AgentStatus::Blocked, 4, 1);
        presentation.project_snapshot(&mut snapshot);

        assert!(presentation.acknowledge_surface(&mut snapshot, &surface(1), Some(true)));
        assert_eq!(snapshot.agents[0].agent_status, AgentStatus::Blocked);
        assert!(super::super::agent_is_blocked_read(&snapshot.agents[0]));
    }

    #[test]
    fn projected_aggregates_rank_working_over_read_blocked_but_not_unread_blocked() {
        let mut snapshot = snapshot(AgentStatus::Blocked, 4, 1);
        snapshot.tabs[0].tab_id = "tab".into();
        snapshot.workspaces[0].workspace_id = "workspace".into();
        snapshot.agents[0].tokens.push((
            crate::protocol::CLIENT_SHELL_BLOCKED_READ_TOKEN.into(),
            "1".into(),
        ));
        let mut working = agent(AgentStatus::Working, 5);
        working.pane_id = "working-pane".into();
        snapshot.agents.push(working);

        project_aggregate_status(&mut snapshot);

        assert_eq!(snapshot.tabs[0].agent_status, AgentStatus::Working);
        assert_eq!(snapshot.workspaces[0].agent_status, AgentStatus::Working);

        snapshot.agents[0].tokens.clear();
        project_aggregate_status(&mut snapshot);

        assert_eq!(snapshot.tabs[0].agent_status, AgentStatus::Blocked);
        assert_eq!(snapshot.workspaces[0].agent_status, AgentStatus::Blocked);
    }

    #[test]
    fn first_snapshot_establishes_an_idle_baseline_without_server_seen_authority() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut snapshot = snapshot(AgentStatus::Done, 4, 1);

        presentation.project_snapshot(&mut snapshot);

        assert_eq!(snapshot.agents[0].agent_status, AgentStatus::Idle);
    }

    #[test]
    fn unpresented_working_completion_projects_done() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut initial = snapshot(AgentStatus::Working, 4, 1);
        presentation.project_snapshot(&mut initial);
        let mut completed = snapshot(AgentStatus::Idle, 5, 2);

        presentation.project_snapshot(&mut completed);

        assert_eq!(completed.agents[0].agent_status, AgentStatus::Done);
    }

    #[test]
    fn coherent_presented_surface_acknowledges_completion() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut initial = snapshot(AgentStatus::Working, 4, 1);
        presentation.project_snapshot(&mut initial);
        let mut completed = snapshot(AgentStatus::Idle, 5, 2);
        presentation.project_snapshot(&mut completed);

        assert!(presentation.acknowledge_surface(&mut completed, &surface(2), Some(true)));
        assert_eq!(completed.agents[0].agent_status, AgentStatus::Idle);
    }

    #[test]
    fn clients_acknowledge_the_same_endpoint_completion_independently() {
        let mut viewing_client = EndpointAgentPresentation::default();
        let mut background_client = EndpointAgentPresentation::default();
        let mut initial_for_viewer = snapshot(AgentStatus::Working, 4, 1);
        let mut initial_for_background = initial_for_viewer.clone();
        viewing_client.project_snapshot(&mut initial_for_viewer);
        background_client.project_snapshot(&mut initial_for_background);
        let mut completed_for_viewer = snapshot(AgentStatus::Idle, 5, 2);
        let mut completed_for_background = completed_for_viewer.clone();
        viewing_client.project_snapshot(&mut completed_for_viewer);
        background_client.project_snapshot(&mut completed_for_background);

        assert!(viewing_client.acknowledge_surface(
            &mut completed_for_viewer,
            &surface(2),
            Some(true)
        ));

        assert_eq!(
            completed_for_viewer.agents[0].agent_status,
            AgentStatus::Idle
        );
        assert_eq!(
            completed_for_background.agents[0].agent_status,
            AgentStatus::Done
        );
    }

    #[test]
    fn stale_or_unfocused_surface_does_not_acknowledge_completion() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut initial = snapshot(AgentStatus::Working, 4, 1);
        presentation.project_snapshot(&mut initial);
        let mut completed = snapshot(AgentStatus::Idle, 5, 2);
        presentation.project_snapshot(&mut completed);

        assert!(!presentation.acknowledge_surface(&mut completed, &surface(1), Some(true)));
        assert!(!presentation.acknowledge_surface(&mut completed, &surface(2), Some(false)));
        assert_eq!(completed.agents[0].agent_status, AgentStatus::Done);
    }
}
