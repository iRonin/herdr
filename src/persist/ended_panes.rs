//! Leaving panes whose agent ended out of a restore (`[session] skip_ended_agent_panes_on_restore`).
//!
//! A save with the setting on marks a pane whose agent ended and whose shell is idle (`PaneSnapshot::agent_ended`). The start
//! that reads such a file runs `prune_ended_agent_panes` over the loaded snapshot and the loaded pane history, before the
//! unchanged `restore`, so everything `restore` indexes by position (the history, `active`, `selected`, `active_tab`) is fixed in
//! one place.

use super::snapshot::{SessionHistorySnapshot, SessionSnapshot};

/// What `prune_ended_agent_panes` left out of the session.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DroppedEndedPanes {
    pub panes: usize,
    pub tabs: usize,
    pub workspaces: usize,
    /// The ids of the workspaces that went, so a new workspace does not reuse one.
    pub workspace_ids: Vec<String>,
}

/// Whether any pane of the snapshot carries the mark.
pub fn has_ended_agent_panes(snapshot: &SessionSnapshot) -> bool {
    let _ = snapshot;
    false
}

/// Removes every marked pane from the snapshot, and from the history in step with it: the layout of a tab that loses a pane,
/// the tab that loses all of them, the workspace that loses every tab, and the indices that pointed past them.
pub fn prune_ended_agent_panes(
    snapshot: &mut SessionSnapshot,
    history: &mut Option<SessionHistorySnapshot>,
) -> DroppedEndedPanes {
    let _ = (snapshot, history);
    DroppedEndedPanes::default()
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    use super::super::snapshot::{
        layout_fingerprint, match_history_to_layout, DirectionSnapshot, HistoryLayoutMatch,
        LayoutSnapshot, PaneHistorySnapshot, PaneSnapshot, TabHistorySnapshot, TabSnapshot,
        WorkspaceHistorySnapshot, WorkspaceSnapshot, SNAPSHOT_VERSION,
    };
    use super::*;

    /// A pane of a test layout: its raw id, and whether the saved layout marks it `agent_ended`.
    type Spec = (u32, bool);

    fn plain(id: u32) -> Spec {
        (id, false)
    }

    fn ended(id: u32) -> Spec {
        (id, true)
    }

    fn pane(spec: Spec) -> PaneSnapshot {
        PaneSnapshot {
            cwd: PathBuf::from(format!("/work/{}", spec.0)),
            label: None,
            agent_name: None,
            managed_agent_kind: None,
            agent_session: None,
            agent_resume: None,
            launch_argv: None,
            agent_ended: spec.1,
        }
    }

    /// A tab whose layout is a chain of splits in the order given: `[1, 2, 3]` is `1 | (2 | 3)`. Pane 1's id is the focused
    /// and the root pane, so a test marks it when it wants those to dangle.
    fn tab(specs: &[Spec]) -> TabSnapshot {
        let (last, rest) = specs.split_last().expect("a tab has a pane");
        TabSnapshot {
            custom_name: None,
            layout: rest
                .iter()
                .rev()
                .fold(LayoutSnapshot::Pane(last.0), |second, spec| {
                    LayoutSnapshot::Split {
                        direction: DirectionSnapshot::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutSnapshot::Pane(spec.0)),
                        second: Box::new(second),
                    }
                }),
            panes: specs.iter().map(|spec| (spec.0, pane(*spec))).collect(),
            zoomed: false,
            focused: Some(specs[0].0),
            root_pane: Some(specs[0].0),
        }
    }

    /// A workspace with the public numbers a herdr saves: panes numbered 1.. in order, tabs numbered 1.. in order.
    fn workspace(id: &str, tabs: &[&[Spec]]) -> WorkspaceSnapshot {
        let mut public_pane_numbers = HashMap::new();
        for spec in tabs.iter().flat_map(|specs| specs.iter()) {
            let number = public_pane_numbers.len() + 1;
            public_pane_numbers.insert(spec.0, number);
        }
        WorkspaceSnapshot {
            id: Some(id.to_string()),
            custom_name: None,
            identity_cwd: PathBuf::from("/work"),
            worktree_space: None,
            next_public_pane_number: public_pane_numbers.len() + 1,
            public_pane_numbers,
            public_tab_numbers: (1..=tabs.len()).collect(),
            next_public_tab_number: tabs.len() + 1,
            tabs: tabs.iter().map(|specs| tab(specs)).collect(),
            active_tab: 0,
        }
    }

    fn session(
        workspaces: Vec<WorkspaceSnapshot>,
        active: Option<usize>,
        selected: usize,
    ) -> SessionSnapshot {
        SessionSnapshot {
            version: SNAPSHOT_VERSION,
            workspaces,
            active,
            selected,
            sidebar_width: Some(26),
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
        }
    }

    /// The history a herdr saves with `snapshot`: an entry for every pane, in the same positions.
    fn history_of(
        snapshot: &SessionSnapshot,
        fingerprint: Option<String>,
    ) -> SessionHistorySnapshot {
        SessionHistorySnapshot {
            version: SNAPSHOT_VERSION,
            layout_fingerprint: fingerprint,
            workspaces: snapshot
                .workspaces
                .iter()
                .map(|workspace| WorkspaceHistorySnapshot {
                    tabs: workspace
                        .tabs
                        .iter()
                        .map(|tab| TabHistorySnapshot {
                            panes: tab
                                .panes
                                .keys()
                                .map(|id| {
                                    (
                                        *id,
                                        PaneHistorySnapshot {
                                            ansi: format!("screen of pane {id}\r\n"),
                                            lines: 1,
                                        },
                                    )
                                })
                                .collect(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    fn layout_ids(node: &LayoutSnapshot) -> Vec<u32> {
        match node {
            LayoutSnapshot::Pane(id) => vec![*id],
            LayoutSnapshot::Split { first, second, .. } => {
                let mut ids = layout_ids(first);
                ids.extend(layout_ids(second));
                ids
            }
        }
    }

    fn pane_ids(tab: &TabSnapshot) -> HashSet<u32> {
        tab.panes.keys().copied().collect()
    }

    fn prune(snapshot: &mut SessionSnapshot) -> DroppedEndedPanes {
        prune_ended_agent_panes(snapshot, &mut None)
    }

    #[test]
    fn has_ended_agent_panes_sees_a_mark_anywhere_in_the_snapshot() {
        let marked = session(
            vec![
                workspace("w1", &[&[plain(1)]]),
                workspace("w2", &[&[plain(2)], &[plain(3), ended(4)]]),
            ],
            Some(0),
            0,
        );
        assert!(has_ended_agent_panes(&marked));
        let unmarked = session(
            vec![
                workspace("w1", &[&[plain(1)]]),
                workspace("w2", &[&[plain(2)], &[plain(3), plain(4)]]),
            ],
            Some(0),
            0,
        );
        assert!(!has_ended_agent_panes(&unmarked));
    }

    /// A user's own shell tabs carry no mark and must come out of the function exactly as they went in, whatever else is
    /// in the session, with the history untouched.
    #[test]
    fn a_session_without_marks_comes_out_exactly_as_it_went_in() {
        let mut snapshot = session(
            vec![
                workspace("w1", &[&[plain(1), plain(2)], &[plain(3)]]),
                workspace("w2", &[&[plain(4)]]),
            ],
            Some(1),
            1,
        );
        let fingerprint = layout_fingerprint(&snapshot);
        let mut history = Some(history_of(&snapshot, fingerprint));
        let before = serde_json::to_string(&snapshot).unwrap();
        let history_before = serde_json::to_string(history.as_ref().unwrap()).unwrap();

        let dropped = prune_ended_agent_panes(&mut snapshot, &mut history);

        assert_eq!(dropped, DroppedEndedPanes::default());
        assert_eq!(serde_json::to_string(&snapshot).unwrap(), before);
        assert_eq!(
            serde_json::to_string(history.as_ref().unwrap()).unwrap(),
            history_before
        );
    }

    #[test]
    fn a_marked_pane_of_a_split_leaves_its_neighbour_alone_in_a_plain_layout_with_focus_on_it() {
        let mut snapshot = session(vec![workspace("w1", &[&[ended(1), plain(2)]])], Some(0), 0);
        snapshot.workspaces[0].tabs[0].zoomed = true;

        let dropped = prune(&mut snapshot);

        let tab = &snapshot.workspaces[0].tabs[0];
        assert_eq!(pane_ids(tab), HashSet::from([2]));
        assert!(
            matches!(tab.layout, LayoutSnapshot::Pane(2)),
            "a split with one side gone must collapse into the pane that is left"
        );
        assert_eq!(tab.focused, Some(2), "the focus named the pane that went");
        assert_eq!(
            tab.root_pane,
            Some(2),
            "the root pane named the pane that went"
        );
        assert!(!tab.zoomed, "a zoom on a pane that went must not stay");
        assert_eq!(dropped.panes, 1);
        assert_eq!(dropped.tabs, 0);
        assert_eq!(dropped.workspaces, 0);
    }

    #[test]
    fn a_marked_pane_that_was_not_the_focused_one_leaves_focus_and_zoom_as_they_were() {
        let mut snapshot = session(vec![workspace("w1", &[&[plain(1), ended(2)]])], Some(0), 0);
        snapshot.workspaces[0].tabs[0].zoomed = true;

        prune(&mut snapshot);

        let tab = &snapshot.workspaces[0].tabs[0];
        assert_eq!(layout_ids(&tab.layout), vec![1]);
        assert_eq!(tab.focused, Some(1));
        assert!(tab.zoomed, "the zoomed pane is still there");
    }

    #[test]
    fn a_three_pane_tab_that_loses_its_middle_pane_keeps_the_order_of_the_other_two() {
        let mut snapshot = session(
            vec![workspace("w1", &[&[plain(1), ended(2), plain(3)]])],
            Some(0),
            0,
        );

        prune(&mut snapshot);

        let tab = &snapshot.workspaces[0].tabs[0];
        assert_eq!(layout_ids(&tab.layout), vec![1, 3]);
        assert_eq!(pane_ids(tab), HashSet::from([1, 3]));
    }

    #[test]
    fn the_active_tab_stays_the_same_tab_when_an_earlier_tab_goes() {
        let mut snapshot = session(
            vec![workspace("w1", &[&[ended(1)], &[plain(2)], &[plain(3)]])],
            Some(0),
            0,
        );
        snapshot.workspaces[0].active_tab = 2;

        let dropped = prune(&mut snapshot);

        let workspace = &snapshot.workspaces[0];
        assert_eq!(workspace.tabs.len(), 2);
        assert_eq!(
            workspace.active_tab, 1,
            "the old index 2 would now be past the end, or the wrong tab"
        );
        assert!(workspace.tabs[workspace.active_tab].panes.contains_key(&3));
        assert_eq!(dropped.tabs, 1);
        assert_eq!(dropped.panes, 1);
        assert_eq!(dropped.workspaces, 0);
    }

    #[test]
    fn the_surviving_tabs_keep_their_numbers_and_no_number_is_reissued() {
        let mut snapshot = session(
            vec![workspace("w1", &[&[ended(1)], &[plain(2)], &[plain(3)]])],
            Some(0),
            0,
        );

        prune(&mut snapshot);

        let workspace = &snapshot.workspaces[0];
        assert_eq!(workspace.public_tab_numbers, vec![2, 3]);
        assert_eq!(
            workspace.next_public_tab_number, 4,
            "a new tab must not take the number of the one that went"
        );
        assert_eq!(workspace.public_pane_numbers.get(&2), Some(&2));
        assert_eq!(workspace.public_pane_numbers.get(&3), Some(&3));
        assert!(!workspace.public_pane_numbers.contains_key(&1));
        assert_eq!(
            workspace.next_public_pane_number, 4,
            "a new pane must not take the number of the one that went"
        );
    }

    #[test]
    fn when_the_active_tab_goes_the_nearest_tab_before_it_is_active_else_the_first_after() {
        let mut snapshot = session(
            vec![workspace("w1", &[&[plain(1)], &[ended(2)], &[plain(3)]])],
            Some(0),
            0,
        );
        snapshot.workspaces[0].active_tab = 1;
        prune(&mut snapshot);
        assert_eq!(snapshot.workspaces[0].active_tab, 0);
        assert!(snapshot.workspaces[0].tabs[0].panes.contains_key(&1));

        let mut snapshot = session(
            vec![workspace("w1", &[&[ended(1)], &[ended(2)], &[plain(3)]])],
            Some(0),
            0,
        );
        snapshot.workspaces[0].active_tab = 1;
        prune(&mut snapshot);
        assert_eq!(snapshot.workspaces[0].active_tab, 0);
        assert!(snapshot.workspaces[0].tabs[0].panes.contains_key(&3));
    }

    #[test]
    fn an_active_tab_index_past_the_end_is_left_for_restore_to_clamp() {
        let mut snapshot = session(
            vec![workspace("w1", &[&[ended(1)], &[plain(2)]])],
            Some(0),
            0,
        );
        snapshot.workspaces[0].active_tab = 9;
        prune(&mut snapshot);
        assert_eq!(snapshot.workspaces[0].active_tab, 9);
    }

    #[test]
    fn a_workspace_whose_every_tab_is_marked_goes_and_the_indices_follow_the_one_that_stays() {
        let mut snapshot = session(
            vec![
                workspace("w1", &[&[ended(1)], &[ended(2), ended(3)]]),
                workspace("w2", &[&[plain(4)]]),
            ],
            Some(1),
            1,
        );

        let dropped = prune(&mut snapshot);

        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].id.as_deref(), Some("w2"));
        assert_eq!(snapshot.active, Some(0), "active would point past the end");
        assert_eq!(snapshot.selected, 0);
        assert_eq!(dropped.workspaces, 1);
        assert_eq!(dropped.tabs, 2);
        assert_eq!(dropped.panes, 3);
        assert_eq!(dropped.workspace_ids, vec!["w1".to_string()]);
    }

    #[test]
    fn when_the_selected_and_active_workspace_goes_the_nearest_one_before_it_is_kept() {
        let mut snapshot = session(
            vec![
                workspace("w1", &[&[plain(1)]]),
                workspace("w2", &[&[ended(2)]]),
                workspace("w3", &[&[plain(3)]]),
            ],
            Some(1),
            1,
        );
        prune(&mut snapshot);
        assert_eq!(snapshot.active, Some(0));
        assert_eq!(snapshot.selected, 0);

        let mut snapshot = session(
            vec![
                workspace("w1", &[&[ended(1)]]),
                workspace("w2", &[&[plain(2)]]),
            ],
            None,
            0,
        );
        prune(&mut snapshot);
        assert_eq!(snapshot.active, None, "no active workspace stays none");
        assert_eq!(snapshot.selected, 0);
    }

    #[test]
    fn a_session_whose_every_pane_is_marked_comes_out_empty() {
        let mut snapshot = session(
            vec![
                workspace("w1", &[&[ended(1), ended(2)]]),
                workspace("w2", &[&[ended(3)]]),
            ],
            Some(1),
            1,
        );
        let mut history = Some(history_of(&snapshot, layout_fingerprint(&snapshot)));

        let dropped = prune_ended_agent_panes(&mut snapshot, &mut history);

        assert!(snapshot.workspaces.is_empty());
        assert_eq!(snapshot.active, None);
        assert_eq!(snapshot.selected, 0);
        assert_eq!(dropped.workspaces, 2);
        assert_eq!(
            dropped.workspace_ids,
            vec!["w1".to_string(), "w2".to_string()]
        );
        assert!(history.is_none_or(|history| history.workspaces.is_empty()));
    }

    /// A file written before herdr saved public numbers has none. Restore numbers its panes in layout order, so the numbers
    /// must be filled in before anything is dropped, or a surviving pane's public id would depend on a pane that went.
    #[test]
    fn a_file_without_public_numbers_gives_every_survivor_the_number_restore_would_have_given_it() {
        let mut snapshot = session(
            vec![workspace("w1", &[&[ended(10), plain(11)], &[plain(12)]])],
            Some(0),
            0,
        );
        {
            let workspace = &mut snapshot.workspaces[0];
            workspace.public_pane_numbers.clear();
            workspace.next_public_pane_number = 0;
            workspace.public_tab_numbers.clear();
            workspace.next_public_tab_number = 0;
        }

        prune(&mut snapshot);

        let workspace = &snapshot.workspaces[0];
        assert_eq!(
            workspace.public_pane_numbers.get(&11),
            Some(&2),
            "pane 11 was the second pane in layout order"
        );
        assert_eq!(workspace.public_pane_numbers.get(&12), Some(&3));
        assert!(workspace.next_public_pane_number >= 4);
        assert_eq!(workspace.public_tab_numbers, vec![1, 2]);
        assert!(workspace.next_public_tab_number >= 3);
    }

    #[test]
    fn a_file_without_tab_numbers_keeps_the_number_of_a_tab_whose_earlier_neighbour_goes() {
        let mut snapshot = session(
            vec![workspace("w1", &[&[ended(1)], &[plain(2)]])],
            Some(0),
            0,
        );
        snapshot.workspaces[0].public_tab_numbers.clear();
        snapshot.workspaces[0].next_public_tab_number = 0;

        prune(&mut snapshot);

        assert_eq!(snapshot.workspaces[0].public_tab_numbers, vec![2]);
    }

    /// The history file is positional. Pruned in step with the layout and stamped with the pruned layout's fingerprint, it
    /// still belongs to the layout; without the stamp every kept pane comes back with an empty screen.
    #[test]
    fn the_history_is_pruned_with_the_layout_and_stamped_with_the_pruned_layouts_fingerprint() {
        let mut snapshot = session(
            vec![
                workspace("w1", &[&[plain(1), ended(2), plain(3)]]),
                workspace("w2", &[&[ended(4)]]),
                workspace("w3", &[&[plain(5)], &[ended(6)]]),
            ],
            Some(0),
            0,
        );
        let original_fingerprint = layout_fingerprint(&snapshot);
        let mut history = Some(history_of(&snapshot, original_fingerprint.clone()));
        assert_eq!(
            match_history_to_layout(history.as_ref().unwrap(), &snapshot),
            HistoryLayoutMatch::Fingerprint,
            "the fixture's own history must match before anything is pruned"
        );

        prune_ended_agent_panes(&mut snapshot, &mut history);

        let history = history.expect("a history that belonged to the layout is kept");
        assert_eq!(
            match_history_to_layout(&history, &snapshot),
            HistoryLayoutMatch::Fingerprint,
            "the history must carry the fingerprint of the pruned layout"
        );
        assert_ne!(history.layout_fingerprint, original_fingerprint);
        assert_eq!(history.workspaces.len(), 2);
        let first: HashSet<u32> = history.workspaces[0].tabs[0]
            .panes
            .keys()
            .copied()
            .collect();
        assert_eq!(first, HashSet::from([1, 3]), "the entry of pane 2 must go");
        assert_eq!(
            history.workspaces[0].tabs[0].panes[&1].ansi, "screen of pane 1\r\n",
            "a kept pane keeps its own screen"
        );
        assert_eq!(history.workspaces[1].tabs.len(), 1);
        assert_eq!(
            history.workspaces[1].tabs[0].panes[&5].ansi,
            "screen of pane 5\r\n"
        );
    }

    #[test]
    fn a_history_without_a_fingerprint_still_fits_the_pruned_layout() {
        let mut snapshot = session(
            vec![
                workspace("w1", &[&[plain(1), ended(2)]]),
                workspace("w2", &[&[ended(3)]]),
            ],
            Some(0),
            0,
        );
        let mut history = Some(history_of(&snapshot, None));
        assert_eq!(
            match_history_to_layout(history.as_ref().unwrap(), &snapshot),
            HistoryLayoutMatch::LegacyStructure
        );

        prune_ended_agent_panes(&mut snapshot, &mut history);

        let history = history.expect("a history that fits the layout is kept");
        assert_eq!(
            history.layout_fingerprint, None,
            "a stamp the file never had"
        );
        assert_eq!(
            match_history_to_layout(&history, &snapshot),
            HistoryLayoutMatch::LegacyStructure,
            "an entry left for a pane that went makes the structure not fit"
        );
    }

    /// Screens of another layout must never be shown in the wrong pane: a history that does not belong to the saved layout is
    /// dropped whole, never pruned and kept.
    #[test]
    fn a_history_that_does_not_belong_to_the_layout_is_dropped_whole() {
        for foreign in [Some("the fingerprint of another layout".to_string()), None] {
            let mut snapshot = session(vec![workspace("w1", &[&[plain(1), ended(2)]])], Some(0), 0);
            let other = session(
                vec![workspace("w1", &[&[plain(1), plain(2), plain(99)]])],
                Some(0),
                0,
            );
            let mut history = Some(history_of(&other, foreign.clone()));
            assert_eq!(
                match_history_to_layout(history.as_ref().unwrap(), &snapshot),
                HistoryLayoutMatch::Mismatch,
                "{foreign:?}: the fixture must be a mismatch"
            );

            prune_ended_agent_panes(&mut snapshot, &mut history);

            assert!(
                history.is_none(),
                "{foreign:?}: a mismatching history was kept"
            );
            assert_eq!(layout_ids(&snapshot.workspaces[0].tabs[0].layout), vec![1]);
        }
    }

    #[test]
    fn no_history_stays_no_history() {
        let mut snapshot = session(vec![workspace("w1", &[&[plain(1), ended(2)]])], Some(0), 0);
        let mut history = None;
        prune_ended_agent_panes(&mut snapshot, &mut history);
        assert!(history.is_none());
        assert_eq!(layout_ids(&snapshot.workspaces[0].tabs[0].layout), vec![1]);
    }

    /// Only the workspaces that lose something are touched: another workspace's layout, numbers and history entries stay
    /// byte for byte what the file held.
    #[test]
    fn a_workspace_that_loses_nothing_is_not_touched() {
        let mut snapshot = session(
            vec![
                workspace("w1", &[&[plain(1), plain(2)]]),
                workspace("w2", &[&[ended(3), plain(4)]]),
            ],
            Some(0),
            0,
        );
        snapshot.workspaces[0].public_pane_numbers.clear();
        snapshot.workspaces[0].next_public_pane_number = 0;
        let untouched = serde_json::to_string(&snapshot.workspaces[0]).unwrap();

        prune(&mut snapshot);

        assert_eq!(
            serde_json::to_string(&snapshot.workspaces[0]).unwrap(),
            untouched,
            "restore fills in the numbers of a workspace it keeps whole; the prune must not"
        );
    }
}
