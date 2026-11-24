//! Leaving panes whose agent ended out of a restore (`[session] skip_ended_agent_panes_on_restore`).
//!
//! A save with the setting on marks a pane whose agent ended and whose shell is idle (`PaneSnapshot::agent_ended`). The start
//! that reads such a file runs `prune_ended_agent_panes` over the loaded snapshot and the loaded pane history, before the
//! unchanged `restore`, so everything `restore` indexes by position (the history, `active`, `selected`, `active_tab`) is fixed in
//! one place.

use std::collections::HashSet;

use tracing::{info, warn};

use super::snapshot::{
    layout_fingerprint, match_history_to_layout, HistoryLayoutMatch, LayoutSnapshot,
    SessionHistorySnapshot, SessionSnapshot, TabSnapshot, WorkspaceHistorySnapshot,
    WorkspaceSnapshot,
};

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
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .flat_map(|tab| tab.panes.values())
        .any(|pane| pane.agent_ended)
}

/// Removes every marked pane from the snapshot, and from the history in step with it: the layout of a tab that loses a pane,
/// the tab that loses all of them, the workspace that loses every tab, and the indices that pointed past them.
///
/// A history that does not belong to the saved layout (`match_history_to_layout` says `Mismatch`, or its shape differs) is
/// dropped whole: it would be ignored by the restore anyway, and screens of another layout must never be shown in a pane. One
/// that belongs to it is pruned the same way and, when it carried a layout fingerprint, stamped with the fingerprint of the
/// pruned layout, so the restore still finds it matching. A workspace that loses nothing is left exactly as it was.
pub fn prune_ended_agent_panes(
    snapshot: &mut SessionSnapshot,
    history: &mut Option<SessionHistorySnapshot>,
) -> DroppedEndedPanes {
    let mut dropped = DroppedEndedPanes::default();
    if !has_ended_agent_panes(snapshot) {
        return dropped;
    }

    // Decided on the layout as it was saved: the history was written together with it.
    let history_fits = history.as_ref().is_some_and(|history| {
        match_history_to_layout(history, snapshot) != HistoryLayoutMatch::Mismatch
            && history_has_the_shape_of(history, snapshot)
    });
    if history.is_some() && !history_fits {
        warn!("Ignoring pane history without a matching session layout");
        *history = None;
    }
    let had_fingerprint = history
        .as_ref()
        .is_some_and(|history| history.layout_fingerprint.is_some());
    let history_version = history.as_ref().map(|history| history.version);
    let mut history_workspaces = history.take().map(|history| history.workspaces.into_iter());
    let mut kept_history_workspaces = Vec::new();

    let old_workspaces = std::mem::take(&mut snapshot.workspaces);
    let mut workspace_survives = Vec::with_capacity(old_workspaces.len());
    let mut kept_workspaces = Vec::new();
    for workspace in old_workspaces {
        let history_workspace = history_workspaces
            .as_mut()
            .and_then(std::iter::Iterator::next);
        let loses_something = workspace
            .tabs
            .iter()
            .any(|tab| tab.panes.values().any(|pane| pane.agent_ended));
        if !loses_something {
            workspace_survives.push(true);
            kept_workspaces.push(workspace);
            kept_history_workspaces.extend(history_workspace);
            continue;
        }
        match prune_workspace(workspace, history_workspace, &mut dropped) {
            (Some(workspace), history_workspace) => {
                workspace_survives.push(true);
                kept_workspaces.push(workspace);
                kept_history_workspaces.extend(history_workspace);
            }
            (None, _) => workspace_survives.push(false),
        }
    }

    snapshot.workspaces = kept_workspaces;
    if snapshot.workspaces.is_empty() {
        snapshot.active = None;
        snapshot.selected = 0;
    } else {
        snapshot.active = snapshot
            .active
            .map(|active| remap_index(active, &workspace_survives));
        snapshot.selected = remap_index(snapshot.selected, &workspace_survives);
    }

    if let Some(version) = history_version {
        *history = Some(SessionHistorySnapshot {
            version,
            layout_fingerprint: if had_fingerprint {
                layout_fingerprint(snapshot)
            } else {
                None
            },
            workspaces: kept_history_workspaces,
        });
    }
    dropped
}

