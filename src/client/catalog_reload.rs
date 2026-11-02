use super::*;

pub(super) fn watch_profiles(
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
    local_session: String,
) {
    // One bounded read per second per client, independent of rendering and pane count.
    std::thread::spawn(move || {
        let mut previous = None;
        while !should_quit.load(Ordering::Acquire) {
            // Only this local session's machines: a change to another session's machine is no
            // reload here, and a machine moved into or out of this session is one.
            let current =
                endpoint::EndpointCatalog::load_profiles_for_local_session(&local_session);
            if previous.as_ref() != Some(&current) {
                previous = Some(current.clone());
                if event_tx
                    .blocking_send(ClientLoopEvent::EndpointCatalog(current))
                    .is_err()
                {
                    break;
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

// Only called between surface handoffs: removing a source must not invalidate an in-flight
// rollback. Connection attempts are independent and fenced by supervisor generations.
pub(super) fn apply_profiles(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    commands: &mut endpoint_commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    catalog: &mut endpoint::EndpointCatalog,
    profiles: Vec<endpoint::SavedSshEndpoint>,
    now: std::time::Instant,
) -> bool {
    if catalog.ssh == profiles {
        return false;
    }
    let previous_size = state
        .shell
        .as_ref()
        .map(|shell| shell.surface_size(state.reported_size.0, state.reported_size.1));
    let retired = supervisors.reconcile_profiles(&profiles, now);
    let active_removed = retired.contains(endpoints.active_id());
    for endpoint_id in retired {
        endpoints.disconnect(&endpoint_id);
        let cancelled = commands.disconnect(&endpoint_id);
        #[cfg(unix)]
        state.forget_endpoint_graphics(&endpoint_id);
        if let Some(shell) = state.shell.as_mut() {
            for request_id in cancelled {
                shell.cancel_endpoint_request(&request_id);
            }
            shell.retire_endpoint(&endpoint_id);
        }
    }
    catalog.ssh = profiles;
    if active_removed {
        endpoints.select_unavailable_local();
        catalog.select_local();
        state.freeze_presentation();
        if let Some(shell) = state.shell.as_mut() {
            shell.select_unavailable_local();
        }
    } else if catalog.selected_profile.as_ref().is_some_and(|selected| {
        !catalog
            .ssh
            .iter()
            .any(|profile| &profile.id == selected && profile.enabled)
    }) {
        catalog.select_local();
    }
    let pixel_mouse_available = state.pixel_mouse_available();
    if let Some(shell) = state.shell.as_mut() {
        shell.set_endpoint_catalog(&catalog.ssh);
        if endpoints.active_surface_available()
            && previous_size
                != Some(shell.surface_size(state.reported_size.0, state.reported_size.1))
        {
            shell.invalidate_pane_surface();
            endpoints.send(&client_shell_resize_message(
                shell,
                state.reported_size.0,
                state.reported_size.1,
                state.reported_cell_size.0,
                state.reported_cell_size.1,
                pixel_mouse_available,
            ));
        }
    }
    active_removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use endpoint::{ClientEndpointId, EndpointCatalog, EndpointRegistry, EndpointSupervisors};
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    struct Transport(Arc<AtomicUsize>);

    impl endpoint::EndpointTransport for Transport {
        fn send(&mut self, _: &ClientMessage) -> io::Result<()> {
            Ok(())
        }

        fn disconnect(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn state() -> ClientState {
        ClientState::test_new()
    }

    #[test]
    fn live_catalog_add_and_rename_keep_local_connection_and_selection() {
        let now = Instant::now();
        let mut state = state();
        let disconnected = Arc::new(AtomicUsize::new(0));
        let mut endpoints =
            EndpointRegistry::new(Transport(disconnected.clone()), 1, Default::default());
        let mut supervisors = EndpointSupervisors::new(&[], now);
        let mut commands = endpoint_commands::EndpointCommands::default();
        let mut catalog = EndpointCatalog::default();
        let mut profile = endpoint::SavedSshEndpoint::new("Build", "build", "main").unwrap();
        for label in ["Build", "Renamed"] {
            profile.label = label.into();
            assert!(!apply_profiles(
                &mut state,
                &mut endpoints,
                &mut commands,
                &mut supervisors,
                &mut catalog,
                vec![profile.clone()],
                now
            ));
            assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);
            assert!(endpoints.active_surface_available());
            assert_eq!(
                endpoints
                    .connection(&ClientEndpointId::Local)
                    .unwrap()
                    .generation,
                1
            );
            assert_eq!(catalog.selected_profile, None);
            assert_eq!(
                state
                    .shell
                    .as_ref()
                    .unwrap()
                    .endpoint_label(&ClientEndpointId::Ssh(profile.id.clone())),
                label
            );
        }
        assert_eq!(disconnected.load(Ordering::Relaxed), 0);
    }

    const SCOPED_ID: &str = "0123456789abcdef0123456789abcdef";

    fn write_scoped_catalog(path: &std::path::Path, local_sessions: &[&str]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            serde_json::json!({
                "version": 1,
                "ssh": [{
                    "id": SCOPED_ID, "label": "Build", "target": "build", "session": "main",
                    "enabled": true, "local_sessions": local_sessions,
                }],
            })
            .to_string(),
        )
        .unwrap();
    }

    /// One reload of a client of local session `legal`: the loader the watcher thread runs, then
    /// the application of what it found.
    fn reload_as_legal(
        path: &std::path::Path,
        state: &mut ClientState,
        endpoints: &mut EndpointRegistry,
        commands: &mut endpoint_commands::EndpointCommands,
        supervisors: &mut EndpointSupervisors,
        catalog: &mut EndpointCatalog,
        now: Instant,
    ) -> bool {
        let profiles =
            EndpointCatalog::load_profiles_from_path_for_local_session(path, "legal").unwrap();
        apply_profiles(
            state,
            endpoints,
            commands,
            supervisors,
            catalog,
            profiles,
            now,
        )
    }

    #[test]
    fn live_reload_applies_a_scope_change_in_both_directions_without_a_restart() {
        let path = std::env::temp_dir()
            .join(format!("herdr-scope-reload-{}", std::process::id()))
            .join("endpoints.json");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let now = Instant::now();
        let mut state = state();
        let remote = ClientEndpointId::Ssh(endpoint::ProfileId::parse(SCOPED_ID).unwrap());
        let disconnected = Arc::new(AtomicUsize::new(0));
        let mut endpoints =
            EndpointRegistry::new(Transport(disconnected.clone()), 1, Default::default());
        let mut supervisors = EndpointSupervisors::new(&[], now);
        let mut commands = endpoint_commands::EndpointCommands::default();
        let mut catalog = EndpointCatalog::default();
        let status = |state: &ClientState| state.shell.as_ref().unwrap().endpoint_status(&remote);

        // Scoped to the default session: this legal client has no such machine.
        write_scoped_catalog(&path, &["default"]);
        assert!(!reload_as_legal(
            &path,
            &mut state,
            &mut endpoints,
            &mut commands,
            &mut supervisors,
            &mut catalog,
            now
        ));
        assert!(catalog.ssh.is_empty());
        assert_eq!(status(&state), None);

        // Widened to include legal: the machine appears, Local stays connected and selected.
        write_scoped_catalog(&path, &["default", "legal"]);
        assert!(!reload_as_legal(
            &path,
            &mut state,
            &mut endpoints,
            &mut commands,
            &mut supervisors,
            &mut catalog,
            now
        ));
        assert_eq!(catalog.ssh.len(), 1);
        assert_eq!(
            status(&state),
            Some(endpoint::ClientEndpointStatus::Connecting)
        );
        assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);
        assert_eq!(disconnected.load(Ordering::Relaxed), 0);

        // The machine is the active one, then the scope narrows back to the default session:
        // it goes away at once and the client falls back to Local.
        catalog.select_ssh(&endpoint::ProfileId::parse(SCOPED_ID).unwrap());
        let remote_disconnects = Arc::new(AtomicUsize::new(0));
        endpoints.insert(
            remote.clone(),
            Transport(remote_disconnects.clone()),
            2,
            Default::default(),
            true,
        );
        endpoints.set_active(&remote);
        endpoints.unfreeze_input();
        write_scoped_catalog(&path, &["default"]);
        assert!(reload_as_legal(
            &path,
            &mut state,
            &mut endpoints,
            &mut commands,
            &mut supervisors,
            &mut catalog,
            now
        ));
        assert!(catalog.ssh.is_empty());
        assert_eq!(catalog.selected_profile, None);
        assert_eq!(status(&state), None);
        assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);
        assert!(endpoints.connection(&remote).is_none());
        assert_eq!(remote_disconnects.load(Ordering::Relaxed), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn live_catalog_remove_or_disable_active_machine_selects_local_without_input() {
        for local_online in [false, true] {
            for disable in [false, true] {
                let now = Instant::now();
                let mut state = state();
                let mut catalog = EndpointCatalog::default();
                let id = catalog.add_ssh("Build", "build", "main").unwrap();
                let remote = ClientEndpointId::Ssh(id.clone());
                catalog.select_ssh(&id);
                state
                    .shell
                    .as_mut()
                    .unwrap()
                    .set_endpoint_catalog(&catalog.ssh);
                let mut supervisors = EndpointSupervisors::new(&catalog.ssh, now);
                let mut endpoints = EndpointRegistry::empty();
                let local_disconnects = Arc::new(AtomicUsize::new(0));
                if local_online {
                    endpoints.insert(
                        ClientEndpointId::Local,
                        Transport(local_disconnects.clone()),
                        1,
                        Default::default(),
                        false,
                    );
                }
                let remote_disconnects = Arc::new(AtomicUsize::new(0));
                endpoints.insert(
                    remote.clone(),
                    Transport(remote_disconnects.clone()),
                    2,
                    Default::default(),
                    true,
                );
                endpoints.set_active(&remote);
                endpoints.unfreeze_input();
                let mut commands = endpoint_commands::EndpointCommands::default();
                let profiles = if disable {
                    let mut profiles = catalog.ssh.clone();
                    profiles[0].enabled = false;
                    profiles
                } else {
                    Vec::new()
                };
                assert!(apply_profiles(
                    &mut state,
                    &mut endpoints,
                    &mut commands,
                    &mut supervisors,
                    &mut catalog,
                    profiles,
                    now
                ));
                assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);
                assert!(!endpoints.active_surface_available());
                assert!(endpoints.connection(&remote).is_none());
                assert_eq!(
                    endpoints.connection(&ClientEndpointId::Local).is_some(),
                    local_online
                );
                assert!(state
                    .shell
                    .as_ref()
                    .unwrap()
                    .endpoint_is_active(&ClientEndpointId::Local));
                assert!(!state.shell.as_ref().unwrap().has_presented_surface());
                assert!(state.presentation_frozen);
                assert_eq!(catalog.selected_profile, None);
                assert_eq!(remote_disconnects.load(Ordering::Relaxed), 1);
                assert_eq!(local_disconnects.load(Ordering::Relaxed), 0);
                assert!(!supervisors.record_status(
                    &remote,
                    2,
                    endpoint::ClientEndpointStatus::Online,
                    now
                ));
            }
        }
    }
}
