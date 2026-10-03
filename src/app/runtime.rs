use std::time::Instant;

#[cfg(test)]
use std::time::Duration;

use super::{
    background_update_check_enabled, App, AUTO_UPDATE_CHECK_INTERVAL, MIN_RENDER_INTERVAL,
};
fn retain_detached_process_after_wait(
    pid: u32,
    result: std::io::Result<Option<std::process::ExitStatus>>,
) -> bool {
    match result {
        Ok(None) => true,
        Ok(Some(_)) => false,
        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => true,
        Err(err) => {
            tracing::warn!(pid, err = %err, "failed to reap detached process");
            false
        }
    }
}

impl App {
    pub(crate) fn reap_finished_detached_processes(&mut self) {
        self.detached_process_children
            .retain_mut(|child| retain_detached_process_after_wait(child.id(), child.try_wait()));
    }

    pub(crate) fn shutdown_terminal_runtime(&mut self, terminal_id: crate::terminal::TerminalId) {
        if let Some(runtime) = self.terminal_runtimes.remove(&terminal_id) {
            runtime.shutdown();
        }
    }

    pub(crate) fn shutdown_detached_terminal_runtimes(&mut self) {
        let terminal_ids = std::mem::take(&mut self.state.terminal_runtime_shutdowns);
        for terminal_id in terminal_ids {
            self.shutdown_terminal_runtime(terminal_id);
        }
    }

    pub(crate) fn sync_agent_metadata_deadline(&mut self) {
        self.agent_metadata_deadline = self.state.next_agent_metadata_expiry();
    }

    pub(crate) fn expire_due_metadata(&mut self, now: Instant) -> bool {
        // This loop checkpoint also owns the display-only `/compact` TTL. Both
        // expiries require the same full-surface repaint and API/client refresh.
        let expired_optimistic_panes = self.state.expire_optimistic_working_at(now);
        let optimistic_working_expired = !expired_optimistic_panes.is_empty();
        for (ws_idx, pane_id) in expired_optimistic_panes {
            self.emit_projected_pane_agent_status_changed(ws_idx, pane_id);
        }
        let Some(deadline) = self
            .agent_metadata_deadline
            .filter(|deadline| now >= *deadline)
        else {
            return optimistic_working_expired;
        };
        self.expire_metadata_at(deadline, now);
        true
    }

    pub(crate) fn expire_metadata_at(&mut self, deadline: Instant, now: Instant) {
        let previous_toast = self.state.toast.clone();
        for update in self.state.expire_agent_metadata_at(deadline, now) {
            self.refresh_new_herdr_toast_context_for_update(&update, &previous_toast);
            self.emit_pane_state_update(&update);
        }
        let (panes, workspaces) = self.state.expire_metadata_tokens(now);
        for (ws_idx, pane_id) in panes {
            self.emit_pane_updated(ws_idx, pane_id);
        }
        for ws_idx in workspaces {
            self.emit_workspace_token_updated(ws_idx);
        }
        self.sync_agent_metadata_deadline();
    }

    pub(crate) fn can_render_now(&self, now: Instant) -> bool {
        match self.last_render_at {
            Some(last_render_at) => now.duration_since(last_render_at) >= MIN_RENDER_INTERVAL,
            None => true,
        }
    }

    pub(crate) fn can_present_now(&self, now: Instant) -> bool {
        match self.last_presentation_at {
            Some(last_presentation_at) => {
                now.duration_since(last_presentation_at) >= MIN_RENDER_INTERVAL
            }
            None => true,
        }
    }

    pub(crate) fn record_render_attempt(&mut self, now: Instant, presentation: bool) {
        self.last_render_at = Some(now);
        if presentation {
            self.last_presentation_at = Some(now);
        }
    }

    pub(crate) fn run_auto_update_check(&mut self) {
        if !background_update_check_enabled(
            self.policy.background_updates,
            self.update_version_check_enabled,
        ) {
            self.next_auto_update_check = None;
            return;
        }

        self.next_auto_update_check = self
            .state
            .update_available
            .is_none()
            .then_some(Instant::now() + AUTO_UPDATE_CHECK_INTERVAL);

        if self.state.update_available.is_some() {
            return;
        }

        let update_tx = self.event_tx.clone();
        std::thread::spawn(move || crate::update::auto_update(update_tx));
    }