/// The history has an entry for every workspace and every tab of the snapshot, in the same positions.
fn history_has_the_shape_of(history: &SessionHistorySnapshot, snapshot: &SessionSnapshot) -> bool {
    history.workspaces.len() == snapshot.workspaces.len()
        && history
            .workspaces
            .iter()
            .zip(&snapshot.workspaces)
            .all(|(history, workspace)| history.tabs.len() == workspace.tabs.len())
}

/// One workspace that holds a marked pane. Returns it without them, or `None` when no tab is left, with its history.
fn prune_workspace(
    mut workspace: WorkspaceSnapshot,
    history: Option<WorkspaceHistorySnapshot>,
    dropped: &mut DroppedEndedPanes,
) -> (Option<WorkspaceSnapshot>, Option<WorkspaceHistorySnapshot>) {
    // Restore numbers an old file's panes and tabs in layout order. Once a pane or a tab is gone the numbers of those after
    // it would shift, so every number is written down first.
    write_down_public_numbers(&mut workspace);

    let workspace_label = workspace.id.clone().unwrap_or_default();
    let old_tabs = std::mem::take(&mut workspace.tabs);
    let old_tab_numbers = std::mem::take(&mut workspace.public_tab_numbers);
    let mut history_tabs = history.map(|history| history.tabs.into_iter());
    let mut kept_history_tabs = Vec::new();
    let mut tab_survives = Vec::with_capacity(old_tabs.len());
    let mut kept_tabs = Vec::new();
    let mut kept_tab_numbers = Vec::new();
    let mut dropped_pane_ids: HashSet<u32> = HashSet::new();

    for (tab_index, (mut tab, tab_number)) in old_tabs.into_iter().zip(old_tab_numbers).enumerate()
    {
        let history_tab = history_tabs.as_mut().and_then(std::iter::Iterator::next);
        let mut ended: Vec<u32> = tab
            .panes
            .iter()
            .filter(|(_, pane)| pane.agent_ended)
            .map(|(id, _)| *id)
            .collect();
        ended.sort_unstable();
        for id in &ended {
            let pane = &tab.panes[id];
            info!(
                event = "persist.restore.skip_ended_pane",
                workspace = %workspace_label,
                tab = tab_index,
                cwd = %pane.cwd.display(),
                agent_session = pane.agent_session.as_ref().map(|session| session.value.as_str()),
                "leaving out a pane whose agent ended and left its shell idle"
            );
        }
        dropped.panes += ended.len();
        let ended: HashSet<u32> = ended.into_iter().collect();
        dropped_pane_ids.extend(&ended);

        if ended.is_empty() {
            tab_survives.push(true);
            kept_tabs.push(tab);
            kept_tab_numbers.push(tab_number);
            kept_history_tabs.extend(history_tab);
            continue;
        }
        let Some(layout) = prune_layout(
            std::mem::replace(&mut tab.layout, LayoutSnapshot::Pane(0)),
            &ended,
        ) else {
            dropped.tabs += 1;
            tab_survives.push(false);
            continue;
        };
        tab.layout = layout;
        tab.panes.retain(|id, _| !ended.contains(id));
        settle_focus(&mut tab, &ended);
        let mut history_tab = history_tab;
        if let Some(history_tab) = history_tab.as_mut() {
            history_tab.panes.retain(|id, _| !ended.contains(id));
        }
        tab_survives.push(true);
        kept_tabs.push(tab);
        kept_tab_numbers.push(tab_number);
        kept_history_tabs.extend(history_tab);
    }

    if kept_tabs.is_empty() {
        dropped.workspaces += 1;
        dropped.workspace_ids.extend(workspace.id.clone());
        return (None, None);
    }
    workspace.active_tab = remap_index(workspace.active_tab, &tab_survives);
    workspace.tabs = kept_tabs;
    workspace.public_tab_numbers = kept_tab_numbers;
    workspace
        .public_pane_numbers
        .retain(|id, _| !dropped_pane_ids.contains(id));
    let history = history_tabs.is_some().then_some(WorkspaceHistorySnapshot {
        tabs: kept_history_tabs,
    });
    (Some(workspace), history)
}

