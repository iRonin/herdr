use super::*;
use crate::api::schema::{
    AgentStatus, EventData, EventEnvelope, EventKind, PaneInfo, PaneReadResult, PaneReadState,
    ReadFormat, ReadSource,
};
use crate::ipc::{poll_local_stream_read_count, LocalStreamReadCount};
use interprocess::local_socket::traits::Listener as _;
use serde_json::{json, Value};
use std::sync::atomic::AtomicU64;
use tokio::sync::mpsc;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

struct SocketTest {
    hub: EventHub,
    running: Arc<AtomicBool>,
    api_tx: ApiRequestSender,
    api_rx: mpsc::UnboundedReceiver<ApiRequestMessage>,
    workers: Vec<std::thread::JoinHandle<io::Result<()>>>,
    paths: Vec<PathBuf>,
}

impl SocketTest {
    fn new() -> Self {
        Self::with_hub(EventHub::default())
    }

    /// A test whose connections block for `liveness` and sample the agent-status snapshot every
    /// `snapshot`: with 30 s, only a hub wake can explain a delivery within a few seconds.
    fn with_timing(liveness: Duration, snapshot: Duration) -> Self {
        Self::with_hub(EventHub::with_timing(WaitTiming {
            liveness,
            agent_status_snapshot: snapshot,
        }))
    }

    fn with_hub(hub: EventHub) -> Self {
        let (api_tx, api_rx) = mpsc::unbounded_channel();
        Self {
            hub,
            running: Arc::new(AtomicBool::new(true)),
            api_tx,
            api_rx,
            workers: Vec::new(),
            paths: Vec::new(),
        }
    }

    fn connect(&mut self) -> Client {
        static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "herdr-sub-{}-{}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = bind_local_listener(&path).unwrap();
        self.paths.push(path.clone());
        let mut stream = crate::ipc::connect_local_stream(&path).unwrap();
        let server = listener.accept().unwrap();
        set_local_stream_polling(&mut stream, true).unwrap();
        let api_tx = self.api_tx.clone();
        let hub = self.hub.clone();
        let running = Arc::clone(&self.running);
        let worker =
            std::thread::spawn(move || handle_connection(server, &api_tx, &hub, &running, None));
        self.workers.push(worker);
        Client {
            stream,
            buffered: Vec::new(),
        }
    }

    fn app_request(&mut self) -> ApiRequestMessage {
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            match self.api_rx.try_recv() {
                Ok(request) => return request,
                Err(mpsc::error::TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for app request"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("app request channel closed: {error}"),
            }
        }
    }
}

impl Drop for SocketTest {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // What `ServerHandle::drop` does: connection threads blocked on the hub notice the flag now.
        self.hub.wake_waiters();
        self.api_rx.close();
        while self.api_rx.try_recv().is_ok() {}
        let deadline = Instant::now() + APP_RESPONSE_TIMEOUT + Duration::from_secs(1);
        let mut failures = Vec::new();
        for worker in self.workers.drain(..) {
            while !worker.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            if worker.is_finished() {
                match worker.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => failures.push(format!("connection failed: {error}")),
                    Err(_) => failures.push("connection panicked".into()),
                }
            } else {
                failures.push("subscription connection did not stop".into());
            }
        }
        // Also remove Windows listener marker files, including after a setup failure.
        for path in self.paths.drain(..) {
            if let Err(error) = std::fs::remove_file(path) {
                if error.kind() != io::ErrorKind::NotFound {
                    failures.push(format!("socket cleanup failed: {error}"));
                }
            }
        }
        if !std::thread::panicking() {
            assert!(failures.is_empty(), "{failures:?}");
        }
    }
}

struct Client {
    stream: LocalStream,
    buffered: Vec<u8>,
}

impl Client {
    fn send(&mut self, request: Value) {
        writeln!(self.stream, "{request}").unwrap();
    }

    fn subscribe(&mut self, id: &str, subscriptions: Value) {
        self.send(json!({
            "id": id,
            "method": "events.subscribe",
            "params": {"subscriptions": subscriptions}
        }));
    }

