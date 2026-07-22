use crate::api::schema::{EmptyParams, Method, Request, ServerLiveHandoffParams};

pub(super) fn run_server_command(args: &[String]) -> std::io::Result<Option<i32>> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        return Ok(None);
    };

    match subcommand {
        "stop" => server_stop(&args[1..]).map(Some),
        "restart" => server_restart(&args[1..]).map(Some),
        "live-handoff" => server_live_handoff(&args[1..]).map(Some),
        "--handoff-import" => Ok(None),
        "reload-config" => server_reload_config(&args[1..]).map(Some),
        "agent-manifests" => server_agent_manifests(&args[1..]).map(Some),
        "update-agent-manifests" => server_update_agent_manifests(&args[1..]).map(Some),
        "reload-agent-manifests" => server_reload_agent_manifests(&args[1..]).map(Some),
        "help" | "--help" | "-h" => {
            print_server_help();
            Ok(Some(0))
        }
        _ => {
            print_server_help();
            Ok(Some(2))
        }
    }
}

fn server_stop(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: herdr server stop");
        return Ok(2);
    }

    let timeout = load_server_stop_timeout();
    match crate::session::stop_active_server(timeout) {
        Ok(()) => {
            eprintln!("\x1b[2mherdr · iRonin fork\x1b[0m");
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn server_restart(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: herdr server restart");
        return Ok(2);
    }

    let config = crate::config::Config::load().config;
    match restart_server_with(
        &config,
        |name| std::env::var_os(name),
        crate::session::active_api_socket_path,
        crate::session::stop_active_server,
        crate::server::autodetect::auto_detect_launch,
    ) {
        Ok(()) => Ok(0),
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn restart_server_with(
    config: &crate::config::Config,
    get_env: impl FnOnce(&str) -> Option<std::ffi::OsString>,
    resolve_target_socket: impl FnOnce() -> std::path::PathBuf,
    stop_server: impl FnOnce(std::time::Duration) -> Result<(), String>,
    start_and_attach: impl FnOnce() -> std::io::Result<()>,
) -> Result<(), String> {
    if let Some(hosting_socket) = get_env(crate::api::SOCKET_PATH_ENV_VAR) {
        let hosting_socket = std::path::PathBuf::from(hosting_socket);
        let target_socket = resolve_target_socket();
        if canonical_socket_paths_match(&hosting_socket, &target_socket) {
            return Err("server restart: refusing to restart the server that hosts this pane — stopping it would kill this command before the new server can start. Run it from a shell outside this session: herdr --session <name> server restart".to_string());
        }
    }

    let timeout = server_stop_timeout(config);
    stop_server(timeout).map_err(|err| {
        let mut message = format!("server restart aborted: {err}\nThe server was not restarted.");
        if err.contains("did not stop within") {
            let max_timeout =
                std::time::Duration::from_millis(crate::config::MAX_STOP_WAIT_TIMEOUT_MS);
            if timeout < max_timeout {
                message.push_str("\nRaise session.stop_timeout_ms and try again.");
            } else {
                message.push_str(&format!(
                    "\nsession.stop_timeout_ms is already at its {}ms maximum.",
                    crate::config::MAX_STOP_WAIT_TIMEOUT_MS
                ));
            }
        }
        message
    })?;

    start_and_attach()
        .map_err(|err| format!("server stopped but failed to restart and attach: {err}"))
}

fn canonical_socket_paths_match(left: &std::path::Path, right: &std::path::Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn load_server_stop_timeout() -> std::time::Duration {
    let config = crate::config::Config::load().config;
    server_stop_timeout(&config)
}

fn server_stop_timeout(config: &crate::config::Config) -> std::time::Duration {
    let configured_ms = config.session.stop_timeout_ms;
    let timeout_ms = if configured_ms == 0 {
        crate::config::DEFAULT_STOP_WAIT_TIMEOUT_MS
    } else {
        configured_ms.min(crate::config::MAX_STOP_WAIT_TIMEOUT_MS)
    };
    std::time::Duration::from_millis(timeout_ms)
}

fn server_reload_config(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: herdr server reload-config");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:server:reload-config".into(),
        method: Method::ServerReloadConfig(EmptyParams::default()),
    })?)
}

fn server_agent_manifests(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("usage: herdr server agent-manifests [--json]");
            return Ok(2);
        }
    };

    let response = super::send_request(&Request {
        id: "cli:server:agent-manifests".into(),
        method: Method::ServerAgentManifests(EmptyParams::default()),
    })?;
    if json || response.get("error").is_some() {
        return super::print_response(&response);
    }

    print_agent_manifest_status(&response);
    Ok(0)
}

