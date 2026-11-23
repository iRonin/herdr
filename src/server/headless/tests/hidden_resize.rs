//! `[ui] resize_hidden_panes`: which panes the server resizes when a client's pane area changes.
//!
//! A pane is "resized" here when its size really changes, which is exactly when the kernel signals
//! its program (`PaneRuntime::resize` returns early for an unchanged size). Every real resize
//! writes to the runtime's content sequence, so `Panes::resizes` counts them. Each test starts the
//! hidden panes at a size no later event can produce, so a zero means "never asked", not "asked
//! for the size it already had".
use super::*;
use crate::api::schema::{
    Method, PaneMoveDestination, PaneMoveParams, Request, SplitDirection, TabMoveToWorkspaceParams,
    TabTarget, WorkspaceTarget, WorktreeOpenParams,
};

/// Rows and columns every test pane starts with.
const START_SIZE: (u16, u16) = (24, 80);

struct Panes {
    shown: crate::layout::PaneId,
    hidden_tab: crate::layout::PaneId,
    hidden_workspace: crate::layout::PaneId,
    shown_tab_id: String,
    hidden_tab_id: String,
    hidden_workspace_id: String,
    /// What one real resize adds to a runtime's content sequence, measured on a spare runtime.
    sequence_per_resize: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Resizes {
    shown: u64,
    hidden_tab: u64,
    hidden_workspace: u64,
}

const NONE: Resizes = Resizes {
    shown: 0,
    hidden_tab: 0,
    hidden_workspace: 0,
};

impl Panes {
    fn runtime<'a>(
        &self,
        server: &'a HeadlessServer,
        workspace: usize,
        pane: crate::layout::PaneId,
    ) -> &'a crate::terminal::TerminalRuntime {
        &server.app.state.workspaces[workspace].test_runtimes[&pane]
    }

    fn total(&self, server: &HeadlessServer) -> Resizes {
        let count = |workspace, pane| self.runtime(server, workspace, pane).content_seq();
        Resizes {
            shown: count(0, self.shown),
            hidden_tab: count(0, self.hidden_tab),
            hidden_workspace: count(1, self.hidden_workspace),
        }
    }

    /// The real resizes of each pane since `before` was taken with `total`.
    fn resizes_since(&self, server: &HeadlessServer, before: Resizes) -> Resizes {
        let after = self.total(server);
        let per = self.sequence_per_resize;
        for (name, before, after) in [
            ("shown", before.shown, after.shown),
            ("hidden tab", before.hidden_tab, after.hidden_tab),
            (
                "hidden workspace",
                before.hidden_workspace,
                after.hidden_workspace,
            ),
        ] {
            assert_eq!(
                (after - before) % per,
                0,
                "{name} pane: the content sequence moved by something other than whole resizes"
            );
        }
        Resizes {
            shown: (after.shown - before.shown) / per,
            hidden_tab: (after.hidden_tab - before.hidden_tab) / per,
            hidden_workspace: (after.hidden_workspace - before.hidden_workspace) / per,
        }
    }

    fn sizes(&self, server: &HeadlessServer) -> [(u16, u16); 3] {
        [
            self.runtime(server, 0, self.shown).current_size(),
            self.runtime(server, 0, self.hidden_tab).current_size(),
            self.runtime(server, 1, self.hidden_workspace)
                .current_size(),
        ]
    }
}