    fn next_line(&mut self, deadline: Instant) -> Option<Value> {
        // LocalStream recv timeouts are unsupported on Windows. Use the same bounded
        // nonblocking/PeekNamedPipe reads as the API client, retaining partial JSON lines.
        loop {
            if let Some(end) = self.buffered.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = self.buffered.drain(..=end).collect();
                return Some(serde_json::from_slice(&line).expect("subscription JSON"));
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for socket response"
            );
            let mut bytes = [0; 4096];
            match poll_local_stream_read_count(&mut self.stream, &mut bytes).unwrap() {
                LocalStreamReadCount::Data(count) => {
                    self.buffered.extend_from_slice(&bytes[..count])
                }
                LocalStreamReadCount::Pending => std::thread::sleep(Duration::from_millis(1)),
                LocalStreamReadCount::Closed => {
                    assert!(self.buffered.is_empty(), "incomplete JSON at EOF");
                    return None;
                }
            }
        }
    }

    fn response(&mut self) -> Value {
        self.next_line(Instant::now() + RESPONSE_TIMEOUT)
            .expect("socket response before EOF")
    }

    fn assert_started(&mut self, id: &str) {
        let response = self.response();
        assert_eq!(response["id"], id);
        assert_eq!(response["result"]["type"], "subscription_started");
    }

    fn assert_renames(&mut self, indices: std::ops::Range<usize>, deadline: Instant) {
        for index in indices {
            let event = self.next_line(deadline).expect("rename before EOF");
            assert_eq!(event["event"], "workspace_renamed");
            assert_eq!(event["data"]["label"], format!("flood-{index}"));
        }
    }

    fn assert_history_lost(&mut self, id: &str) {
        let response = self.response();
        assert_eq!(response["id"], id);
        assert_eq!(response["error"]["code"], "events_lost", "{response}");
        assert_eq!(self.next_line(Instant::now() + RESPONSE_TIMEOUT), None);
    }
}

fn renamed_event(index: usize) -> EventEnvelope {
    EventEnvelope {
        event: EventKind::WorkspaceRenamed,
        data: EventData::WorkspaceRenamed {
            workspace_id: "workspace_1".into(),
            label: format!("flood-{index}"),
        },
    }
}

fn output_subscription() -> Value {
    json!({
        "type": "pane.output_matched",
        "pane_id": "pane_1",
        "source": "recent",
        "match": {"type": "substring", "value": "never"}
    })
}

fn reply(request: ApiRequestMessage, result: ResponseResult) {
    request
        .respond_to
        .send(
            serde_json::to_string(&SuccessResponse {
                id: request.request.id,
                result,
            })
            .unwrap(),
        )
        .unwrap();
}

fn reply_to_probe(request: ApiRequestMessage) {
    reply_to_probe_with_status(request, AgentStatus::Working);
}

fn reply_to_probe_with_status(request: ApiRequestMessage, agent_status: AgentStatus) {
    let result = match request.request.method {
        Method::PaneGet(_) => ResponseResult::PaneInfo {
            pane: PaneInfo {
                pane_id: "pane_1".into(),
                terminal_id: "term_1".into(),
                workspace_id: "workspace_1".into(),
                tab_id: "tab_1".into(),
                focused: true,
                cwd: None,
                foreground_cwd: None,
                restore_error: None,
                label: None,
                agent: Some("pi".into()),
                title: None,
                terminal_title: None,
                terminal_title_stripped: None,
                display_agent: None,
                agent_status,
                state_labels: Default::default(),
                tokens: Default::default(),
                agent_session: None,
                scroll: None,
                revision: 0,
            },
        },
        Method::PaneRead(_) => ResponseResult::PaneRead {
            read: PaneReadResult {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                tab_id: "tab_1".into(),
                source: ReadSource::RecentUnwrapped,
                format: ReadFormat::Text,
                text: String::new(),
                revision: 0,
                truncated: false,
            },
        },
        ref other => panic!("unexpected subscription probe: {other:?}"),
    };
    reply(request, result);
}

#[test]
fn subscriptions_drain_retained_bursts_without_per_event_poll_delay() {
    let mut test = SocketTest::new();
    let mut client = test.connect();
    client.subscribe("burst", json!([{"type": "workspace.renamed"}]));
    client.assert_started("burst");
    for index in 0..128 {
        test.hub.push(renamed_event(index));
    }
    // One deadline for the entire batch detects a 100 ms delay per event.
    client.assert_renames(0..128, Instant::now() + RESPONSE_TIMEOUT);
}

#[test]
fn subscriptions_report_history_loss_before_sending_a_partial_stream() {
    assert_subscription_history_loss(false);
}

#[test]
fn subscriptions_report_history_loss_before_initial_agent_status() {
    assert_subscription_history_loss(true);
}