fn server_reload_agent_manifests(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: herdr server reload-agent-manifests");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:server:reload-agent-manifests".into(),
        method: Method::ServerReloadAgentManifests(EmptyParams::default()),
    })?)
}

fn server_update_agent_manifests(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("usage: herdr server update-agent-manifests [--json]");
            return Ok(2);
        }
    };

    let response = match update_agent_manifest_status(super::send_request, || {
        crate::detect::manifest_update::check_and_update().map(|_| ())
    })? {
        Ok(response) => response,
        Err(err) => {
            if json {
                return super::print_response(&agent_manifest_update_error_response(&err));
            }
            eprintln!("failed to update agent detection manifests: {err}");
            return Ok(1);
        }
    };
    if json || response.get("error").is_some() {
        return super::print_response(&response);
    }

    print_agent_manifest_status(&response);
    Ok(0)
}

fn update_agent_manifest_status(
    mut send_request: impl FnMut(&Request) -> std::io::Result<serde_json::Value>,
    update_manifests: impl FnOnce() -> Result<(), String>,
) -> std::io::Result<Result<serde_json::Value, String>> {
    if let Err(err) = update_manifests() {
        return Ok(Err(err));
    }

    let reload_response = send_request(&Request {
        id: "cli:server:reload-agent-manifests".into(),
        method: Method::ServerReloadAgentManifests(EmptyParams::default()),
    })?;
    if reload_response.get("error").is_some() {
        return Ok(Ok(reload_response));
    }

    send_request(&Request {
        id: "cli:server:agent-manifests".into(),
        method: Method::ServerAgentManifests(EmptyParams::default()),
    })
    .map(Ok)
}

fn agent_manifest_update_error_response(err: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "cli:server:update-agent-manifests",
        "error": {
            "code": "agent_manifest_update_failed",
            "message": err,
        }
    })
}

fn print_agent_manifest_status(response: &serde_json::Value) {
    let result = &response["result"];
    let last_check = result["last_check_unix"]
        .as_u64()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "never".to_string());
    let last_result = result["last_result"].as_str().unwrap_or("not checked");
    println!("last check: {last_check}");
    println!("result: {last_result}");
    println!();

    let Some(manifests) = result["manifests"].as_array() else {
        return;
    };
    for manifest in manifests {
        let agent = manifest["agent"].as_str().unwrap_or("-");
        let source = manifest["source_kind"].as_str().unwrap_or("-");
        let active_version = manifest["active_version"].as_str().unwrap_or("-");
        let remote_version = manifest["cached_remote_version"].as_str().unwrap_or("-");
        let remote_result = manifest["remote_update_result"]
            .as_str()
            .unwrap_or("not checked");
        let local_override_shadowing_remote = manifest["local_override_shadowing_remote"]
            .as_bool()
            .unwrap_or(false);
        let marker = if local_override_shadowing_remote {
            "!"
        } else if manifest["remote_update_error"].as_str().is_some() {
            "x"
        } else {
            " "
        };
        println!(
            "{marker} {agent:<9} {source:<14} active {active_version:<14} remote {remote_version:<14} {remote_result}"
        );
        if let Some(error) = manifest["remote_update_error"].as_str() {
            println!("  {error}");
        } else if local_override_shadowing_remote {
            println!("  local override shadows cached remote rules");
        } else if let Some(warning) = manifest["warning"].as_str() {
            println!("  {warning}");
        }
    }
}

fn server_live_handoff(args: &[String]) -> std::io::Result<i32> {
    let Some(params) = parse_live_handoff_params(args) else {
        eprintln!(
            "usage: herdr server live-handoff [--import-exe <path>] [--expected-protocol <n>] [--expected-version <version>]"
        );
        return Ok(2);
    };

    // Live handoff is itself a protocol-mismatch recovery path, so it must
    // reach the running server without the normal CLI compatibility guard.
    let response = super::send_request_unchecked(&Request {
        id: "cli:server:live-handoff".into(),
        method: Method::ServerLiveHandoff(params),
    })?;
    if response.get("error").is_some() {
        let rendered = serde_json::to_string(&response).unwrap_or_else(|err| {
            format!(
                "{{\"error\":{{\"code\":\"render_failed\",\"message\":\"failed to render error response: {err}\"}}}}"
            )
        });
        eprintln!("{rendered}");
        return Ok(1);
    }

    eprintln!(
        "live handoff complete; server log: {}",
        crate::session::data_dir()
            .join("herdr-server.log")
            .display()
    );
    Ok(0)
}

