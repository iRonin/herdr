use super::*;

fn read_state_events(event_hub: &api::EventHub, since: u64) -> Vec<api::schema::PaneReadState> {
    event_hub
        .events_after(since)
        .into_iter()
        .filter_map(|(_, event)| match event.data {
            api::schema::EventData::PaneReadStateChanged { read_state } => Some(read_state),
            _ => None,
        })
        .collect()
}

#[test]
fn a_read_state_request_announces_a_pending_change_before_answering() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("one")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
    let public_id = server.app.public_pane_id(0, pane_id).unwrap();
    // The last loop checkpoint recorded the pane as read.
    server.app.sync_read_state_events();

    // Something earlier in the same loop turn marked it unread; no checkpoint has run since.
    server.app.state.workspaces[0]
        .pane_state_mut(pane_id)
        .unwrap()
        .seen = false;
    let sequence = event_hub.current_sequence();

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "read-state".into(),
            method: api::schema::Method::PaneReadState(api::schema::PaneTarget {
                pane_id: public_id.clone(),
            }),
        },
        peer_process: None,
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });
    let response: serde_json::Value =
        serde_json::from_str(&response_rx.recv().expect("read state response")).unwrap();

    let returned: api::schema::PaneReadState =
        serde_json::from_value(response["result"]["read_state"].clone()).unwrap();
    assert!(!returned.seen);
    assert_eq!(
        read_state_events(&event_hub, sequence),
        vec![returned],
        "the value the request returned must already have been announced"
    );
}