fn assert_subscription_history_loss(agent_status: bool) {
    let mut test = SocketTest::new();
    let mut client = test.connect();
    let subscriptions = if agent_status {
        json!([{
            "type": "pane.agent_status_changed",
            "pane_id": "pane_1",
            "agent_status": "working"
        }])
    } else {
        json!([{"type": "workspace.renamed"}, output_subscription()])
    };
    client.subscribe("history-gap", subscriptions);
    // Hold the setup probe after the server pins its subscription cursor.
    let probe = test.app_request();
    assert!(probe.request.id.ends_with(":probe"));
    for index in 0..600 {
        test.hub.push(renamed_event(index));
    }
    reply_to_probe(probe);
    client.assert_started("history-gap");
    client.assert_history_lost("history-gap");
}

#[test]
fn lagging_subscription_closes_without_interrupting_other_clients() {
    let mut test = SocketTest::new();
    let mut healthy = test.connect();
    healthy.subscribe("healthy", json!([{"type": "workspace.renamed"}]));
    healthy.assert_started("healthy");

    let mut slow = test.connect();
    slow.subscribe(
        "slow",
        json!([{"type": "workspace.renamed"}, output_subscription()]),
    );
    let probe = test.app_request();
    assert_eq!(probe.request.id, "slow:sub:1:probe");
    reply_to_probe(probe);
    slow.assert_started("slow");
    // Pause only this connection in an existing app request, rather than depending
    // on OS socket buffer sizes or sleeping to make its event cursor fall behind.
    let paused_read = test.app_request();
    assert_eq!(paused_read.request.id, "slow:sub:1:read");
    assert!(matches!(paused_read.request.method, Method::PaneRead(_)));
    // All five batches must finish before the held app request can time out.
    let deadline = Instant::now() + RESPONSE_TIMEOUT;
    for batch in 0..5 {
        let indices = batch * 128..(batch + 1) * 128;
        for index in indices.clone() {
            test.hub.push(renamed_event(index));
        }
        healthy.assert_renames(indices, deadline);
    }

    reply_to_probe(paused_read);
    slow.assert_history_lost("slow");
    test.hub.push(renamed_event(640));
    healthy.assert_renames(640..641, Instant::now() + RESPONSE_TIMEOUT);

    let mut ordinary = test.connect();
    ordinary.send(json!({"id": "ordinary", "method": "workspace.list", "params": {}}));
    let request = test.app_request();
    assert_eq!(request.request.id, "ordinary");
    assert!(matches!(request.request.method, Method::WorkspaceList(_)));
    reply(
        request,
        ResponseResult::WorkspaceList {
            workspaces: Vec::new(),
        },
    );
    let response = ordinary.response();
    assert_eq!(response["id"], "ordinary");
    assert_eq!(response["result"]["type"], "workspace_list");
}

fn idle_agent_status_subscription() -> Value {
    json!([{"type": "pane.agent_status_changed", "pane_id": "pane_1"}])
}

fn agent_status_event(title: Option<&str>) -> EventEnvelope {
    EventEnvelope {
        event: EventKind::PaneAgentStatusChanged,
        data: EventData::PaneAgentStatusChanged {
            pane_id: "pane_1".into(),
            workspace_id: "workspace_1".into(),
            agent_status: AgentStatus::Working,
            agent: Some("pi".into()),
            title: title.map(Into::into),
            display_agent: None,
            state_labels: Default::default(),
        },
    }
}

/// Answers every app request that arrives during `window` with the probe reply and returns how
/// many `pane.get` requests there were.
fn answer_pane_gets_for(test: &mut SocketTest, window: Duration) -> usize {
    answer_pane_gets_with_status_for(test, window, AgentStatus::Working)
}