/// Workspace "shown" with two tabs (the first is shown), and a second workspace "hidden workspace".
fn hidden_resize_server(resize_hidden_panes: bool) -> (HeadlessServer, Panes) {
    let mut server = test_headless_server();
    server.app.state.resize_hidden_panes = resize_hidden_panes;
    let mut first = crate::workspace::Workspace::test_new("shown");
    let shown = first.tabs[0].root_pane;
    let hidden_tab_index = first.test_add_tab(Some("hidden tab"));
    let hidden_tab = first.tabs[hidden_tab_index].root_pane;
    first.switch_tab(0);
    let mut second = crate::workspace::Workspace::test_new("hidden workspace");
    let hidden_workspace = second.tabs[0].root_pane;
    first.insert_test_runtime(
        shown,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(START_SIZE.1, START_SIZE.0, b""),
    );
    first.insert_test_runtime(
        hidden_tab,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(START_SIZE.1, START_SIZE.0, b""),
    );
    second.insert_test_runtime(
        hidden_workspace,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(START_SIZE.1, START_SIZE.0, b""),
    );
    server.app.state.workspaces = vec![first, second];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let spare =
        crate::terminal::TerminalRuntime::test_with_screen_bytes(START_SIZE.1, START_SIZE.0, b"");
    let before = spare.content_seq();
    spare.resize(START_SIZE.0 + 5, START_SIZE.1 + 7, 0, 0);
    let sequence_per_resize = spare.content_seq() - before;
    assert!(
        sequence_per_resize > 0,
        "a real resize must move the sequence"
    );
    spare.resize(START_SIZE.0 + 5, START_SIZE.1 + 7, 0, 0);
    assert_eq!(
        spare.content_seq() - before,
        sequence_per_resize,
        "instrument: a resize to the size a pane already has must not move the sequence"
    );
    spare.shutdown();

    let panes = Panes {
        shown,
        hidden_tab,
        hidden_workspace,
        shown_tab_id: server.app.public_tab_id(0, 0).unwrap(),
        hidden_tab_id: server.app.public_tab_id(0, hidden_tab_index).unwrap(),
        hidden_workspace_id: server.app.public_workspace_id(1),
        sequence_per_resize,
    };
    (server, panes)
}

type Receivers = (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
);

fn attach(server: &mut HeadlessServer, client_id: u64, cols: u16, rows: u16) -> Receivers {
    let (control, render) = connect_test_shell(server, client_id, cols, rows);
    let _ = control.recv().expect("snapshot");
    (control, render)
}

/// The client reports a new pane area: a window resize, a sidebar toggle and a change of the tab
/// bar's height all arrive at the server as exactly this message.
fn client_pane_area(server: &mut HeadlessServer, client_id: u64, cols: u16, rows: u16) {
    assert!(server.handle_server_event(ServerEvent::ClientShellResize {
        client_id,
        surface_cols: cols,
        surface_rows: rows,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    }));
}

fn api_request(method: Method) -> crate::api::ApiRequestMessage {
    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    crate::api::ApiRequestMessage {
        request: Request {
            id: "hidden-resize".into(),
            method,
        },
        peer_process: None,
        respond_to,
        response_write_complete: None,
    }
}

/// A client's own navigation request, as its tab bar or sidebar sends it.
fn client_shows_tab(server: &mut HeadlessServer, client_id: u64, tab_id: &str) {
    server.handle_client_shell_api_request(
        client_id,
        api_request(Method::TabFocus(TabTarget {
            tab_id: tab_id.to_owned(),
        })),
    );
}

/// A tab focus on the public socket, as `herdr tab focus` sends it.
fn socket_shows_tab(server: &mut HeadlessServer, tab_id: &str) {
    server.handle_api_request_with_shutdown_check(api_request(Method::TabFocus(TabTarget {
        tab_id: tab_id.to_owned(),
    })));
}

fn socket_shows_workspace(server: &mut HeadlessServer, workspace_id: &str) {
    server.handle_api_request_with_shutdown_check(api_request(Method::WorkspaceFocus(
        WorkspaceTarget {
            workspace_id: workspace_id.to_owned(),
        },
    )));
}

/// The pane-area changes of one client, each with the event it stands for.
const AREA_EVENTS: [(&str, u16, u16); 3] = [
    ("the window is resized", 60, 16),
    ("the sidebar is collapsed, the pane area gets wider", 82, 16),
    ("the tab bar gets a row, the pane area gets shorter", 82, 15),
];

/// Counter for the instrument itself: a pane the server resizes to a size it already has must not
/// count, or every zero below would be worth nothing.
#[tokio::test]
async fn the_resize_counter_counts_only_resizes_that_change_a_size() {
    let (mut server, panes) = hidden_resize_server(true);
    let before = panes.total(&server);
    let runtime = panes.runtime(&server, 0, panes.shown);
    runtime.resize(START_SIZE.0, START_SIZE.1, 0, 0);
    assert_eq!(panes.resizes_since(&server, before), NONE);
    runtime.resize(START_SIZE.0 + 1, START_SIZE.1, 0, 0);
    assert_eq!(
        panes.resizes_since(&server, before),
        Resizes { shown: 1, ..NONE }
    );
    shutdown_test_runtimes(&mut server);
}