/// Gives every pane and every tab of the workspace the public number restore would give it, and raises the next numbers past
/// all of them, so that a number that belonged to a pane or a tab that goes is not handed out again.
fn write_down_public_numbers(workspace: &mut WorkspaceSnapshot) {
    let mut next_pane_number = workspace
        .public_pane_numbers
        .values()
        .copied()
        .max()
        .and_then(|max| max.checked_add(1))
        .unwrap_or(1)
        .max(workspace.next_public_pane_number);
    let pane_numbers =
        super::restore::migrated_public_pane_numbers_by_old_raw(workspace, &mut next_pane_number);
    workspace.public_pane_numbers = pane_numbers;
    workspace.next_public_pane_number = next_pane_number;

    let tab_numbers: Vec<usize> = (0..workspace.tabs.len())
        .map(|index| {
            workspace
                .public_tab_numbers
                .get(index)
                .copied()
                .unwrap_or(index + 1)
        })
        .collect();
    workspace.next_public_tab_number = tab_numbers
        .iter()
        .copied()
        .max()
        .and_then(|max| max.checked_add(1))
        .unwrap_or(1)
        .max(workspace.next_public_tab_number);
    workspace.public_tab_numbers = tab_numbers;
}

/// The layout without the panes in `ended`; a split with one side gone becomes the pane that is left.
fn prune_layout(node: LayoutSnapshot, ended: &HashSet<u32>) -> Option<LayoutSnapshot> {
    match node {
        LayoutSnapshot::Pane(id) => (!ended.contains(&id)).then_some(LayoutSnapshot::Pane(id)),
        LayoutSnapshot::Split {
            direction,
            ratio,
            first,
            second,
        } => match (prune_layout(*first, ended), prune_layout(*second, ended)) {
            (Some(first), Some(second)) => Some(LayoutSnapshot::Split {
                direction,
                ratio,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
            (None, None) => None,
        },
    }
}

fn first_pane_in(node: &LayoutSnapshot) -> Option<u32> {
    match node {
        LayoutSnapshot::Pane(id) => Some(*id),
        LayoutSnapshot::Split { first, second, .. } => {
            first_pane_in(first).or_else(|| first_pane_in(second))
        }
    }
}

/// A focus or root pane that named a pane that went falls to the first pane left, in layout order (what restore does for a
/// saved id it cannot find), and a zoom on a pane that went is undone.
fn settle_focus(tab: &mut TabSnapshot, ended: &HashSet<u32>) {
    let fallback = first_pane_in(&tab.layout);
    if tab.focused.is_some_and(|id| ended.contains(&id)) {
        tab.focused = fallback;
        tab.zoomed = false;
    }
    if tab.root_pane.is_some_and(|id| ended.contains(&id)) {
        tab.root_pane = fallback;
    }
}

/// Where the item at `index` is after the items that do not survive are gone: the same item when it survives, otherwise the
/// nearest survivor before it, otherwise the first one after it. An index past the end is not an item, so it is left for
/// restore to clamp as it always did.
fn remap_index(index: usize, survives: &[bool]) -> usize {
    if index >= survives.len() {
        return index;
    }
    let target = if survives[index] {
        Some(index)
    } else {
        (0..index)
            .rev()
            .find(|candidate| survives[*candidate])
            .or_else(|| (index + 1..survives.len()).find(|candidate| survives[*candidate]))
    };
    target.map_or(0, |target| {
        survives[..target]
            .iter()
            .filter(|survives| **survives)
            .count()
    })
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

    use super::super::snapshot::{PaneAgentResumeSnapshot, PaneAgentSessionSnapshot};

    /// The saved session of an agent: a pane that still holds one is a pane the next start would resume.
    fn give_session(snapshot: &mut SessionSnapshot, workspace: usize, tab: usize, id: u32) {
        snapshot.workspaces[workspace].tabs[tab]
            .panes
            .get_mut(&id)
            .unwrap()
            .agent_session = Some(PaneAgentSessionSnapshot {
            source: "herdr:pi".to_string(),
            agent: "pi".to_string(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: format!("/sessions/{id}.jsonl"),
        });
    }

    /// The command a custom reporter gave for resuming: the other way a pane holds on to its agent.
    fn give_resume(snapshot: &mut SessionSnapshot, workspace: usize, tab: usize, id: u32) {
        snapshot.workspaces[workspace].tabs[tab]
            .panes
            .get_mut(&id)
            .unwrap()
            .agent_resume = Some(PaneAgentResumeSnapshot {
            source: "custom:reporter".to_string(),
            agent: "pi".to_string(),
            argv: vec!["agent".to_string(), "--resume".to_string()],
        });
    }

    /// A marked pane that still holds an agent session is a resume the next start would make, not a leftover: a failed start
    /// keeps its session so the next start retries it, and this pass must never turn one resume failure across the fleet into
    /// every pane gone. Only a mark with nothing to resume is acted on.
    #[test]
    fn has_ended_agent_panes_ignores_a_mark_on_a_pane_that_holds_a_session_or_a_resume() {
        let build = || session(vec![workspace("w1", &[&[plain(1), ended(2)]])], Some(0), 0);
        let mut with_session = build();
        give_session(&mut with_session, 0, 0, 2);
        assert!(!has_ended_agent_panes(&with_session));
        let mut with_resume = build();
        give_resume(&mut with_resume, 0, 0, 2);
        assert!(!has_ended_agent_panes(&with_resume));
        assert!(has_ended_agent_panes(&build()));
    }

    #[test]
    fn a_marked_pane_that_holds_an_agent_session_is_kept_and_the_file_is_left_exactly_as_it_was() {
        let mut snapshot = session(
            vec![
                workspace("w1", &[&[plain(1), ended(2)]]),
                workspace("w2", &[&[plain(3)]]),
            ],
            Some(0),
            0,
        );
        give_session(&mut snapshot, 0, 0, 2);
        let fingerprint = layout_fingerprint(&snapshot);
        let mut history = Some(history_of(&snapshot, fingerprint));
        let before = serde_json::to_string(&snapshot).unwrap();
        let history_before = serde_json::to_string(history.as_ref().unwrap()).unwrap();

        let dropped = prune_ended_agent_panes(&mut snapshot, &mut history);

        assert_eq!(dropped.panes, 0, "a pane that still holds its session went");
        assert_eq!(serde_json::to_string(&snapshot).unwrap(), before);
        assert_eq!(
            serde_json::to_string(history.as_ref().unwrap()).unwrap(),
            history_before,
            "nothing was dropped, so the history is not touched"
        );
    }

    #[test]
    fn a_marked_pane_that_holds_a_reported_resume_is_kept() {
        let mut snapshot = session(vec![workspace("w1", &[&[plain(1), ended(2)]])], Some(0), 0);
        give_resume(&mut snapshot, 0, 0, 2);

        let dropped = prune(&mut snapshot);

        assert_eq!(dropped.panes, 0, "a pane that still holds a resume went");
        assert_eq!(
            pane_ids(&snapshot.workspaces[0].tabs[0]),
            HashSet::from([1, 2])
        );
    }

    /// The kept pane and the one that goes sit in one tab: only the one with nothing to resume goes, and the layout, focus and
    /// numbers follow it.
    #[test]
    fn a_marked_pane_with_nothing_to_resume_still_goes_beside_a_marked_pane_that_holds_a_session() {
        let mut snapshot = session(vec![workspace("w1", &[&[ended(1), ended(2)]])], Some(0), 0);
        give_session(&mut snapshot, 0, 0, 1);

        let dropped = prune(&mut snapshot);

        assert_eq!(dropped.panes, 1);
        let tab = &snapshot.workspaces[0].tabs[0];
        assert_eq!(pane_ids(tab), HashSet::from([1]));
        assert_eq!(layout_ids(&tab.layout), vec![1]);
        assert_eq!(tab.focused, Some(1));
    }

    /// A tab whose only marked pane holds a session is not an empty tab and a workspace whose every pane is such a pane is not
    /// an empty workspace.
    #[test]
    fn a_tab_and_a_workspace_whose_marked_panes_all_hold_a_session_stay() {
        let mut snapshot = session(
            vec![
                workspace("w1", &[&[ended(1)], &[ended(2)]]),
                workspace("w2", &[&[plain(3)]]),
            ],
            Some(0),
            0,
        );
        give_session(&mut snapshot, 0, 0, 1);
        give_session(&mut snapshot, 0, 1, 2);

        let dropped = prune(&mut snapshot);

        assert_eq!(dropped, DroppedEndedPanes::default());
        assert_eq!(snapshot.workspaces.len(), 2);
        assert_eq!(snapshot.workspaces[0].tabs.len(), 2);
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