/// `answer_pane_gets_for`, answering every `pane.get` with `agent_status`.
fn answer_pane_gets_with_status_for(
    test: &mut SocketTest,
    window: Duration,
    agent_status: AgentStatus,
) -> usize {
    let end = Instant::now() + window;
    let mut pane_gets = 0;
    while Instant::now() < end {
        match test.api_rx.try_recv() {
            Ok(request) => {
                if matches!(request.request.method, Method::PaneGet(_)) {
                    pane_gets += 1;
                }
                reply_to_probe_with_status(request, agent_status);
            }
            Err(mpsc::error::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => panic!("app request channel closed: {error}"),
        }
    }
    pane_gets
}

#[test]
fn an_idle_event_subscription_does_not_poll_the_history() {
    let mut test = SocketTest::new();
    let mut client = test.connect();
    client.subscribe("idle", json!([{"type": "workspace.renamed"}]));
    client.assert_started("idle");
    let reads_before = test.hub.history_reads();
    std::thread::sleep(Duration::from_millis(3500));
    let reads = test.hub.history_reads() - reads_before;
    // Before the event hub the connection thread read the history every 100 ms: about 35 reads here.
    // Now it blocks on the hub and looks again once per liveness interval (5 s), so none in this
    // window; at the 1 s interval of the previous release there were 3 or 4.
    assert!(
        reads <= 1,
        "an idle subscription read the event history {reads} times in 3.5 s"
    );
}

#[test]
fn an_idle_agent_status_subscription_asks_the_app_for_a_pane_once_per_snapshot_interval() {
    let mut test = SocketTest::new();
    let mut client = test.connect();
    client.subscribe("idle-status", idle_agent_status_subscription());
    // Setup probe, as the server pins its cursor.
    let probe = test.app_request();
    assert!(probe.request.id.ends_with(":probe"));
    reply_to_probe(probe);
    client.assert_started("idle-status");
    let pane_gets = answer_pane_gets_for(&mut test, Duration::from_millis(2500));
    // At 3284e19a every idle 100 ms pass asked the app for the pane: about 25 requests here.
    // Now the snapshot is taken once per interval (1 s: the first pass and two more), and it stays
    // at 1 s when the liveness interval is 5 s: fewer than 2 requests would be a slower snapshot.
    assert!(
        (2..=5).contains(&pane_gets),
        "an idle agent-status subscription asked for the pane {pane_gets} times in 2.5 s"
    );
}

#[test]
fn an_event_wakes_an_idle_connection_that_would_otherwise_sleep_for_30_seconds() {
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let mut client = test.connect();
    client.subscribe("wake", json!([{"type": "workspace.renamed"}]));
    client.assert_started("wake");
    // Let the connection thread block on the hub, then publish.
    std::thread::sleep(Duration::from_millis(300));
    test.hub.push(renamed_event(0));
    // With a 30 s liveness interval only the hub wake can deliver this within 5 s.
    client.assert_renames(0..1, Instant::now() + Duration::from_secs(5));
}

#[test]
fn a_title_change_event_reaches_an_idle_agent_status_subscriber_without_waiting_for_a_snapshot() {
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let mut client = test.connect();
    client.subscribe("title", idle_agent_status_subscription());
    let probe = test.app_request();
    assert!(probe.request.id.ends_with(":probe"));
    reply_to_probe(probe);
    client.assert_started("title");
    // No status filter, so there is no initial event; the first pass takes its snapshot.
    answer_pane_gets_for(&mut test, Duration::from_millis(300));
    // The app emits this event when an agent reports a new title (`emit_pane_state_update`). With
    // both intervals at 30 s, only the hub wake can deliver it within 5 s: it does not wait for
    // the 1 s snapshot of production.
    let mut event = agent_status_event(Some("fixing the bug"));
    if let EventData::PaneAgentStatusChanged { title, .. } = &mut event.data {
        assert_eq!(title.as_deref(), Some("fixing the bug"));
    }
    test.hub.push(event);
    let delivered = client
        .next_line(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(delivered["event"], "pane.agent_status_changed");
    assert_eq!(delivered["data"]["title"], "fixing the bug");
}

#[test]
fn a_closed_peer_is_noticed_within_the_liveness_interval() {
    let mut test = SocketTest::with_timing(Duration::from_millis(200), Duration::from_secs(30));
    let mut client = test.connect();
    client.subscribe("close", json!([{"type": "workspace.renamed"}]));
    client.assert_started("close");
    std::thread::sleep(Duration::from_millis(100));
    drop(client);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !test.workers[0].is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        test.workers[0].is_finished(),
        "the connection thread did not notice that its peer closed"
    );
}

#[test]
fn stopping_the_server_releases_a_connection_blocked_on_the_hub() {
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let mut client = test.connect();
    client.subscribe("stop", json!([{"type": "workspace.renamed"}]));
    client.assert_started("stop");
    std::thread::sleep(Duration::from_millis(300));
    // What `ServerHandle::drop` does, once: a single wake-up must be enough (it is not repeated here, which would hide a
    // lost one); `dropping_the_server_handle_releases_a_held_subscription` pins the real `ServerHandle::drop`.
    test.running.store(false, Ordering::Relaxed);
    test.hub.wake_waiters();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !test.workers[0].is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        test.workers[0].is_finished(),
        "a connection blocked on the hub kept running after the server stopped"
    );
}