    pub(crate) fn run_agent_manifest_update_check(&mut self) {
        if !background_update_check_enabled(
            self.policy.background_updates,
            self.update_manifest_check_enabled,
        ) {
            self.next_agent_manifest_update_check = None;
            return;
        }

        self.next_agent_manifest_update_check = Some(Instant::now() + AUTO_UPDATE_CHECK_INTERVAL);

        let manifest_update_tx = self.event_tx.clone();
        std::thread::spawn(move || crate::detect::manifest_update::auto_update(manifest_update_tx));
    }

    pub(crate) fn next_headless_loop_deadline_with_git_refresh(
        &self,
        now: Instant,
        needs_render: bool,
        include_git_refresh: bool,
    ) -> Option<Instant> {
        let render_deadline = if needs_render {
            self.last_render_at
                .map(|last_render_at| last_render_at + MIN_RENDER_INTERVAL)
                .filter(|deadline| *deadline > now)
        } else {
            None
        };

        [
            self.config_diagnostic_deadline,
            self.toast_deadline,
            self.state.next_pending_agent_notification_deadline(),
            self.state.next_managed_agent_deadline(),
            include_git_refresh
                .then(|| self.git_refresh_deadline())
                .flatten(),
            self.next_auto_update_check,
            self.next_agent_manifest_update_check,
            self.agent_metadata_deadline,
            self.pending_agent_resume_deadline,
            self.session_save_deadline,
            self.next_tab_bar_status_deadline(),
            self.state.next_optimistic_working_deadline(),
            render_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    #[cfg(test)]
    pub(crate) fn drain_internal_events(&mut self) -> bool {
        self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT)
            .1
    }

    #[cfg(test)]
    pub(crate) fn drain_all_internal_events(&mut self) -> bool {
        let mut changed = false;
        loop {
            let (had_event, batch_changed) =
                self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT);
            changed |= batch_changed;
            if !had_event {
                break;
            }
        }
        changed
    }

    #[cfg(test)]
    fn drain_internal_events_up_to(&mut self, limit: usize) -> (bool, bool) {
        let mut had_event = false;
        let mut changed = false;
        for _ in 0..limit {
            let Ok(ev) = self.event_rx.try_recv() else {
                break;
            };
            had_event = true;
            changed |= self.handle_internal_event_with_render_impact(ev);
        }
        (had_event, changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;

    #[test]
    fn hidden_render_attempt_keeps_presentation_cadence_available() {
        let (mut app, _) = test_app_with_pane();
        let initial_presentation = Instant::now();
        app.record_render_attempt(initial_presentation, true);

        let hidden_attempt = initial_presentation + MIN_RENDER_INTERVAL;
        app.record_render_attempt(hidden_attempt, false);
        let foreground_echo = hidden_attempt + Duration::from_millis(1);

        assert!(!app.can_render_now(foreground_echo));
        assert!(app.can_present_now(foreground_echo));
    }

    #[test]
    fn interrupted_detached_process_wait_keeps_child_for_retry() {
        let interrupted = std::io::Error::new(std::io::ErrorKind::Interrupted, "test interrupt");

        assert!(retain_detached_process_after_wait(42, Err(interrupted)));
    }

    #[test]
    fn compact_overlay_deadline_wakes_the_loop_and_expiry_requests_repaint() {
        let (mut app, pane_id) = test_app_with_pane();
        let terminal_id = app.state.terminal_id_for_pane(0, pane_id).unwrap();
        let now = Instant::now();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("worker".into());
        terminal.state = crate::detect::AgentState::Idle;
        assert!(terminal.note_forwarded_input_at(b"/compact\r", now));
        let deadline = terminal.optimistic_working_deadline().unwrap();

        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, false),
            Some(deadline),
            "the display TTL must wake an otherwise idle headless loop"
        );
        assert!(!app.expire_due_metadata(deadline - Duration::from_millis(1)));
        assert!(app.event_hub.events_after(0).is_empty());
        assert!(
            app.expire_due_metadata(deadline),
            "expiry must request a repaint even without metadata deadlines"
        );
        assert_eq!(
            app.state.terminals[&terminal_id].display_state_at(deadline),
            crate::detect::AgentState::Idle
        );
        assert!(app.state.terminals[&terminal_id]
            .optimistic_working_deadline()
            .is_none());
        let events = app.event_hub.events_after(0);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].1.data,
            crate::api::schema::EventData::PaneAgentStatusChanged {
                agent_status: crate::api::schema::AgentStatus::Idle,
                ..
            }
        ));
        assert!(app.state.toast.is_none());
    }

    fn test_app_with_pane() -> (super::super::App, crate::layout::PaneId) {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        app.state.workspaces.push(ws);
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.view.pane_infos.push(crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 80, 24),
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::NONE,
            is_focused: true,
        });
        (app, pane_id)
    }
}
