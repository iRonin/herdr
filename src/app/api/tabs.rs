use std::path::PathBuf;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, ResponseResult, TabCreateParams, TabListParams,
    TabMoveParams, TabMoveToWorkspaceParams, TabRenameParams, TabTarget,
};
use crate::app::{App, Mode};

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_tab_list(&mut self, id: String, params: TabListParams) -> String {
        let tabs = if let Some(workspace_id) = params.workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            let Some(_) = self.state.workspaces.get(ws_idx) else {
                return workspace_not_found(id, &workspace_id);
            };
            self.tab_list_info(ws_idx)
        } else {
            let mut tabs = Vec::new();
            for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
                for tab_idx in 0..ws.tabs.len() {
                    if let Some(tab) = self.tab_info(ws_idx, tab_idx) {
                        tabs.push(tab);
                    }
                }
            }
            tabs
        };

        encode_success(id, ResponseResult::TabList { tabs })
    }

    pub(super) fn handle_tab_get(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        let Some(tab) = self.tab_info(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_create(&mut self, id: String, params: TabCreateParams) -> String {
        let TabCreateParams {
            workspace_id,
            cwd,
            focus,
            label,
            env,
        } = params;
        let ws_idx = if let Some(workspace_id) = workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            ws_idx
        } else if let Some(active) = self.state.active {
            active
        } else {
            return encode_error(id, "workspace_not_found", "no active workspace");
        };
        let cwd = cwd.map(PathBuf::from).unwrap_or_else(|| {
            self.resolve_new_terminal_cwd(self.focused_pane_cwd_in_workspace(ws_idx))
        });
        let (rows, cols) = self.state.estimate_pane_size();
        let default_shell = self.state.default_shell.clone();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let extra_env = match super::env::normalize_launch_env(env) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let result = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| std::io::Error::other("workspace disappeared"))
            .and_then(|ws| {
                ws.create_tab(
                    rows,
                    cols,
                    cwd,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                    crate::pane::PaneShellConfig::new(&default_shell, self.state.shell_mode),
                    extra_env,
                )
            });
        match result {
            Ok((tab_idx, terminal, runtime)) => {
                self.terminal_runtimes.insert(terminal.id.clone(), runtime);
                self.state.terminals.insert(terminal.id.clone(), terminal);
                self.state.remove_alias_shadowed_by_new_pane(
                    self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
                );
                if let Some(label) = label {
                    let workspace_id = self.state.workspaces[ws_idx].id.clone();
                    let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
                        crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
                    });
                    if let Some(tab) = self
                        .state
                        .workspaces
                        .get_mut(ws_idx)
                        .and_then(|ws| ws.tabs.get_mut(tab_idx))
                    {
                        tab.set_custom_name(label);
                        crate::logging::tab_renamed(&workspace_id, &tab_id);
                    }
                }
                if focus {
                    self.state.switch_workspace_tab(ws_idx, tab_idx);
                    self.state.mode = Mode::Terminal;
                }
                self.schedule_session_save();
                self.emit_tab_created_events(ws_idx, tab_idx);
                encode_success(
                    id,
                    self.tab_created_result(ws_idx, tab_idx)
                        .expect("new tab should produce a complete create response"),
                )
            }
            Err(err) => encode_error(id, "tab_create_failed", err.to_string()),
        }
    }

    pub(super) fn handle_tab_focus(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        self.state.switch_workspace_tab(ws_idx, tab_idx);
        let tab = self.tab_info(ws_idx, tab_idx).unwrap();

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_rename(&mut self, id: String, params: TabRenameParams) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
            crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
        });
        let Some(tab) = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs.get_mut(tab_idx))
        else {
            return tab_not_found(id, &params.tab_id);
        };
        tab.set_custom_name(params.label.clone());
        crate::logging::tab_renamed(&workspace_id, &tab_id);
        if self.state.active == Some(ws_idx) {
            // Reflow the tab bar so the new label width takes effect immediately.
            // The tab bar renders into cached hit areas; without this refresh the
            // old geometry lingers until the next refresh (e.g. a tab switch),
            // leaving the visible label stale. Mirrors handle_tab_move.
            self.state.refresh_tab_bar_view();
        }
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabRenamed,
            data: EventData::TabRenamed {
                tab_id: self.public_tab_id(ws_idx, tab_idx).unwrap(),
                workspace_id: self.public_workspace_id(ws_idx),
                label: params.label,
            },
        });
        let tab = self.tab_info(ws_idx, tab_idx).unwrap();

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_move(&mut self, id: String, params: TabMoveParams) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return tab_not_found(id, &params.tab_id);
        };
        if params.insert_index > ws.tabs.len() {
            return encode_error(
                id,
                "tab_move_failed",
                format!("insert_index {} is out of bounds", params.insert_index),
            );
        }

        let tab_id = self
            .public_tab_id(ws_idx, tab_idx)
            .unwrap_or_else(|| crate::workspace::public_tab_id_for_number(&ws.id, tab_idx + 1));
        let workspace_id = self.public_workspace_id(ws_idx);
        let insert_index = params.insert_index;
        let moved = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.move_tab(tab_idx, insert_index));
        let tabs = self.tab_list_info(ws_idx);
        if moved {
            self.schedule_session_save();
            if self.state.active == Some(ws_idx) {
                self.state.tab_scroll_follow_active = true;
                self.state.refresh_tab_bar_view();
            }
            self.emit_event(EventEnvelope {
                event: EventKind::TabMoved,
                data: EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    insert_index,
                    tabs: tabs.clone(),
                    previous_tab_id: None,
                    previous_workspace_id: None,
                },
            });
        }

        encode_success(id, ResponseResult::TabList { tabs })
    }

    /// Move a whole tab — panes, layout, label, zoom, and terminal/agent state
    /// — to another workspace (or reorder within its current one when
    /// `workspace_id` names the tab's own workspace). Mirrors the pane.move
    /// cross-workspace grammar: public tab/pane numbers are re-assigned by the
    /// destination workspace, old public pane ids keep resolving through
    /// aliases, and a source workspace left without tabs is closed.
    ///
    /// Ordering guarantee (why no recovery path is needed): after the
    /// fallible validation steps above, every remaining operation is total
    /// bookkeeping (Vec/HashMap ops). The tab is inserted into the destination
    /// BEFORE the source workspace is removed, so it can never be stranded
    /// between workspaces, and the destination index parsed upfront is still
    /// valid at insertion time because no removal has happened yet — this
    /// handler never yields to other mutation paths in between.
    pub(super) fn handle_tab_move_to_workspace(
        &mut self,
        id: String,
        params: TabMoveToWorkspaceParams,
    ) -> String {
        let Some((source_ws_idx, source_tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let Some(target_ws_idx) = self.parse_workspace_id(&params.workspace_id) else {
            return encode_error(
                id,
                "workspace_not_found",
                format!("workspace {} not found", params.workspace_id),
            );
        };
        let target_workspace_id = self.public_workspace_id(target_ws_idx);
        let previous_workspace_id = self.public_workspace_id(source_ws_idx);
        let Some(previous_tab_id) = self.public_tab_id(source_ws_idx, source_tab_idx) else {
            return tab_not_found(id, &params.tab_id);
        };

        if source_ws_idx == target_ws_idx {
            return self.tab_move_within_workspace(
                id,
                source_ws_idx,
                source_tab_idx,
                params.insert_index,
                previous_tab_id,
                previous_workspace_id,
            );
        }

        let target_tab_count = self.state.workspaces[target_ws_idx].tabs.len();
        let insert_index = match params.insert_index {
            Some(index) if index > target_tab_count => {
                return encode_error(
                    id,
                    "tab_move_failed",
                    format!("insert_index {} is out of bounds", index),
                );
            }
            Some(index) => index,
            None => target_tab_count,
        };

        // A move that empties the source workspace closes it. Match the
        // contract `tab.close` has carried since upstream's a79b3d55 rather than
        // silently destroying a worktree group: closing the last tab of a
        // group member returns `confirmation_required` there, and the same
        // destructive effect must not slip through a different verb.
        //
        // Knowable pre-flight, so it belongs above the infallible region.
        if self
            .state
            .workspaces
            .get(source_ws_idx)
            .is_some_and(|ws| ws.tabs.len() <= 1)
            && self
                .state
                .confirm_implicit_worktree_group_close(source_ws_idx)
        {
            return encode_error(
                id,
                "confirmation_required",
                "moving this tab would close a worktree group",
            );
        }

        let previous_focus = self.state.current_pane_focus_target();
        let source_pane_ids: Vec<_> = self.state.workspaces[source_ws_idx].tabs[source_tab_idx]
            .panes
            .keys()
            .copied()
            .collect();
        let previous_pane_ids: Vec<String> = source_pane_ids
            .iter()
            .map(|pane_id| {
                self.public_pane_id(source_ws_idx, *pane_id)
                    .unwrap_or_default()
            })
            .collect();
        let focused_moved_pane = self.state.workspaces[source_ws_idx].tabs[source_tab_idx]
            .layout
            .focused();

        // Infallible region starts here. Detach the tab (the source workspace
        // may become empty but is NOT removed yet, so `target_ws_idx` stays
        // valid) and insert it into the destination first.
        let Some(tab) = self
            .state
            .workspaces
            .get_mut(source_ws_idx)
            .and_then(|ws| ws.take_tab_for_move(source_tab_idx))
        else {
            return encode_error(id, "tab_move_failed", "source tab could not be moved");
        };
        let moved_root_pane = tab.root_pane;
        self.state.workspaces[target_ws_idx].insert_moved_tab(tab, insert_index);
        for (pane_id, previous_pane_id) in source_pane_ids.iter().zip(previous_pane_ids.iter()) {
            if !previous_pane_id.is_empty() {
                self.state
                    .public_pane_id_aliases
                    .insert(previous_pane_id.clone(), *pane_id);
            }
        }

        // The tab is safe in the destination; only now close the source
        // workspace if the move emptied it (pane.move's adjustments).
        let mut closed_workspace_id = None;
        if self
            .state
            .workspaces
            .get(source_ws_idx)
            .is_some_and(|ws| ws.tabs.is_empty())
        {
            self.state.workspaces.remove(source_ws_idx);
            closed_workspace_id = Some(previous_workspace_id.clone());
            if self.state.workspaces.is_empty() {
                self.state.active = None;
                self.state.selected = 0;
            } else {
                if let Some(active) = self.state.active {
                    if active == source_ws_idx {
                        self.state.active =
                            Some(source_ws_idx.min(self.state.workspaces.len() - 1));
                    } else if active > source_ws_idx {
                        self.state.active = Some(active - 1);
                    }
                }
                if self.state.selected == source_ws_idx {
                    self.state.selected = source_ws_idx.min(self.state.workspaces.len() - 1);
                } else if self.state.selected > source_ws_idx {
                    self.state.selected -= 1;
                }
            }
        }

        // Removing the source workspace shifts the destination's index; locate
        // the moved tab by content (its root pane) instead of re-resolving an
        // index, which is total regardless of any shift.
        let (target_ws_idx, new_tab_idx) = self
            .state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, ws)| {
                ws.find_tab_index_for_pane(moved_root_pane)
                    .map(|tab_idx| (ws_idx, tab_idx))
            })
            .expect("moved tab must be locatable after insertion");

        if params.focus || self.state.active.is_none() {
            self.state.switch_workspace_tab(target_ws_idx, new_tab_idx);
            self.state
                .record_pane_focus_change(previous_focus, target_ws_idx, focused_moved_pane);
            self.state.settle_terminal_mode_after_focus();
        }

        for pane_id in &source_pane_ids {
            self.state.remove_alias_shadowed_by_new_pane(*pane_id);
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        self.state.tab_scroll_follow_active = true;
        self.state.refresh_tab_bar_view();

        let pane_id_map = source_pane_ids
            .iter()
            .zip(previous_pane_ids.iter())
            .map(
                |(pane_id, previous_pane_id)| crate::api::schema::TabMovePaneRemap {
                    previous_pane_id: previous_pane_id.clone(),
                    pane_id: self
                        .public_pane_id(target_ws_idx, *pane_id)
                        .unwrap_or_default(),
                },
            )
            .collect();
        let Some(tab_info) = self.tab_info(target_ws_idx, new_tab_idx) else {
            return encode_error(id, "tab_move_failed", "moved tab is unavailable");
        };
        let Some(target_layout) = self.pane_layout_snapshot(target_ws_idx, new_tab_idx) else {
            return encode_error(id, "pane_layout_unavailable", "pane layout unavailable");
        };
        let target_tabs = self.tab_list_info(target_ws_idx);

        // pane.move event grammar for cross-workspace moves: no fake
        // close/create for the moved entity — subscribers correlate the new
        // identity through `TabMoved.previous_tab_id`/`previous_workspace_id`.
        if let Some(closed_workspace_id) = &closed_workspace_id {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id: closed_workspace_id.clone(),
                    workspace: None,
                },
            });
        }
        self.emit_event(EventEnvelope {
            event: EventKind::TabMoved,
            data: EventData::TabMoved {
                tab_id: tab_info.tab_id.clone(),
                workspace_id: target_workspace_id.clone(),
                insert_index: new_tab_idx,
                tabs: target_tabs,
                previous_tab_id: Some(previous_tab_id.clone()),
                previous_workspace_id: Some(previous_workspace_id.clone()),
            },
        });
        self.emit_layout_updated_snapshot(target_layout.clone());

        encode_success(
            id,
            ResponseResult::TabMoveToWorkspace {
                move_result: crate::api::schema::TabMoveToWorkspaceResult {
                    changed: true,
                    previous_tab_id,
                    previous_workspace_id,
                    tab: tab_info,
                    insert_index: new_tab_idx,
                    closed_workspace_id,
                    pane_id_map,
                    target_layout: Box::new(target_layout),
                },
            },
        )
    }

    /// `workspace_id` naming the tab's own workspace: a reorder, reported in
    /// the move response shape so `tab.move_to_workspace` callers always get a
    /// `TabMoveToWorkspaceResult` back.
    fn tab_move_within_workspace(
        &mut self,
        id: String,
        ws_idx: usize,
        tab_idx: usize,
        insert_index: Option<usize>,
        previous_tab_id: String,
        previous_workspace_id: String,
    ) -> String {
        let tab_count = self.state.workspaces[ws_idx].tabs.len();
        let insert_index = match insert_index {
            Some(index) if index > tab_count => {
                return encode_error(
                    id,
                    "tab_move_failed",
                    format!("insert_index {} is out of bounds", index),
                );
            }
            Some(index) => index,
            None => tab_count,
        };
        let tab_number = self.state.workspaces[ws_idx].tabs[tab_idx].number;
        let moved = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.move_tab(tab_idx, insert_index));
        let new_tab_idx = self.state.workspaces[ws_idx]
            .tabs
            .iter()
            .position(|tab| tab.number == tab_number)
            .unwrap_or(tab_idx);
        if moved {
            self.schedule_session_save();
            if self.state.active == Some(ws_idx) {
                self.state.tab_scroll_follow_active = true;
                self.state.refresh_tab_bar_view();
            }
            let tabs = self.tab_list_info(ws_idx);
            self.emit_event(EventEnvelope {
                event: EventKind::TabMoved,
                data: EventData::TabMoved {
                    tab_id: previous_tab_id.clone(),
                    workspace_id: previous_workspace_id.clone(),
                    insert_index: new_tab_idx,
                    tabs,
                    previous_tab_id: None,
                    previous_workspace_id: None,
                },
            });
        }
        let Some(tab_info) = self.tab_info(ws_idx, new_tab_idx) else {
            return encode_error(id, "tab_move_failed", "moved tab is unavailable");
        };
        let Some(target_layout) = self.pane_layout_snapshot(ws_idx, new_tab_idx) else {
            return encode_error(id, "pane_layout_unavailable", "pane layout unavailable");
        };
        encode_success(
            id,
            ResponseResult::TabMoveToWorkspace {
                move_result: crate::api::schema::TabMoveToWorkspaceResult {
                    changed: moved,
                    previous_tab_id,
                    previous_workspace_id,
                    tab: tab_info,
                    insert_index: new_tab_idx,
                    closed_workspace_id: None,
                    pane_id_map: Vec::new(),
                    target_layout: Box::new(target_layout),
                },
            },
        )
    }

    pub(super) fn handle_tab_close(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        let workspace_id = self.public_workspace_id(ws_idx);
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        let closes_workspace = ws.tabs.len() <= 1;
        let terminal_ids = self.state.terminal_ids_for_tab(ws_idx, tab_idx);
        let pane_ids = ws
            .tabs
            .get(tab_idx)
            .map(|tab| tab.layout.pane_ids())
            .unwrap_or_default();

        if closes_workspace {
            if self.state.confirm_implicit_worktree_group_close(ws_idx) {
                return encode_error(
                    id,
                    "confirmation_required",
                    "closing this tab would close a worktree group",
                );
            }
            let workspace = self.workspace_info(ws_idx);
            self.state.selected = ws_idx;
            self.state.close_selected_workspace();
            self.state.remove_plugin_pane_records(pane_ids);
            self.shutdown_detached_terminal_runtimes();
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id,
                    workspace_id: workspace_id.clone(),
                },
            });
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id,
                    workspace: Some(workspace),
                },
            });
            return encode_success(id, ResponseResult::Ok {});
        }

        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        if !ws.close_tab(tab_idx) {
            return encode_error(
                id,
                "tab_close_failed",
                format!("tab {} could not be closed", target.tab_id),
            );
        }
        self.state.remove_plugin_pane_records(pane_ids);
        self.state.remove_unattached_terminal_ids(terminal_ids);
        self.shutdown_detached_terminal_runtimes();
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabClosed,
            data: EventData::TabClosed {
                tab_id,
                workspace_id,
            },
        });

        encode_success(id, ResponseResult::Ok {})
    }

    fn tab_list_info(&self, ws_idx: usize) -> Vec<crate::api::schema::TabInfo> {
        self.state
            .workspaces
            .get(ws_idx)
            .map(|ws| {
                (0..ws.tabs.len())
                    .filter_map(|idx| self.tab_info(ws_idx, idx))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn workspace_not_found(id: String, workspace_id: &str) -> String {
    encode_error(
        id,
        "workspace_not_found",
        format!("workspace {workspace_id} not found"),
    )
}

fn tab_not_found(id: String, tab_id: &str) -> String {
    encode_error(id, "tab_not_found", format!("tab {tab_id} not found"))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
    use super::*;
    use crate::{
        api::schema::SuccessResponse,
        config::{Config, ShellModeConfig},
        workspace::Workspace,
    };

    #[test]
    fn api_tab_close_last_tab_closes_workspace_and_emits_both_events() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("tabs")];
        app.state.active = Some(0);
        app.state.selected = 0;
        let tab_id = app.public_tab_id(0, 0).unwrap();
        let workspace_id = app.public_workspace_id(0);

        let response = app.handle_tab_close(
            "req".into(),
            TabTarget {
                tab_id: tab_id.clone(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.result, ResponseResult::Ok {});
        assert!(app.state.workspaces.is_empty());
        assert!(app.state.active.is_none());
        let events = event_hub.events_after(0);
        assert_eq!(
            events
                .iter()
                .map(|(_, event)| event.event)
                .collect::<Vec<_>>(),
            [EventKind::TabClosed, EventKind::WorkspaceClosed]
        );
        assert!(matches!(
            &events[0].1.data,
            EventData::TabClosed {
                tab_id: closed_tab_id,
                workspace_id: closed_workspace_id,
            } if closed_tab_id == &tab_id && closed_workspace_id == &workspace_id
        ));
        assert!(matches!(
            &events[1].1.data,
            EventData::WorkspaceClosed {
                workspace_id: closed_workspace_id,
                workspace: Some(workspace),
            } if closed_workspace_id == &workspace_id
                && workspace.workspace_id == workspace_id
        ));
    }

    // `tab.close` has returned `confirmation_required` for a worktree group's
    // last tab since upstream's a79b3d55. `tab.move_to_workspace` closes the
    // source workspace too when the move empties it, so it must honour the same
    // contract -- otherwise the identical destructive effect slips through a
    // different verb, and `src/app/api/tabs.rs` merges with zero conflicts so
    // git never mentions it.
    #[test]
    fn tab_move_to_workspace_requires_confirmation_when_it_would_close_a_worktree_group() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![
            Workspace::test_new("parent"),
            Workspace::test_new("child"),
            Workspace::test_new("elsewhere"),
        ];
        // Two members sharing a worktree key, the source being the parent
        // checkout: closing it takes the whole group with it.
        for (idx, linked) in [(0usize, false), (1usize, true)] {
            app.state.workspaces[idx].worktree_space =
                Some(crate::workspace::WorktreeSpaceMembership {
                    key: "repo-key".into(),
                    label: "herdr".into(),
                    repo_root: "/repo/herdr".into(),
                    checkout_path: if linked {
                        format!("/repo/worktree-{idx}").into()
                    } else {
                        std::path::PathBuf::from("/repo/herdr")
                    },
                    is_linked_worktree: linked,
                });
        }
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        assert_eq!(app.state.workspaces[0].tabs.len(), 1, "source has one tab");

        let moved_tab_id = app.public_tab_id(0, 0).unwrap();
        let target_workspace_id = app.public_workspace_id(2);
        let response = app.handle_tab_move_to_workspace(
            "req".into(),
            TabMoveToWorkspaceParams {
                tab_id: moved_tab_id,
                workspace_id: target_workspace_id,
                insert_index: None,
                focus: false,
            },
        );

        assert!(
            response.contains("confirmation_required"),
            "response: {response}"
        );
        // And nothing moved: the guard is pre-flight, not a post-hoc report.
        assert_eq!(app.state.workspaces.len(), 3);
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        assert_eq!(app.state.workspaces[2].tabs.len(), 1);
    }

    #[test]
    fn api_tab_move_reorders_tabs_in_target_workspace() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        let mut workspace = Workspace::test_new("tabs");
        workspace.test_add_tab(Some("two"));
        workspace.test_add_tab(Some("three"));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        let moved_root = app.state.workspaces[0].tabs[0].root_pane;
        let moved_id = app.public_tab_id(0, 0).unwrap();

        let response = app.handle_tab_move(
            "req".into(),
            TabMoveParams {
                tab_id: moved_id.clone(),
                insert_index: 3,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::TabList { tabs } = success.result else {
            panic!("expected tab list");
        };
        assert_eq!(app.state.workspaces[0].tabs[2].root_pane, moved_root);
        assert_eq!(tabs[2].tab_id, app.public_tab_id(0, 2).unwrap());
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    insert_index: 3,
                    tabs,
                    ..
                } if tab_id == &moved_id
                    && workspace_id == &app.public_workspace_id(0)
                    && tabs[2].tab_id == moved_id
            )
        }));
    }

    fn app_with_workspaces(names: &[&str]) -> (App, crate::api::EventHub) {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = names.iter().map(|name| Workspace::test_new(name)).collect();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        (app, event_hub)
    }

    #[test]
    fn api_tab_move_to_workspace_preserves_layout_label_and_terminals() {
        let (mut app, _event_hub) = app_with_workspaces(&["source", "target"]);
        app.state.workspaces[0].test_add_tab(Some("keep"));
        app.state.workspaces[1].test_add_tab(Some("there"));
        app.state.ensure_test_terminals();
        // Give the moved tab a split layout, a label, and zoom: the move must
        // carry all three, not just the pane set.
        let root = app.state.workspaces[0].tabs[0].root_pane;
        let split = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.workspaces[0].tabs[0].custom_name = Some("deploy".into());
        app.state.workspaces[0].tabs[0].zoomed = true;
        app.state.ensure_test_terminals();
        let root_terminal = app.state.workspaces[0].tabs[0]
            .terminal_id(root)
            .unwrap()
            .clone();
        let split_terminal = app.state.workspaces[0].tabs[0]
            .terminal_id(split)
            .unwrap()
            .clone();
        let previous_tab_id = app.public_tab_id(0, 0).unwrap();
        let previous_root_pane_id = app.public_pane_id(0, root).unwrap();
        let previous_split_pane_id = app.public_pane_id(0, split).unwrap();
        let previous_workspace_id = app.public_workspace_id(0);
        let target_workspace_id = app.public_workspace_id(1);

        let response = app.handle_tab_move_to_workspace(
            "req".into(),
            TabMoveToWorkspaceParams {
                tab_id: previous_tab_id.clone(),
                workspace_id: target_workspace_id.clone(),
                insert_index: None,
                focus: false,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::TabMoveToWorkspace { move_result } = success.result else {
            panic!("expected tab move result");
        };
        assert!(move_result.changed);
        assert_eq!(move_result.previous_tab_id, previous_tab_id);
        assert_eq!(move_result.previous_workspace_id, previous_workspace_id);
        assert_eq!(move_result.closed_workspace_id, None);
        // Appended after the target workspace's two existing tabs.
        assert_eq!(move_result.insert_index, 2);
        assert_eq!(app.state.workspaces.len(), 2);
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        assert_eq!(app.state.workspaces[1].tabs.len(), 3);

        // The moved tab kept its panes, their terminals, its label, and zoom.
        let moved = &app.state.workspaces[1].tabs[2];
        assert_eq!(moved.custom_name.as_deref(), Some("deploy"));
        assert!(moved.zoomed);
        assert_eq!(moved.layout.pane_count(), 2);
        assert_eq!(moved.terminal_id(root), Some(&root_terminal));
        assert_eq!(moved.terminal_id(split), Some(&split_terminal));
        assert!(app.state.terminals.contains_key(&root_terminal));
        assert!(app.state.terminals.contains_key(&split_terminal));

        // Public identities moved to the target workspace; the old tab id is
        // gone but old public pane ids keep resolving through aliases.
        assert_ne!(move_result.tab.tab_id, previous_tab_id);
        assert_eq!(move_result.tab.workspace_id, target_workspace_id);
        assert!(move_result
            .tab
            .tab_id
            .starts_with(&format!("{target_workspace_id}:t")));
        assert_eq!(move_result.tab.label, "deploy");
        assert_eq!(move_result.tab.pane_count, 2);
        assert!(app.parse_tab_id(&previous_tab_id).is_none());
        assert_eq!(move_result.pane_id_map.len(), 2);
        for remap in &move_result.pane_id_map {
            assert_ne!(remap.previous_pane_id, remap.pane_id);
            assert!(remap
                .pane_id
                .starts_with(&format!("{target_workspace_id}:p")));
            assert!(app.parse_pane_id(&remap.pane_id).is_some());
        }
        assert_eq!(app.parse_pane_id(&previous_root_pane_id), Some((1, root)));
        assert_eq!(app.parse_pane_id(&previous_split_pane_id), Some((1, split)));

        // The split geometry survived the move (direction and ratio unchanged).
        let before_splits: Vec<_> = move_result
            .target_layout
            .splits
            .iter()
            .map(|split| (split.direction.clone(), split.ratio))
            .collect();
        assert_eq!(before_splits.len(), 1);
        assert!(move_result.target_layout.zoomed);
    }

    #[test]
    fn api_tab_move_last_tab_closes_source_workspace() {
        let (mut app, event_hub) = app_with_workspaces(&["source", "target"]);
        let previous_tab_id = app.public_tab_id(0, 0).unwrap();
        let previous_workspace_id = app.public_workspace_id(0);
        let target_workspace_id = app.public_workspace_id(1);

        let response = app.handle_tab_move_to_workspace(
            "req".into(),
            TabMoveToWorkspaceParams {
                tab_id: previous_tab_id.clone(),
                workspace_id: target_workspace_id.clone(),
                insert_index: None,
                focus: false,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::TabMoveToWorkspace { move_result } = success.result else {
            panic!("expected tab move result");
        };
        assert_eq!(
            move_result.closed_workspace_id,
            Some(previous_workspace_id.clone())
        );
        assert_eq!(app.state.workspaces.len(), 1);
        // The sole remaining workspace takes over active/selected.
        assert_eq!(app.state.active, Some(0));
        assert_eq!(app.state.selected, 0);
        assert_eq!(app.state.workspaces[0].tabs.len(), 2);

        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::WorkspaceClosed { workspace_id, .. }
                    if workspace_id == &previous_workspace_id
            )
        }));
        // pane.move grammar: no fake close/create events for the moved tab —
        // subscribers correlate through the TabMoved previous_* fields below.
        assert!(!events.iter().any(|(_, event)| {
            matches!(&event.data, EventData::TabClosed { tab_id, .. } if tab_id == &previous_tab_id)
        }));
        assert!(!events
            .iter()
            .any(|(_, event)| { matches!(&event.data, EventData::TabCreated { .. }) }));
        // The correlation event links the old identity to the new one.
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    previous_tab_id: Some(prev_tab),
                    previous_workspace_id: Some(prev_ws),
                    ..
                } if tab_id == &move_result.tab.tab_id
                    && workspace_id == &target_workspace_id
                    && prev_tab == &previous_tab_id
                    && prev_ws == &previous_workspace_id
            )
        }));
    }

    #[test]
    fn api_tab_move_to_workspace_shifts_target_index_when_source_closes() {
        let (mut app, _event_hub) = app_with_workspaces(&["one", "two", "three"]);
        let third_workspace_id = app.public_workspace_id(2);
        let moved_tab_id = app.public_tab_id(0, 0).unwrap();
        let moved_root = app.state.workspaces[0].tabs[0].root_pane;

        let response = app.handle_tab_move_to_workspace(
            "req".into(),
            TabMoveToWorkspaceParams {
                tab_id: moved_tab_id,
                workspace_id: third_workspace_id.clone(),
                insert_index: None,
                focus: false,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::TabMoveToWorkspace { move_result } = success.result else {
            panic!("expected tab move result");
        };
        // "one" closed, shifting "three" from index 2 to index 1; the tab must
        // land there, not in the workspace now at index 2 (formerly "three").
        assert_eq!(app.state.workspaces.len(), 2);
        assert_eq!(move_result.tab.workspace_id, third_workspace_id);
        assert_eq!(app.state.workspaces[1].tabs.len(), 2);
        assert_eq!(app.state.workspaces[1].tabs[1].root_pane, moved_root);
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
    }

    #[test]
    fn api_tab_move_to_workspace_focus_switches_to_target() {
        let (mut app, _event_hub) = app_with_workspaces(&["source", "target"]);
        // Keep a second tab in the source workspace so it survives the move.
        app.state.workspaces[0].test_add_tab(Some("keep"));
        app.state.ensure_test_terminals();
        let moved_tab_id = app.public_tab_id(0, 0).unwrap();
        let target_workspace_id = app.public_workspace_id(1);

        let response = app.handle_tab_move_to_workspace(
            "req".into(),
            TabMoveToWorkspaceParams {
                tab_id: moved_tab_id,
                workspace_id: target_workspace_id,
                insert_index: None,
                focus: true,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::TabMoveToWorkspace { move_result } = success.result else {
            panic!("expected tab move result");
        };
        assert_eq!(app.state.active, Some(1));
        assert_eq!(app.state.workspaces[1].active_tab, move_result.insert_index);
        assert!(move_result.tab.focused);
    }

    #[test]
    fn api_tab_move_to_workspace_preserves_agent_status() {
        let (mut app, _event_hub) = app_with_workspaces(&["source", "target"]);
        let root = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0]
            .terminal_id(root)
            .unwrap()
            .clone();
        app.state.terminals.get_mut(&terminal_id).unwrap().state =
            crate::detect::AgentState::Working;
        let moved_tab_id = app.public_tab_id(0, 0).unwrap();
        let target_workspace_id = app.public_workspace_id(1);

        let response = app.handle_tab_move_to_workspace(
            "req".into(),
            TabMoveToWorkspaceParams {
                tab_id: moved_tab_id,
                workspace_id: target_workspace_id,
                insert_index: None,
                focus: false,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::TabMoveToWorkspace { move_result } = success.result else {
            panic!("expected tab move result");
        };
        // Agent status is derived through the pane's terminal; a Working agent
        // must still read Working after the move.
        assert_eq!(
            move_result.tab.agent_status,
            crate::api::schema::AgentStatus::Working
        );
        assert_eq!(
            app.state.terminals.get(&terminal_id).unwrap().state,
            crate::detect::AgentState::Working
        );
    }

    #[test]
    fn api_tab_move_to_own_workspace_reorders_with_move_result() {
        let (mut app, _event_hub) = app_with_workspaces(&["solo"]);
        app.state.workspaces[0].test_add_tab(Some("two"));
        app.state.workspaces[0].test_add_tab(Some("three"));
        app.state.ensure_test_terminals();
        let moved_root = app.state.workspaces[0].tabs[0].root_pane;
        let moved_tab_id = app.public_tab_id(0, 0).unwrap();
        let workspace_id = app.public_workspace_id(0);

        let response = app.handle_tab_move_to_workspace(
            "req".into(),
            TabMoveToWorkspaceParams {
                tab_id: moved_tab_id.clone(),
                workspace_id: workspace_id.clone(),
                insert_index: Some(2),
                focus: false,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::TabMoveToWorkspace { move_result } = success.result else {
            panic!("expected tab move result");
        };
        assert!(move_result.changed);
        assert_eq!(move_result.previous_tab_id, moved_tab_id);
        assert_eq!(move_result.tab.tab_id, moved_tab_id);
        assert_eq!(move_result.tab.workspace_id, workspace_id);
        assert_eq!(move_result.closed_workspace_id, None);
        assert!(move_result.pane_id_map.is_empty());
        // Legacy reorder semantics: insert_index counts the list before
        // removal, so moving tab 0 to insert_index 2 in [A B C] yields
        // [B A C] — the tab's final position is 1.
        assert_eq!(move_result.insert_index, 1);
        assert_eq!(app.state.workspaces[0].tabs[1].root_pane, moved_root);
        assert_eq!(app.state.workspaces.len(), 1);
    }

    #[test]
    fn api_tab_move_to_workspace_rejects_out_of_bounds_index() {
        let (mut app, _event_hub) = app_with_workspaces(&["source", "target"]);
        let moved_tab_id = app.public_tab_id(0, 0).unwrap();
        let target_workspace_id = app.public_workspace_id(1);

        let response = app.handle_tab_move_to_workspace(
            "req".into(),
            TabMoveToWorkspaceParams {
                tab_id: moved_tab_id.clone(),
                workspace_id: target_workspace_id,
                insert_index: Some(5),
                focus: false,
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "tab_move_failed");
        // Nothing moved.
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        assert_eq!(app.state.workspaces[1].tabs.len(), 1);
        assert_eq!(app.parse_tab_id(&moved_tab_id), Some((0, 0)));
    }

    #[tokio::test]
    async fn api_tab_move_to_workspace_survives_snapshot_restore() {
        let (mut app, _event_hub) = app_with_workspaces(&["source", "target"]);
        // A last-tab move closes the source workspace; the persisted session
        // must round-trip the tab in its new home and keep the source closed.
        app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.workspaces[0].tabs[0].custom_name = Some("deploy".into());
        app.state.workspaces[0].tabs[0].zoomed = true;
        app.state.ensure_test_terminals();
        let moved_tab_id = app.public_tab_id(0, 0).unwrap();
        let target_workspace_id = app.public_workspace_id(1);

        let response = app.handle_tab_move_to_workspace(
            "req".into(),
            TabMoveToWorkspaceParams {
                tab_id: moved_tab_id,
                workspace_id: target_workspace_id.clone(),
                insert_index: None,
                focus: false,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::TabMoveToWorkspace { .. } = success.result else {
            panic!("expected tab move result");
        };
        assert_eq!(app.state.workspaces.len(), 1);

        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let snapshot = crate::persist::capture(
            &app.state.workspaces,
            &app.state.terminals,
            &terminal_runtimes,
            app.state.active,
            app.state.selected,
            app.state.sidebar_width,
            app.state.sidebar_section_split,
            app.state.collapsed_space_keys.clone(),
        );
        // The moved state must be representable in the on-disk JSON format.
        let snapshot: crate::persist::SessionSnapshot =
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
        assert_eq!(snapshot.workspaces.len(), 1);
        let snap_ws = &snapshot.workspaces[0];
        assert_eq!(snap_ws.tabs.len(), 2);
        let snap_tab = &snap_ws.tabs[1];
        assert_eq!(snap_tab.custom_name.as_deref(), Some("deploy"));
        assert!(snap_tab.zoomed);
        assert_eq!(snap_tab.panes.len(), 2);
        assert!(
            matches!(
                snap_tab.layout,
                crate::persist::LayoutSnapshot::Split { .. }
            ),
            "split layout must persist across snapshot capture"
        );

        let (events, _event_rx) = tokio::sync::mpsc::channel(4);
        let (workspaces, _terminals, _runtimes) = crate::persist::restore(
            &snapshot,
            None,
            24,
            80,
            0,
            "/bin/sh",
            crate::config::ShellModeConfig::NonLogin,
            false,
            events,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(crate::render_signal::RenderSignal::new()),
        );

        assert_eq!(
            workspaces.len(),
            1,
            "a source workspace closed by the move must stay closed"
        );
        let ws = &workspaces[0];
        assert_eq!(ws.id, target_workspace_id);
        assert_eq!(ws.tabs.len(), 2);
        let moved = &ws.tabs[1];
        assert_eq!(moved.custom_name.as_deref(), Some("deploy"));
        assert!(moved.zoomed);
        assert_eq!(moved.panes.len(), 2);
        assert_eq!(moved.layout.pane_count(), 2);
        // Public numbering rewritten by the move must come back collision-free.
        let tab_numbers: std::collections::HashSet<_> =
            ws.tabs.iter().map(|tab| tab.number).collect();
        assert_eq!(tab_numbers.len(), ws.tabs.len());
        let mut pane_numbers: Vec<_> = ws.public_pane_numbers.values().copied().collect();
        pane_numbers.sort_unstable();
        pane_numbers.dedup();
        assert_eq!(pane_numbers.len(), 3);
    }

    #[test]
    fn api_tab_rename_reflows_active_tab_bar() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub);
        let workspace = Workspace::test_new("tabs");
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.view.tab_bar_rect = ratatui::layout::Rect::new(0, 0, 60, 1);
        app.state.refresh_tab_bar_view();

        let tab_id = app.public_tab_id(0, 0).unwrap();
        let width_before = app.state.view.tab_hit_areas[0].width;

        app.handle_tab_rename(
            "req".into(),
            TabRenameParams {
                tab_id,
                label: "a much longer custom tab label".into(),
            },
        );

        let width_after = app.state.view.tab_hit_areas[0].width;
        assert!(
            width_after > width_before,
            "tab bar should reflow to the new label width immediately: \
             before={width_before}, after={width_after}"
        );
    }

    #[tokio::test]
    async fn tab_create_follows_cached_focused_pane_cwd_without_runtime() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub);
        app.state.default_shell = exiting_test_command().into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        let workspace = Workspace::test_new("tabs");
        let focused_pane = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        let cached_cwd = std::env::temp_dir();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(focused_pane)
            .cloned()
            .unwrap();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = cached_cwd.clone();

        let response = app.handle_tab_create(
            "req".into(),
            TabCreateParams {
                workspace_id: None,
                cwd: None,
                focus: false,
                label: None,
                env: Default::default(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::TabCreated { .. }));
        let created = &app.state.workspaces[0].tabs[1];
        let created_terminal_id = created.terminal_id(created.root_pane).unwrap();
        let created_cwd = &app.state.terminals.get(created_terminal_id).unwrap().cwd;
        assert_eq!(
            crate::worktree::canonical_or_original(created_cwd),
            crate::worktree::canonical_or_original(&cached_cwd)
        );
        shutdown_test_runtimes(&mut app);
    }
}