fn parse_live_handoff_params(args: &[String]) -> Option<ServerLiveHandoffParams> {
    let mut params = ServerLiveHandoffParams::default();
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        let (flag, value) = if let Some((flag, value)) = arg.split_once('=') {
            (flag, Some(value.to_string()))
        } else {
            let value = args.get(idx + 1).cloned();
            idx += 1;
            (arg.as_str(), value)
        };
        let value = value?;
        match flag {
            "--import-exe" => params.import_exe = Some(value),
            "--expected-protocol" => {
                params.expected_protocol = Some(value.parse().ok()?);
            }
            "--expected-version" => params.expected_version = Some(value),
            _ => return None,
        }
        idx += 1;
    }
    Some(params)
}

fn print_server_help() {
    eprintln!("herdr server commands:");
    eprintln!("  herdr server                run as headless server");
    eprintln!("  herdr server stop           stop the running server via the API socket");
    eprintln!(
        "  herdr server restart        stop, start, and attach (run from outside the target session)"
    );
    eprintln!("  herdr server live-handoff   hand off live panes to a new local server");
    eprintln!("  herdr server reload-config  reload config.toml in the running server");
    eprintln!("  herdr server agent-manifests [--json]  show agent detection manifest status");
    eprintln!("  herdr server update-agent-manifests [--json]  fetch and reload agent detection manifests");
    eprintln!("  herdr server reload-agent-manifests  reload agent detection manifests in the running server");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_restart_stops_before_starting_and_attaching() {
        let events = std::cell::RefCell::new(Vec::new());
        let config = crate::config::Config::default();

        restart_server_with(
            &config,
            |_| None,
            || panic!("target socket is irrelevant outside a hosted pane"),
            |timeout| {
                assert_eq!(timeout, std::time::Duration::from_millis(15_000));
                events.borrow_mut().push("stop");
                Ok(())
            },
            || {
                events.borrow_mut().push("start-and-attach");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(*events.borrow(), ["stop", "start-and-attach"]);
    }

    #[test]
    fn server_restart_refuses_when_target_server_hosts_current_pane() {
        let config = crate::config::Config::default();
        let target_socket = std::env::current_dir().unwrap().canonicalize().unwrap();
        let hosting_socket = target_socket.join("src").join("..");
        assert_ne!(
            hosting_socket, target_socket,
            "fixture must require canonicalization"
        );
        let stop_called = std::cell::Cell::new(false);
        let start_called = std::cell::Cell::new(false);

        let error = restart_server_with(
            &config,
            |name| {
                assert_eq!(name, crate::api::SOCKET_PATH_ENV_VAR);
                Some(hosting_socket.clone().into_os_string())
            },
            || target_socket.clone(),
            |_| {
                stop_called.set(true);
                Ok(())
            },
            || {
                start_called.set(true);
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(
            error,
            "server restart: refusing to restart the server that hosts this pane — stopping it would kill this command before the new server can start. Run it from a shell outside this session: herdr --session <name> server restart"
        );
        assert!(!stop_called.get(), "guard must run before stop");
        assert!(!start_called.get(), "refused restart must not start");
    }

    #[test]
    fn server_restart_proceeds_when_hosting_socket_is_unset() {
        let config = crate::config::Config::default();
        let events = std::cell::RefCell::new(Vec::new());

        restart_server_with(
            &config,
            |name| {
                assert_eq!(name, crate::api::SOCKET_PATH_ENV_VAR);
                None
            },
            || std::env::current_dir().unwrap(),
            |_| {
                events.borrow_mut().push("stop");
                Ok(())
            },
            || {
                events.borrow_mut().push("start-and-attach");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(*events.borrow(), ["stop", "start-and-attach"]);
    }

    #[test]
    fn server_restart_proceeds_when_hosted_by_a_different_server() {
        let config = crate::config::Config::default();
        let hosting_socket = std::env::current_dir().unwrap().canonicalize().unwrap();
        let target_socket = hosting_socket
            .parent()
            .expect("test working directory has a parent")
            .to_path_buf();
        let events = std::cell::RefCell::new(Vec::new());

        restart_server_with(
            &config,
            |_| Some(hosting_socket.clone().into_os_string()),
            || target_socket.clone(),
            |_| {
                events.borrow_mut().push("stop");
                Ok(())
            },
            || {
                events.borrow_mut().push("start-and-attach");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(*events.borrow(), ["stop", "start-and-attach"]);
    }

    #[test]
    fn server_restart_timeout_does_not_start_and_suggests_raising_timeout() {
        let config: crate::config::Config =
            toml::from_str("[session]\nstop_timeout_ms = 25\n").unwrap();
        let start_called = std::cell::Cell::new(false);

        let error = restart_server_with(
            &config,
            |_| None,
            || panic!("target socket is irrelevant outside a hosted pane"),
            |timeout| {
                Err(format!(
                    "server did not stop within {}ms; sockets are still reachable",
                    timeout.as_millis()
                ))
            },
            || {
                start_called.set(true);
                Ok(())
            },
        )
        .unwrap_err();

        assert!(
            !start_called.get(),
            "restart must not start over a live server"
        );
        assert!(error.contains("server restart aborted"), "{error}");
        assert!(error.contains("server was not restarted"), "{error}");
        assert!(error.contains("Raise session.stop_timeout_ms"), "{error}");
    }

    #[test]
    fn server_restart_timeout_at_cap_does_not_suggest_impossible_raise() {
        let config: crate::config::Config =
            toml::from_str("[session]\nstop_timeout_ms = 500000\n").unwrap();

        let error = restart_server_with(
            &config,
            |_| None,
            || panic!("target socket is irrelevant outside a hosted pane"),
            |timeout| {
                Err(format!(
                    "server did not stop within {}ms; sockets are still reachable",
                    timeout.as_millis()
                ))
            },
            || panic!("restart must not start over a live server"),
        )
        .unwrap_err();

        assert!(!error.contains("Raise session.stop_timeout_ms"), "{error}");
        assert!(error.contains("300000ms maximum"), "{error}");
    }

    #[test]
    fn server_restart_succeeds_with_raised_stop_timeout() {
        let config: crate::config::Config =
            toml::from_str("[session]\nstop_timeout_ms = 45000\n").unwrap();
        let observed_timeout = std::cell::Cell::new(std::time::Duration::ZERO);
        let start_called = std::cell::Cell::new(false);

        restart_server_with(
            &config,
            |_| None,
            || panic!("target socket is irrelevant outside a hosted pane"),
            |timeout| {
                observed_timeout.set(timeout);
                Ok(())
            },
            || {
                start_called.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            observed_timeout.get(),
            std::time::Duration::from_millis(45_000)
        );
        assert!(start_called.get());
    }

    #[test]
    fn server_stop_timeout_uses_configured_value_and_default() {
        let default_config = crate::config::Config::default();
        assert_eq!(
            server_stop_timeout(&default_config),
            std::time::Duration::from_millis(crate::config::DEFAULT_STOP_WAIT_TIMEOUT_MS)
        );

        let config: crate::config::Config = toml::from_str(
            r#"
[session]
stop_timeout_ms = 5000
"#,
        )
        .unwrap();
        assert_eq!(
            server_stop_timeout(&config),
            std::time::Duration::from_millis(5_000)
        );
    }

    #[test]
    fn server_stop_timeout_defaults_zero_and_clamps_oversized_values() {
        assert_eq!(crate::config::MAX_STOP_WAIT_TIMEOUT_MS, 300_000);

        let mut config = crate::config::Config::default();
        config.session.stop_timeout_ms = 0;
        assert_eq!(
            server_stop_timeout(&config),
            std::time::Duration::from_millis(crate::config::DEFAULT_STOP_WAIT_TIMEOUT_MS)
        );

        config.session.stop_timeout_ms = i64::MAX as u64;
        assert_eq!(
            server_stop_timeout(&config),
            std::time::Duration::from_millis(crate::config::MAX_STOP_WAIT_TIMEOUT_MS)
        );
    }

    #[test]
    fn load_server_stop_timeout_reads_config_and_defaults_when_absent() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "herdr-server-stop-config-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);

        assert_eq!(
            load_server_stop_timeout(),
            std::time::Duration::from_millis(crate::config::DEFAULT_STOP_WAIT_TIMEOUT_MS)
        );

        std::fs::write(&path, "[session]\nstop_timeout_ms = 5000\n").unwrap();
        assert_eq!(
            load_server_stop_timeout(),
            std::time::Duration::from_millis(5_000)
        );

        std::fs::write(&path, "[session]\nstop_timeout_ms = 0\n").unwrap();
        assert_eq!(
            load_server_stop_timeout(),
            std::time::Duration::from_millis(crate::config::DEFAULT_STOP_WAIT_TIMEOUT_MS)
        );

        std::fs::write(
            &path,
            format!(
                "[session]\nstop_timeout_ms = {}\n",
                crate::config::MAX_STOP_WAIT_TIMEOUT_MS + 1
            ),
        )
        .unwrap();
        assert_eq!(
            load_server_stop_timeout(),
            std::time::Duration::from_millis(crate::config::MAX_STOP_WAIT_TIMEOUT_MS)
        );

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn server_stop_timeout_falls_back_when_config_is_invalid() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "herdr-server-stop-invalid-config-{}.toml",
            std::process::id()
        ));
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);

        let invalid_config = "[session\nstop_timeout_ms = 'broken'";
        std::fs::write(&path, invalid_config).unwrap();
        assert_eq!(
            load_server_stop_timeout(),
            std::time::Duration::from_millis(crate::config::DEFAULT_STOP_WAIT_TIMEOUT_MS),
            "invalid config should fall back: {invalid_config:?}"
        );

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn update_agent_manifest_status_fetches_reloads_then_reads_status() {
        let mut methods = Vec::new();
        let response = update_agent_manifest_status(
            |request| {
                methods.push(request.method.clone());
                match &request.method {
                    Method::ServerReloadAgentManifests(_) => Ok(serde_json::json!({
                        "id": request.id,
                        "result": { "type": "agent_manifest_reload", "manifests": [] }
                    })),
                    Method::ServerAgentManifests(_) => Ok(serde_json::json!({
                        "id": request.id,
                        "result": {
                            "type": "agent_manifest_status",
                            "last_result": "checked",
                            "manifests": []
                        }
                    })),
                    _ => panic!("unexpected request"),
                }
            },
            || Ok(()),
        )
        .unwrap()
        .unwrap();

        assert_eq!(response["result"]["type"], "agent_manifest_status");
        assert_eq!(
            methods,
            vec![
                Method::ServerReloadAgentManifests(EmptyParams::default()),
                Method::ServerAgentManifests(EmptyParams::default())
            ]
        );
    }

    #[test]
    fn update_agent_manifest_status_skips_server_when_fetch_fails() {
        let response = update_agent_manifest_status(
            |_request| panic!("server should not be called after fetch failure"),
            || Err("network unavailable".to_string()),
        )
        .unwrap();

        assert_eq!(response, Err("network unavailable".to_string()));
        assert_eq!(
            agent_manifest_update_error_response("network unavailable")["error"]["code"],
            "agent_manifest_update_failed"
        );
    }

    #[test]
    fn update_agent_manifest_status_stops_after_reload_error() {
        let mut methods = Vec::new();
        let response = update_agent_manifest_status(
            |request| {
                methods.push(request.method.clone());
                Ok(serde_json::json!({
                    "id": request.id,
                    "error": {
                        "code": "reload_failed",
                        "message": "reload failed"
                    }
                }))
            },
            || Ok(()),
        )
        .unwrap()
        .unwrap();

        assert_eq!(response["error"]["code"], "reload_failed");
        assert_eq!(
            methods,
            vec![Method::ServerReloadAgentManifests(EmptyParams::default())]
        );
    }

    #[test]
    fn live_handoff_params_parse_remote_update_fields() {
        let args = vec![
            "--import-exe".to_string(),
            "/home/me/.local/bin/herdr".to_string(),
            "--expected-protocol=9".to_string(),
            "--expected-version".to_string(),
            "0.6.2".to_string(),
        ];

        let params = parse_live_handoff_params(&args).expect("params");

        assert_eq!(
            params.import_exe.as_deref(),
            Some("/home/me/.local/bin/herdr")
        );
        assert_eq!(params.expected_protocol, Some(9));
        assert_eq!(params.expected_version.as_deref(), Some("0.6.2"));
    }
}