#[test]
fn an_events_wait_ends_when_the_matching_event_arrives_without_polling() {
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let mut client = test.connect();
    client.send(json!({
        "id": "wait",
        "method": "events.wait",
        "params": {
            "match_event": {
                "event": "pane_agent_status_changed",
                "pane_id": "pane_1",
                "agent_status": "idle"
            },
            "timeout_ms": 60_000
        }
    }));
    // Setup probe and the first snapshot: the pane is working, so there is no match yet.
    answer_pane_gets_for(&mut test, Duration::from_millis(500));
    let mut event = agent_status_event(None);
    if let EventData::PaneAgentStatusChanged { agent_status, .. } = &mut event.data {
        *agent_status = AgentStatus::Idle;
    }
    test.hub.push(event);
    // With a 30 s liveness and snapshot interval only the hub wake can end the wait within 5 s.
    let response = client
        .next_line(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(response["id"], "wait");
    assert!(response.get("error").is_none(), "{response}");
}

#[test]
fn a_pane_event_takes_a_snapshot_at_once_but_only_for_the_subscribed_pane() {
    // Both intervals are 30 s: a `pane.get` within the test can only be the answer to an event.
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let mut client = test.connect();
    client.subscribe("pane-events", idle_agent_status_subscription());
    let probe = test.app_request();
    assert!(probe.request.id.ends_with(":probe"));
    reply_to_probe(probe);
    client.assert_started("pane-events");
    // The first pass takes its snapshot; then the connection blocks on the hub.
    assert_eq!(
        answer_pane_gets_for(&mut test, Duration::from_millis(400)),
        1
    );

    let closed = |pane_id: &str| EventEnvelope {
        event: EventKind::PaneClosed,
        data: EventData::PaneClosed {
            pane_id: pane_id.into(),
            workspace_id: "workspace_1".into(),
        },
    };
    // An event about another pane changes nothing this subscription reports.
    test.hub.push(closed("pane_2"));
    assert_eq!(
        answer_pane_gets_for(&mut test, Duration::from_millis(400)),
        0
    );
    // An event about the subscribed pane (here: it closed, so `pane.get` may now fail with
    // `pane_not_found`) is checked at once, not at the next interval.
    test.hub.push(closed("pane_1"));
    let request = test.app_request();
    assert!(matches!(request.request.method, Method::PaneGet(_)));
    assert!(
        request.request.id.ends_with(":pane"),
        "{}",
        request.request.id
    );
    reply_to_probe(request);
}

fn read_state_changed(pane_id: &str) -> EventEnvelope {
    EventEnvelope {
        event: EventKind::PaneReadStateChanged,
        data: EventData::PaneReadStateChanged {
            read_state: PaneReadState {
                pane_id: pane_id.into(),
                workspace_id: "workspace_1".into(),
                seen: true,
                agent_status: AgentStatus::Idle,
            },
        },
    }
}

#[test]
fn a_viewed_done_pane_reaches_an_agent_status_subscriber_at_once() {
    // Viewing a done pane (or giving it the `auto_read` token late) turns done into idle with no
    // `pane.agent_status_changed`; the app says so with `pane.read_state_changed`. Both intervals
    // are 30 s, so only that event can make the subscription look at the pane within 5 s.
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let mut client = test.connect();
    client.subscribe("read", idle_agent_status_subscription());
    let probe = test.app_request();
    assert!(probe.request.id.ends_with(":probe"));
    reply_to_probe_with_status(probe, AgentStatus::Done);
    client.assert_started("read");
    // The first pass takes its snapshot (done); then the connection blocks on the hub.
    assert_eq!(
        answer_pane_gets_with_status_for(&mut test, Duration::from_millis(400), AgentStatus::Done),
        1
    );

    // A read-state change of another pane is no concern of this subscription.
    test.hub.push(read_state_changed("pane_2"));
    assert_eq!(
        answer_pane_gets_with_status_for(&mut test, Duration::from_millis(400), AgentStatus::Idle),
        0
    );
    // The subscribed pane was viewed: its snapshot now says idle, and the subscriber is told.
    test.hub.push(read_state_changed("pane_1"));
    let request = test.app_request();
    assert!(matches!(request.request.method, Method::PaneGet(_)));
    reply_to_probe_with_status(request, AgentStatus::Idle);
    let delivered = client
        .next_line(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(delivered["event"], "pane.agent_status_changed");
    assert_eq!(delivered["data"]["agent_status"], "idle");
}

#[test]
fn removing_a_tab_or_workspace_takes_a_snapshot_at_once() {
    // `tab.close`, `workspace.close` and `tab.move_to_workspace` emit no `pane.closed` or
    // `pane.moved` for the panes inside, yet `pane.get` answers `pane_not_found` (or another id)
    // afterwards. Both intervals are 30 s, so a `pane.get` within the test answers one of them.
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let mut client = test.connect();
    client.subscribe("removed", idle_agent_status_subscription());
    let probe = test.app_request();
    assert!(probe.request.id.ends_with(":probe"));
    reply_to_probe(probe);
    client.assert_started("removed");
    assert_eq!(
        answer_pane_gets_for(&mut test, Duration::from_millis(400)),
        1
    );

    // Control: an event that cannot change the pane takes no snapshot.
    test.hub.push(EventEnvelope {
        event: EventKind::WorkspaceRenamed,
        data: EventData::WorkspaceRenamed {
            workspace_id: "workspace_1".into(),
            label: "renamed".into(),
        },
    });
    assert_eq!(
        answer_pane_gets_for(&mut test, Duration::from_millis(400)),
        0
    );

    let removals = [
        EventEnvelope {
            event: EventKind::TabClosed,
            data: EventData::TabClosed {
                tab_id: "tab_1".into(),
                workspace_id: "workspace_1".into(),
            },
        },
        EventEnvelope {
            event: EventKind::WorkspaceClosed,
            data: EventData::WorkspaceClosed {
                workspace_id: "workspace_1".into(),
                workspace: None,
            },
        },
        EventEnvelope {
            event: EventKind::TabMoved,
            data: EventData::TabMoved {
                tab_id: "tab_3".into(),
                workspace_id: "workspace_2".into(),
                insert_index: 0,
                tabs: Vec::new(),
                previous_tab_id: Some("tab_1".into()),
                previous_workspace_id: Some("workspace_1".into()),
            },
        },
    ];
    for removal in removals {
        let name = format!("{:?}", removal.event);
        test.hub.push(removal);
        let request = test.app_request();
        assert!(
            matches!(request.request.method, Method::PaneGet(_)),
            "{name} must take a snapshot"
        );
        reply_to_probe(request);
    }
}

// ---- the filtered wake: which held connection an event wakes ---------------------------------------------

/// Answers the `pane.read_state` probe of a read-state subscription with the pane it asked about.
fn reply_to_read_state_probe(request: ApiRequestMessage) {
    let Method::PaneReadState(target) = &request.request.method else {
        panic!(
            "unexpected subscription probe: {:?}",
            request.request.method
        );
    };
    let pane_id = target.pane_id.clone();
    reply(
        request,
        ResponseResult::PaneReadState {
            read_state: PaneReadState {
                pane_id,
                workspace_id: "workspace_1".into(),
                seen: true,
                agent_status: AgentStatus::Idle,
            },
        },
    );
}

/// One connection holding a `pane.read_state_changed` subscription for each of `panes`, started and
/// idle: it has answered its probes and is blocked on the hub.
fn hold_read_state_subscriptions(test: &mut SocketTest, id: &str, panes: &[&str]) -> Client {
    let mut client = test.connect();
    let subscriptions: Vec<Value> = panes
        .iter()
        .map(|pane| json!({"type": "pane.read_state_changed", "pane_id": pane}))
        .collect();
    client.subscribe(id, Value::Array(subscriptions));
    for _ in panes {
        reply_to_read_state_probe(test.app_request());
    }
    client.assert_started(id);
    // Let the connection thread finish its first pass and block on the hub.
    std::thread::sleep(Duration::from_millis(300));
    client
}

fn sample_event(kind: EventKind, pane_id: &str) -> EventEnvelope {
    crate::api::event_hub::samples::event(kind, pane_id)
}

/// Waits until the hub's history has been read at least `count` more times than `before`.
fn wait_for_history_reads(hub: &EventHub, before: u64, count: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while hub.history_reads() < before + count && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    hub.history_reads() >= before + count
}

#[test]
fn a_subscriber_on_one_pane_is_not_woken_by_events_about_another_pane() {
    // WHY: with one subscriber per pane (a per-pane client holds many), an event about pane B must
    // not cost the subscriber of pane A a thread wake-up and a history read. The liveness interval is
    // 30 s, so a history read inside this test can only have been caused by an event.
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let mut client = hold_read_state_subscriptions(&mut test, "a", &["pane_1"]);
    let reads = test.hub.history_reads();
    for _ in 0..40 {
        test.hub.push(read_state_changed("pane_2"));
        test.hub
            .push(sample_event(EventKind::PaneAgentStatusChanged, "pane_2"));
        test.hub
            .push(sample_event(EventKind::PaneUpdated, "pane_2"));
        test.hub
            .push(sample_event(EventKind::WorkspaceFocused, "pane_2"));
    }
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        test.hub.history_reads(),
        reads,
        "160 events about other panes woke the subscriber of pane_1"
    );
    // Not deaf: its own pane's event still arrives.
    test.hub.push(read_state_changed("pane_1"));
    let delivered = client
        .next_line(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(delivered["event"], "pane.read_state_changed");
    assert_eq!(delivered["data"]["pane_id"], "pane_1");
}

#[test]
fn a_subscriber_on_a_pane_is_woken_promptly_by_an_event_about_that_pane() {
    // The liveness interval is 30 s: only the wake can deliver this within 5 s.
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let mut client = hold_read_state_subscriptions(&mut test, "a", &["pane_1"]);
    let other = hold_read_state_subscriptions(&mut test, "b", &["pane_2"]);
    let reads = test.hub.history_reads();
    let started = Instant::now();
    test.hub.push(read_state_changed("pane_1"));
    let delivered = client
        .next_line(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(delivered["data"]["pane_id"], "pane_1");
    assert!(started.elapsed() < Duration::from_secs(5));
    // Exactly the one subscriber that the event is about read the history.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(test.hub.history_reads(), reads + 1);
    drop(other);
}

#[test]
fn a_subscription_that_is_not_about_one_pane_still_sees_every_event_of_its_kind() {
    // `pane.created`, `workspace.renamed` and the like name no pane in the subscription: they must
    // keep seeing every event of their kind, from any pane, while per-pane subscribers sit idle.
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let _idle = hold_read_state_subscriptions(&mut test, "idle", &["pane_1"]);
    let mut created = test.connect();
    created.subscribe("created", json!([{"type": "pane.created"}]));
    created.assert_started("created");
    let mut renamed = test.connect();
    renamed.subscribe("renamed", json!([{"type": "workspace.renamed"}]));
    renamed.assert_started("renamed");
    std::thread::sleep(Duration::from_millis(300));

    test.hub
        .push(sample_event(EventKind::PaneCreated, "pane_7"));
    test.hub.push(renamed_event(0));
    test.hub
        .push(sample_event(EventKind::PaneCreated, "pane_8"));
    test.hub.push(sample_event(EventKind::PaneClosed, "pane_8"));
    test.hub.push(renamed_event(1));
    let deadline = Instant::now() + Duration::from_secs(5);
    for pane in ["pane_7", "pane_8"] {
        let event = created.next_line(deadline).unwrap();
        assert_eq!(event["event"], "pane_created");
        assert_eq!(event["data"]["pane"]["pane_id"], pane);
    }
    renamed.assert_renames(0..2, deadline);
}

#[test]
fn a_connection_with_subscriptions_on_two_panes_wakes_for_either_and_for_no_other() {
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let mut client = hold_read_state_subscriptions(&mut test, "two", &["pane_1", "pane_2"]);
    let deadline = || Instant::now() + Duration::from_secs(5);
    test.hub.push(read_state_changed("pane_2"));
    assert_eq!(
        client.next_line(deadline()).unwrap()["data"]["pane_id"],
        "pane_2"
    );
    test.hub.push(read_state_changed("pane_1"));
    assert_eq!(
        client.next_line(deadline()).unwrap()["data"]["pane_id"],
        "pane_1"
    );
    // A third pane is nobody's concern here.
    std::thread::sleep(Duration::from_millis(200));
    let reads = test.hub.history_reads();
    test.hub.push(read_state_changed("pane_3"));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(test.hub.history_reads(), reads);
}

#[test]
fn an_event_that_removes_panes_without_naming_them_wakes_every_held_connection() {
    // A tab or workspace that closes (or a tab that moves) carries no pane list: nobody can tell it
    // does not concern them, so every connection looks (a history read each). 30 s liveness again.
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let _a = hold_read_state_subscriptions(&mut test, "a", &["pane_1"]);
    let _b = hold_read_state_subscriptions(&mut test, "b", &["pane_2"]);
    let mut renamed = test.connect();
    renamed.subscribe("renamed", json!([{"type": "workspace.renamed"}]));
    renamed.assert_started("renamed");
    std::thread::sleep(Duration::from_millis(300));
    let reads = test.hub.history_reads();
    test.hub.push(sample_event(EventKind::TabClosed, "pane_1"));
    assert!(
        wait_for_history_reads(&test.hub, reads, 3),
        "tab.closed woke only {} of 3 connections",
        test.hub.history_reads() - reads
    );
}

#[test]
fn a_quiet_subscriber_keeps_up_with_a_flood_of_events_about_other_panes() {
    // WHY: the history keeps 512 events of every kind and a subscription that falls out of it is
    // closed with `events_lost`. A subscriber that no event is about is not woken by them, so the hub
    // must still wake it before it falls behind (half the history). 1200 events about another
    // pane, in bursts, with a 30 s liveness interval: the subscriber must survive and still deliver.
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let mut client = hold_read_state_subscriptions(&mut test, "quiet", &["pane_1"]);
    for _ in 0..4 {
        let reads = test.hub.history_reads();
        for _ in 0..300 {
            test.hub
                .push(sample_event(EventKind::PaneUpdated, "pane_2"));
        }
        assert!(
            wait_for_history_reads(&test.hub, reads, 1),
            "the subscriber did not look at the history while 300 events about another pane arrived"
        );
    }
    test.hub.push(read_state_changed("pane_1"));
    let delivered = client
        .next_line(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert!(delivered.get("error").is_none(), "{delivered}");
    assert_eq!(delivered["data"]["pane_id"], "pane_1");
}

#[test]
fn stopping_the_server_releases_every_kind_of_held_connection_at_once() {
    // Shutdown wakes all, whatever the connection listens for: a per-pane subscription, an event
    // kind, an agent-status subscription. 30 s intervals, so a single `wake_waiters` must do it.
    let mut test = SocketTest::with_timing(Duration::from_secs(30), Duration::from_secs(30));
    let _read = hold_read_state_subscriptions(&mut test, "read", &["pane_1"]);
    let mut renamed = test.connect();
    renamed.subscribe("renamed", json!([{"type": "workspace.renamed"}]));
    renamed.assert_started("renamed");
    let mut status = test.connect();
    status.subscribe("status", idle_agent_status_subscription());
    reply_to_probe(test.app_request());
    status.assert_started("status");
    answer_pane_gets_for(&mut test, Duration::from_millis(400));

    test.running.store(false, Ordering::Relaxed);
    test.hub.wake_waiters();
    let deadline = Instant::now() + Duration::from_secs(5);
    while test.workers.iter().any(|worker| !worker.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let running: Vec<usize> = test
        .workers
        .iter()
        .enumerate()
        .filter(|(_, worker)| !worker.is_finished())
        .map(|(index, _)| index)
        .collect();
    assert!(
        running.is_empty(),
        "connections {running:?} kept running after the server stopped"
    );
}

#[test]
fn a_held_agent_status_subscription_notices_a_change_only_the_snapshot_sees_within_the_snapshot_interval(
) {
    // WHY: the snapshot is the safety net for a change that no event reports. It must not slow down
    // to the liveness interval (5 s) of a connection that has nothing else to do: with production
    // timing the change below reaches the subscriber within the 2.5 s window, not after 5 s.
    let mut test = SocketTest::new();
    let mut client = test.connect();
    client.subscribe("snapshot", idle_agent_status_subscription());
    reply_to_probe(test.app_request());
    client.assert_started("snapshot");
    answer_pane_gets_with_status_for(&mut test, Duration::from_millis(300), AgentStatus::Working);
    // The pane turns idle with no event: only a snapshot can tell.
    answer_pane_gets_with_status_for(&mut test, Duration::from_millis(2500), AgentStatus::Idle);
    let delivered = client
        .next_line(Instant::now() + Duration::from_millis(200))
        .unwrap();
    assert_eq!(delivered["event"], "pane.agent_status_changed");
    assert_eq!(delivered["data"]["agent_status"], "idle");
}

#[test]
fn an_agent_status_wait_notices_a_change_only_the_snapshot_sees_within_the_snapshot_interval() {
    // The same for `events.wait`: its blocking time is the shorter of the liveness interval (5 s)
    // and the next snapshot (1 s), so a change that no event reports ends the wait within the window.
    let mut test = SocketTest::new();
    let mut client = test.connect();
    client.send(json!({
        "id": "wait-snapshot",
        "method": "events.wait",
        "params": {
            "match_event": {
                "event": "pane_agent_status_changed",
                "pane_id": "pane_1",
                "agent_status": "idle"
            },
            "timeout_ms": 20_000
        }
    }));
    answer_pane_gets_with_status_for(&mut test, Duration::from_millis(300), AgentStatus::Working);
    answer_pane_gets_with_status_for(&mut test, Duration::from_millis(2500), AgentStatus::Idle);
    let response = client
        .next_line(Instant::now() + Duration::from_millis(200))
        .unwrap();
    assert_eq!(response["id"], "wait-snapshot");
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["event"]["data"]["agent_status"], "idle");
}
