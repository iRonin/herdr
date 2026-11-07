use std::time::{Duration, Instant};

use super::{App, SESSION_SAVE_DEBOUNCE};

enum SessionSaveJob {
    Clear,
    Save {
        snapshot: crate::persist::SessionSnapshot,
        history: Option<crate::persist::SessionHistorySnapshot>,
    },
}

impl App {
    pub(super) fn schedule_session_save(&mut self) {
        if self.policy.persist_session {
            self.pane_exit_checkpoint_pending = false;
            self.deferred_checkpoint = None;
            self.session_save_deadline = Some(Instant::now() + SESSION_SAVE_DEBOUNCE);
        }
    }

    pub(crate) fn sync_session_save_schedule(&mut self) {
        if self.state.session_dirty {
            self.state.session_dirty = false;
            self.schedule_session_save();
        }
    }

    fn reap_finished_session_save(&mut self) {
        if self
            .session_save_thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            if let Some(thread) = self.session_save_thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn capture_session_save_job(&self) -> SessionSaveJob {
        if self.state.workspaces.is_empty() {
            SessionSaveJob::Clear
        } else {
            let snapshot = crate::persist::capture(
                &self.state.workspaces,
                &self.state.terminals,
                &self.terminal_runtimes,
                self.state.active,
                self.state.selected,
            );
            let history = self.persist_pane_history.then(|| {
                crate::persist::capture_history(
                    &snapshot,
                    &self.state.workspaces,
                    &self.terminal_runtimes,
                )
            });
            SessionSaveJob::Save { snapshot, history }
        }
    }

    pub(crate) fn start_background_session_save(&mut self) {
        if !self.policy.persist_session {
            self.session_save_deadline = None;
            return;
        }

        self.reap_finished_session_save();
        if self.session_save_thread.is_some() {
            self.session_save_deadline = Some(Instant::now() + Duration::from_millis(250));
            return;
        }

        let job = self.capture_session_save_job();
        self.pane_exit_checkpoint_pending = false;
        self.deferred_checkpoint = None;
        self.session_save_deadline = None;
        let writer = self.session_writer.clone();
        match std::thread::Builder::new()
            .name("herdr-session-save".into())
            .spawn(move || run_session_save_job(job, &writer))
        {
            Ok(thread) => self.session_save_thread = Some(thread),
            Err(err) => {
                tracing::warn!(err = %err, "failed to spawn session save thread; saving inline");
                run_session_save_job(self.capture_session_save_job(), &self.session_writer);
            }
        }
    }

    pub(crate) fn save_session_now(&mut self) {
        if let Some(thread) = self.session_save_thread.take() {
            let _ = thread.join();
        }

        if !self.policy.persist_session {
            self.session_save_deadline = None;
            return;
        }

        run_session_save_job(self.capture_session_save_job(), &self.session_writer);
        self.pane_exit_checkpoint_pending = false;
        self.deferred_checkpoint = None;
        self.session_save_deadline = None;
    }

    pub(crate) fn checkpoint_session_before_pane_exit(&mut self) {
        if !self.policy.persist_session
            || (self.pane_exit_checkpoint_pending && !self.state.session_dirty)
        {
            return;
        }
        self.save_session_now();
        self.pane_exit_checkpoint_pending = true;
        self.state.session_dirty = false;
    }

    /// The checkpoint of a pane that ends with its resumed agent. Same bookkeeping as `checkpoint_session_before_pane_exit`, with
    /// two differences, both because it runs on the server loop and can run once per agent exit in a burst of them:
    ///
    /// - It writes the layout only (`SessionWriter::save_layout_only`). The full save also captures and writes every pane's
    ///   screen history, which took 3.9 s for 250 panes with 200 KB of history each (about 9 ms without the history). The next
    ///   debounced autosave rewrites both files; a stop before it restores every pane, and the history file, whose layout
    ///   fingerprint then no longer matches the saved layout, is ignored once.
    /// - It never waits for a background save. While one is writing, it holds the writer, and the layout that save writes was
    ///   captured before changes made since, so it is no checkpoint of this moment. The checkpoint keeps the layout it just
    ///   captured instead (`deferred_checkpoint`), which holds those changes and the pane that is ending with the agent session it
    ///   was resumed from (this exit is what clears that session). `save_session_on_shutdown` writes the kept layout after the
    ///   running save ends. Anything that clears or sets the pending flag clears or replaces it: an autosave or a full save
    ///   supersedes it, and a later checkpoint that gets the writer writes a newer layout.
    pub(crate) fn checkpoint_session_layout_before_pane_exit(&mut self) {
        if !self.policy.persist_session
            || (self.pane_exit_checkpoint_pending && !self.state.session_dirty)
        {
            return;
        }
        if self.state.workspaces.is_empty() {
            self.checkpoint_session_before_pane_exit();
            return;
        }
        let snapshot = crate::persist::capture(
            &self.state.workspaces,
            &self.state.terminals,
            &self.terminal_runtimes,
            self.state.active,
            self.state.selected,
        );
        match self.session_writer.try_lock() {
            Ok(mut writer) => {
                writer.save_layout_only(&snapshot);
                self.deferred_checkpoint = None;
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                tracing::info!(
                    event = "persist.checkpoint",
                    subsystem = "persist",
                    outcome = "background_save_running",
                    "a background save is writing; the pane-exit checkpoint keeps its layout for the stop"
                );
                self.deferred_checkpoint = Some(snapshot);
            }
            Err(std::sync::TryLockError::Poisoned(err)) => {
                tracing::warn!(err = %err, "session writer is poisoned; refusing to modify session");
                return;
            }
        }
        self.session_save_deadline = None;
        self.pane_exit_checkpoint_pending = true;
        self.state.session_dirty = false;
    }

    pub(crate) fn finish_checkpointed_pane_exit(&mut self) {
        if self.pane_exit_checkpoint_pending {
            self.state.session_dirty = false;
            self.session_save_deadline = Some(Instant::now() + SESSION_SAVE_DEBOUNCE);
        }
    }

    pub(crate) fn save_session_on_shutdown(&mut self) {
        if self.pane_exit_checkpoint_pending && !self.state.session_dirty {
            self.write_deferred_checkpoint();
            self.session_save_deadline = None;
            return;
        }
        self.save_session_now();
    }

    /// Writes the layout a checkpoint kept because a background save held the writer (see
    /// `checkpoint_session_layout_before_pane_exit`): waits for that save to end, as upstream's checkpoint always did, then
    /// writes the layout only. Blocking is fine here, at the stop.
    fn write_deferred_checkpoint(&mut self) {
        let Some(snapshot) = self.deferred_checkpoint.take() else {
            return;
        };
        if let Some(thread) = self.session_save_thread.take() {
            let _ = thread.join();
        }
        match self.session_writer.lock() {
            Ok(mut writer) => writer.save_layout_only(&snapshot),
            Err(err) => {
                tracing::warn!(err = %err, "session writer is poisoned; refusing to modify session");
            }
        }
    }
}

fn run_session_save_job(
    job: SessionSaveJob,
    writer: &std::sync::Mutex<crate::persist::SessionWriter>,
) {
    let mut writer = match writer.lock() {
        Ok(writer) => writer,
        Err(err) => {
            tracing::warn!(err = %err, "session writer is poisoned; refusing to modify session");
            return;
        }
    };
    match job {
        SessionSaveJob::Clear => writer.clear(),
        SessionSaveJob::Save { snapshot, history } => writer.save(&snapshot, history.as_ref()),
    }
}