/// Control: with the setting on (upstream's behaviour, the default) a client's pane area reaches
/// every pane of every tab and workspace.
#[tokio::test]
async fn on_every_pane_follows_the_clients_pane_area() {
    let (mut server, panes) = hidden_resize_server(true);
    let (_control, _render) = attach(&mut server, 1, 100, 30);
    let attached = panes.sizes(&server);
    assert_ne!(attached[0], START_SIZE, "the attach changes the shown pane");
    assert_eq!(
        attached[1], attached[0],
        "the hidden tab is sized with the shown one"
    );
    assert_eq!(
        attached[2], attached[0],
        "the hidden workspace is sized with the shown one"
    );

    for (event, cols, rows) in AREA_EVENTS {
        let before = panes.total(&server);
        client_pane_area(&mut server, 1, cols, rows);
        assert_eq!(
            panes.resizes_since(&server, before),
            Resizes {
                shown: 1,
                hidden_tab: 1,
                hidden_workspace: 1
            },
            "{event}"
        );
        let sizes = panes.sizes(&server);
        assert_eq!(sizes[1], sizes[0], "{event}");
        assert_eq!(sizes[2], sizes[0], "{event}");
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn off_a_pane_area_change_resizes_only_the_shown_tab() {
    let (mut server, panes) = hidden_resize_server(false);
    let (_control, _render) = attach(&mut server, 1, 100, 30);
    let attached = panes.sizes(&server);
    assert_ne!(attached[0], START_SIZE, "the attach changes the shown pane");
    assert_eq!(
        attached[1], START_SIZE,
        "attach: the hidden tab is left alone"
    );
    assert_eq!(
        attached[2], START_SIZE,
        "attach: the hidden workspace is left alone"
    );

    for (event, cols, rows) in AREA_EVENTS {
        let before = panes.total(&server);
        let shown_before = panes.sizes(&server)[0];
        client_pane_area(&mut server, 1, cols, rows);
        let sizes = panes.sizes(&server);
        assert_ne!(
            sizes[0], shown_before,
            "{event}: guard, the shown pane really changes size"
        );
        assert_ne!(
            sizes[0], START_SIZE,
            "{event}: guard, the hidden panes would change if they were resized"
        );
        assert_eq!(
            panes.resizes_since(&server, before),
            Resizes { shown: 1, ..NONE },
            "{event}"
        );
        assert_eq!(
            sizes[1], START_SIZE,
            "{event}: the hidden tab keeps its size"
        );
        assert_eq!(
            sizes[2], START_SIZE,
            "{event}: the hidden workspace keeps its size"
        );
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn off_a_hidden_tab_and_a_hidden_workspace_are_resized_once_when_shown() {
    let (mut server, panes) = hidden_resize_server(false);
    let (_control, _render) = attach(&mut server, 1, 100, 30);
    client_pane_area(&mut server, 1, 60, 16);
    let current = panes.sizes(&server)[0];
    assert_ne!(current, START_SIZE);
    assert_eq!(
        panes.sizes(&server)[1],
        START_SIZE,
        "guard: the tab is stale"
    );

    // The client shows the hidden tab: one resize, to what a shown tab has.
    let before = panes.total(&server);
    client_shows_tab(&mut server, 1, &panes.hidden_tab_id);
    assert_eq!(
        panes.resizes_since(&server, before),
        Resizes {
            hidden_tab: 1,
            ..NONE
        },
        "the client shows the hidden tab"
    );
    assert_eq!(panes.sizes(&server)[1], current);

    // Back to the first tab, whose size never changed while it was hidden: no resize at all.
    let before = panes.total(&server);
    socket_shows_tab(&mut server, &panes.shown_tab_id);
    assert_eq!(
        panes.resizes_since(&server, before),
        NONE,
        "the socket shows the first tab again"
    );

    // And once more to the tab that was resized when it was first shown: still no resize.
    let before = panes.total(&server);
    socket_shows_tab(&mut server, &panes.hidden_tab_id);
    assert_eq!(
        panes.resizes_since(&server, before),
        NONE,
        "the socket shows the hidden tab a second time"
    );

    // A hidden workspace.
    assert_eq!(panes.sizes(&server)[2], START_SIZE, "guard: it is stale");
    let before = panes.total(&server);
    socket_shows_workspace(&mut server, &panes.hidden_workspace_id);
    assert_eq!(
        panes.resizes_since(&server, before),
        Resizes {
            hidden_workspace: 1,
            ..NONE
        },
        "the socket shows the hidden workspace"
    );
    assert_eq!(panes.sizes(&server)[2], current);
    shutdown_test_runtimes(&mut server);
}

/// Two clients of different sizes, the one that controls the shown tab leaves. The client that
/// stays has its size applied to what it shows. With the setting on it is applied to every tab,
/// including tabs this client never showed and never resized.
fn second_client_leaves(resize_hidden_panes: bool) -> (Resizes, [(u16, u16); 3]) {
    let (mut server, panes) = hidden_resize_server(resize_hidden_panes);
    let (_first_control, _first_render) = attach(&mut server, 1, 100, 30);
    let (_second_control, _second_render) = attach(&mut server, 2, 70, 20);
    // The second client's focus report makes it the controller of the tab it shows.
    assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id: 2,
        focused: true,
    }));
    // The first client is no longer the controller: its resize reaches no tab.
    let before = panes.total(&server);
    client_pane_area(&mut server, 1, 90, 26);
    assert_eq!(
        panes.resizes_since(&server, before),
        NONE,
        "guard: a client that does not control the shown tab resizes nothing"
    );
    let shown_with_second = panes.sizes(&server)[0];

    let before = panes.total(&server);
    server.remove_client_and_resize_if_needed(2);
    let resizes = panes.resizes_since(&server, before);
    let sizes = panes.sizes(&server);
    assert_ne!(
        sizes[0], shown_with_second,
        "the shown tab goes to the size of the client that stays"
    );
    shutdown_test_runtimes(&mut server);
    (resizes, sizes)
}

#[tokio::test]
async fn on_a_departing_client_brings_every_tab_to_the_remaining_clients_size() {
    let (resizes, sizes) = second_client_leaves(true);
    assert_eq!(
        resizes,
        Resizes {
            shown: 1,
            hidden_tab: 1,
            hidden_workspace: 1
        }
    );
    assert_eq!(sizes[1], sizes[0]);
    assert_eq!(sizes[2], sizes[0]);
}

#[tokio::test]
async fn off_a_departing_client_resizes_only_what_the_remaining_client_shows() {
    let (resizes, sizes) = second_client_leaves(false);
    assert_eq!(resizes, Resizes { shown: 1, ..NONE });
    assert_eq!(sizes[1], START_SIZE, "the hidden tab keeps the size it had");
    assert_eq!(sizes[2], START_SIZE, "the hidden workspace keeps its size");
}

/// A tab a client showed earlier keeps that client as its controller while the client lives, even
/// after the client moved on. A reapply of every controlled tab (any request that may change
/// geometry does it) used to resize such a tab although nobody shows it.
fn reapply_after_a_client_moved_on(resize_hidden_panes: bool) -> Resizes {
    let (mut server, panes) = hidden_resize_server(resize_hidden_panes);
    let (_first_control, _first_render) = attach(&mut server, 1, 100, 30);
    let (_second_control, _second_render) = attach(&mut server, 2, 70, 20);
    // Client 1 shows the hidden tab and then the first tab again; the hidden tab keeps its controller.
    assert!(server.focus_shell_client_on_tab(1, &panes.hidden_tab_id));
    assert!(server.claim_shell_tab_geometry(1, false));
    assert!(server.focus_shell_client_on_tab(1, &panes.shown_tab_id));
    // Client 1 already controls the first tab, so this claim has nothing to apply.
    assert!(!server.claim_shell_tab_geometry(1, false));
    assert_eq!(
        server.tab_geometry_controllers.get(&panes.hidden_tab_id),
        Some(&1),
        "guard: the tab nobody shows still has a controller"
    );
    let hidden_with_first = panes.sizes(&server)[1];
    // Client 1 controls the shown tab and changes its pane area: only the shown tab follows.
    client_pane_area(&mut server, 1, 90, 26);
    assert_eq!(
        panes.sizes(&server)[1],
        hidden_with_first,
        "guard: the tab nobody shows is stale"
    );

    let before = panes.total(&server);
    assert!(server.reapply_controlled_shell_tab_geometry(false));
    let resizes = panes.resizes_since(&server, before);
    shutdown_test_runtimes(&mut server);
    resizes
}

#[tokio::test]
async fn on_reapplying_controlled_geometry_resizes_a_tab_nobody_shows() {
    assert_eq!(
        reapply_after_a_client_moved_on(true),
        Resizes {
            hidden_tab: 1,
            ..NONE
        }
    );
}

#[tokio::test]
async fn off_reapplying_controlled_geometry_leaves_a_tab_nobody_shows() {
    assert_eq!(reapply_after_a_client_moved_on(false), NONE);
}

/// With no client attached nobody shows any tab. A layout pass at the server's own size used to
/// resize every pane, which is what a server does right after a live handoff.
fn layout_pass_without_clients(resize_hidden_panes: bool) -> Resizes {
    let (mut server, panes) = hidden_resize_server(resize_hidden_panes);
    assert!(server.clients.is_empty());
    let before = panes.total(&server);
    server.render_and_stream();
    let resizes = panes.resizes_since(&server, before);
    shutdown_test_runtimes(&mut server);
    resizes
}

#[tokio::test]
async fn on_a_layout_pass_without_clients_resizes_every_pane() {
    assert_eq!(
        layout_pass_without_clients(true),
        Resizes {
            shown: 1,
            hidden_tab: 1,
            hidden_workspace: 1
        }
    );
}

#[tokio::test]
async fn off_a_layout_pass_without_clients_resizes_no_pane() {
    assert_eq!(layout_pass_without_clients(false), NONE);
}

/// `compute_view_internal` resizes the shown tab and then every background tab. The server's
/// foreground client is always an active client shell, so its own layout pass does not take this
/// path; the test guards the function for any caller that does.
fn full_layout_pass(resize_hidden_panes: bool) -> Resizes {
    let (mut server, panes) = hidden_resize_server(resize_hidden_panes);
    let before = panes.total(&server);
    crate::ui::compute_view_with_runtime_registry(
        &mut server.app.state,
        &server.app.terminal_runtimes,
        Rect::new(0, 0, 100, 30),
    );
    let resizes = panes.resizes_since(&server, before);
    shutdown_test_runtimes(&mut server);
    resizes
}

#[tokio::test]
async fn on_a_full_layout_pass_resizes_the_background_tabs() {
    assert_eq!(
        full_layout_pass(true),
        Resizes {
            shown: 1,
            hidden_tab: 1,
            hidden_workspace: 1
        }
    );
}

#[tokio::test]
async fn off_a_full_layout_pass_resizes_only_the_shown_tab() {
    assert_eq!(full_layout_pass(false), Resizes { shown: 1, ..NONE });
}

// A tab can become shown without a navigation request of its own: a pane moved into it, the tab
// next to it taking over when the shown tab is moved away, a deferred worktree open that finishes
// later. With the setting off such a tab is stale unless the server sizes it when it comes into
// view. Every test below runs the same scenario with the setting on and off and asks the same
// question of the tab that is shown afterwards: does it have the size its client's pane area
// produces? With the setting on it always did; with it off it must too.

/// One client attached, then back on the first tab after visiting the hidden tab (so it still
/// controls that tab), and its pane area changed. With the setting off the hidden tab and the
/// hidden workspace now have sizes the pane area no longer produces. The size the pane area
/// produces for a one-pane tab is returned: what the first tab's pane has.
fn stale_hidden_tabs(resize_hidden_panes: bool) -> (HeadlessServer, Panes, (u16, u16), Receivers) {
    let (mut server, panes) = hidden_resize_server(resize_hidden_panes);
    let receivers = attach(&mut server, 1, 100, 30);
    assert!(server.focus_shell_client_on_tab(1, &panes.hidden_tab_id));
    assert!(server.claim_shell_tab_geometry(1, false));
    assert!(server.focus_shell_client_on_tab(1, &panes.shown_tab_id));
    client_pane_area(&mut server, 1, 60, 16);
    let expected = panes.sizes(&server)[0];
    assert_ne!(expected, START_SIZE);
    if !resize_hidden_panes {
        let sizes = panes.sizes(&server);
        assert_ne!(sizes[1], expected, "guard: the hidden tab is stale");
        assert_ne!(sizes[2], expected, "guard: the hidden workspace is stale");
        assert_eq!(
            server.tab_geometry_controllers.get(&panes.hidden_tab_id),
            Some(&1),
            "guard: the client still controls the hidden tab it visited"
        );
    }
    (server, panes, expected, receivers)
}

/// A pane of a tab the client shows fits the pane area its client reported. A tab of one pane has
/// exactly the size that area gives it (`alone`); a pane that shares its tab with another one is
/// smaller, and the layout decides by how much, so it only has to fit. A tab nobody showed since
/// the area changed is bigger than the new area here, so both forms tell it from a sized tab.
fn assert_shown_pane_fits(
    server: &HeadlessServer,
    workspace: usize,
    pane: crate::layout::PaneId,
    expected: (u16, u16),
    alone: bool,
    what: &str,
) {
    let (rows, cols) = server.app.state.workspaces[workspace].test_runtimes[&pane].current_size();
    if alone {
        assert_eq!(
            rows, expected.0,
            "{what}: the pane has {rows} rows, its client's pane area gives it {}",
            expected.0
        );
    } else {
        assert!(
            rows > 0 && rows <= expected.0,
            "{what}: the pane has {rows} rows, its client's pane area has {}",
            expected.0
        );
    }
    assert!(
        cols > 0 && cols <= expected.1,
        "{what}: the pane has {cols} columns, its client's pane area has {}",
        expected.1
    );
}

fn api_request_with_response(
    method: Method,
) -> (
    crate::api::ApiRequestMessage,
    std::sync::mpsc::Receiver<String>,
) {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    (
        crate::api::ApiRequestMessage {
            request: Request {
                id: "hidden-resize".into(),
                method,
            },
            peer_process: None,
            respond_to,
            response_write_complete: None,
        },
        response_rx,
    )
}

/// `pane.move` with focus brings the hidden tab into view next to the pane that was moved into it.
fn public_pane_move_into_the_hidden_tab(resize_hidden_panes: bool) {
    let (mut server, panes, expected, _receivers) = stale_hidden_tabs(resize_hidden_panes);
    let moved_pane = server.app.public_pane_id(0, panes.shown).unwrap();
    let hidden_tab_sequence = panes.runtime(&server, 0, panes.hidden_tab).content_seq();
    let (request, response_rx) = api_request_with_response(Method::PaneMove(PaneMoveParams {
        pane_id: moved_pane,
        destination: PaneMoveDestination::Tab {
            tab_id: panes.hidden_tab_id.clone(),
            target_pane_id: None,
            split: SplitDirection::Right,
            ratio: None,
        },
        focus: true,
    }));
    server.handle_api_request_with_shutdown_check(request);
    let response = response_rx.recv().expect("pane move response");
    assert!(
        response.contains("\"changed\":true"),
        "guard: the move happened: {response}"
    );
    assert_eq!(
        server.shell_tab_id_for_client(1).as_deref(),
        Some(panes.hidden_tab_id.as_str()),
        "guard: the client shows the tab the pane was moved into"
    );
    assert_shown_pane_fits(
        &server,
        0,
        panes.hidden_tab,
        expected,
        false,
        "the tab a pane was moved into",
    );
    if resize_hidden_panes {
        // The setting's default applies no geometry for this request, as upstream does: the tab
        // was sized already, and the shown-tabs rule must not run with the setting on.
        assert_eq!(
            panes.runtime(&server, 0, panes.hidden_tab).content_seq(),
            hidden_tab_sequence,
            "with the setting on the move resizes nothing"
        );
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn on_a_pane_moved_into_the_hidden_tab_leaves_the_shown_tab_sized() {
    public_pane_move_into_the_hidden_tab(true);
}

#[tokio::test]
async fn off_a_pane_moved_into_the_hidden_tab_leaves_the_shown_tab_sized() {
    public_pane_move_into_the_hidden_tab(false);
}

/// The client drags the tab it shows to another workspace; the tab next to it takes over.
fn client_moves_the_shown_tab_away(resize_hidden_panes: bool) {
    let (mut server, panes, expected, _receivers) = stale_hidden_tabs(resize_hidden_panes);
    server.handle_client_shell_api_request(
        1,
        api_request(Method::TabMoveToWorkspace(TabMoveToWorkspaceParams {
            tab_id: panes.shown_tab_id.clone(),
            workspace_id: panes.hidden_workspace_id.clone(),
            insert_index: None,
            focus: false,
        })),
    );
    assert_eq!(
        server.shell_tab_id_for_client(1).as_deref(),
        Some(panes.hidden_tab_id.as_str()),
        "guard: the tab next to the moved one is shown now"
    );
    assert_shown_pane_fits(
        &server,
        0,
        panes.hidden_tab,
        expected,
        true,
        "the tab that took over",
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn on_a_tab_moved_away_leaves_its_neighbour_sized() {
    client_moves_the_shown_tab_away(true);
}

#[tokio::test]
async fn off_a_tab_moved_away_leaves_its_neighbour_sized() {
    client_moves_the_shown_tab_away(false);
}

/// The end of a worktree open that a client asked for: its response names `tab_id`, and the
/// client is taken to that tab.
fn finish_endpoint_open_onto(server: &mut HeadlessServer, client_id: u64, tab_id: &str) {
    let client = server.clients.get_mut(&client_id).unwrap();
    client.shell_endpoint_command_in_flight = true;
    let revision = client.shell_projection_revision;
    client.shell_endpoint_command_surface_revision = Some(revision);
    client.shell_deferred_navigation_response = Some(Vec::new());
    let response = serde_json::json!({
        "id": "open-worktree",
        "result": {
            "type": "worktree_opened",
            "tab": { "tab_id": tab_id }
        }
    });
    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointResponseChunkReady {
            client_id,
            boot_id: server.client_shell_boot_id.clone(),
            request_id: "open-worktree".into(),
            final_chunk: true,
            data: serde_json::to_vec(&response).unwrap(),
        })
    );
}

/// A worktree open that a client asked for finishes later and shows a tab the client controls.
fn deferred_navigation_onto_a_controlled_tab(resize_hidden_panes: bool) {
    let (mut server, panes, expected, _receivers) = stale_hidden_tabs(resize_hidden_panes);
    finish_endpoint_open_onto(&mut server, 1, &panes.hidden_tab_id);
    assert_eq!(
        server.shell_tab_id_for_client(1).as_deref(),
        Some(panes.hidden_tab_id.as_str()),
        "guard: the finished open shows the tab"
    );
    assert_shown_pane_fits(
        &server,
        0,
        panes.hidden_tab,
        expected,
        true,
        "the tab a finished endpoint open shows",
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn on_a_finished_endpoint_open_leaves_the_tab_it_shows_sized() {
    deferred_navigation_onto_a_controlled_tab(true);
}

#[tokio::test]
async fn off_a_finished_endpoint_open_leaves_the_tab_it_shows_sized() {
    deferred_navigation_onto_a_controlled_tab(false);
}

/// The same with two clients attached. A client's pane area then sizes only the tab it shows, so
/// the tab client 1 visited earlier is stale even with the setting on, and the end of a deferred
/// open onto it only claims, as upstream does: the setting's default must leave it alone. With the
/// setting off the tab that comes into view is sized. The on test pins upstream's behaviour for
/// the default, stale tab included; a later fix of that behaviour would change it on purpose.
fn deferred_navigation_onto_a_controlled_tab_with_two_clients(resize_hidden_panes: bool) {
    let (mut server, panes) = hidden_resize_server(resize_hidden_panes);
    let (_first_control, _first_render) = attach(&mut server, 1, 100, 30);
    let (_second_control, _second_render) = attach(&mut server, 2, 70, 20);
    // Client 1 shows the hidden tab and then the first tab again; it keeps controlling both.
    assert!(server.focus_shell_client_on_tab(1, &panes.hidden_tab_id));
    assert!(server.claim_shell_tab_geometry(1, false));
    assert!(server.focus_shell_client_on_tab(1, &panes.shown_tab_id));
    assert!(
        !server.claim_shell_tab_geometry(1, false),
        "guard: client 1 already controls the first tab"
    );
    // With two clients its pane area change sizes only the tab it shows.
    client_pane_area(&mut server, 1, 60, 16);
    let expected = panes.sizes(&server)[0];
    assert_ne!(expected, START_SIZE);
    assert_ne!(
        panes.sizes(&server)[1],
        expected,
        "guard: the hidden tab is stale, with the setting on too"
    );
    assert_eq!(
        server.tab_geometry_controllers.get(&panes.hidden_tab_id),
        Some(&1),
        "guard: client 1 still controls the hidden tab it visited"
    );
    let hidden_tab_sequence = panes.runtime(&server, 0, panes.hidden_tab).content_seq();

    finish_endpoint_open_onto(&mut server, 1, &panes.hidden_tab_id);
    assert_eq!(
        server.shell_tab_id_for_client(1).as_deref(),
        Some(panes.hidden_tab_id.as_str()),
        "guard: the finished open shows the tab"
    );
    if resize_hidden_panes {
        assert_eq!(
            panes.runtime(&server, 0, panes.hidden_tab).content_seq(),
            hidden_tab_sequence,
            "with the setting on the finished open applies no geometry to a tab its client controls"
        );
    } else {
        assert_shown_pane_fits(
            &server,
            0,
            panes.hidden_tab,
            expected,
            true,
            "the tab a finished endpoint open shows, with two clients",
        );
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn on_a_finished_endpoint_open_with_two_clients_applies_no_geometry() {
    deferred_navigation_onto_a_controlled_tab_with_two_clients(true);
}

#[tokio::test]
async fn off_a_finished_endpoint_open_with_two_clients_sizes_the_tab_it_shows() {
    deferred_navigation_onto_a_controlled_tab_with_two_clients(false);
}

/// A public `worktree.open` with focus on a worktree that is already open finishes later and
/// shows the workspace that holds it.
async fn public_worktree_open_shows_the_hidden_workspace(resize_hidden_panes: bool) {
    let (mut server, panes, expected, _receivers) = stale_hidden_tabs(resize_hidden_panes);
    // One folder holds the repository and its checkout; it is removed when the test ends, also
    // when it panics.
    let root = std::env::temp_dir().join(format!(
        "herdr-hidden-resize-open-{}-{resize_hidden_panes}",
        std::process::id()
    ));
    let repo = root.join("repo");
    let checkout = root.join("checkout");
    std::fs::create_dir_all(&root).unwrap();
    let _root_guard = TestTempPath::owns_root(root, repo.clone());
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet", repo.to_str().unwrap()]);
    git(&[
        "-C",
        repo.to_str().unwrap(),
        "-c",
        "user.name=Herdr Test",
        "-c",
        "user.email=herdr@example.invalid",
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "initial",
    ]);
    git(&[
        "-C",
        repo.to_str().unwrap(),
        "worktree",
        "add",
        "--quiet",
        "-b",
        "hidden-open",
        checkout.to_str().unwrap(),
    ]);
    // The first workspace is the repository and the hidden workspace is a checkout of it that is
    // already open. A workspace takes its place from the working directory of its terminals.
    for (index, path) in [(0, &repo), (1, &checkout)] {
        server.app.state.workspaces[index].identity_cwd = path.clone();
        let terminal_ids = server.app.state.workspaces[index]
            .tabs
            .iter()
            .flat_map(|tab| {
                tab.panes
                    .values()
                    .map(|pane| pane.attached_terminal_id.clone())
            })
            .collect::<Vec<_>>();
        for terminal_id in terminal_ids {
            server
                .app
                .state
                .terminals
                .get_mut(&terminal_id)
                .unwrap()
                .cwd = path.clone();
        }
    }
    let source_id = server.app.public_workspace_id(0);
    let (request, response_rx) =
        api_request_with_response(Method::WorktreeOpen(WorktreeOpenParams {
            workspace_id: Some(source_id),
            branch: Some("hidden-open".into()),
            focus: true,
            ..Default::default()
        }));
    server.handle_api_request_with_shutdown_check(request);
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = server.app.event_rx.recv().await.unwrap();
            let completed = matches!(&event, AppEvent::WorktreeReadFinished(_));
            server.handle_internal_event_with_forwarding(event);
            if completed {
                break;
            }
        }
    })
    .await
    .expect("the open finishes");
    let response = response_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("worktree open response");
    assert!(
        response.contains("worktree_opened"),
        "guard: the open succeeded: {response}"
    );
    assert_eq!(
        server.shell_tab_id_for_client(1).as_deref(),
        server.app.public_tab_id(1, 0).as_deref(),
        "guard: the client shows the workspace that was already open"
    );
    assert_shown_pane_fits(
        &server,
        1,
        panes.hidden_workspace,
        expected,
        true,
        "the workspace a finished open shows",
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn on_a_finished_public_worktree_open_leaves_the_workspace_it_shows_sized() {
    public_worktree_open_shows_the_hidden_workspace(true).await;
}

#[tokio::test]
async fn off_a_finished_public_worktree_open_leaves_the_workspace_it_shows_sized() {
    public_worktree_open_shows_the_hidden_workspace(false).await;
}
