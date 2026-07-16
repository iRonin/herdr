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

    match crate::session::stop_active_server() {
        Ok(()) => Ok(0),
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

    match restart_server_with(
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
    get_env: impl FnOnce(&str) -> Option<std::ffi::OsString>,
    resolve_target_socket: impl FnOnce() -> std::path::PathBuf,
    stop_server: impl FnOnce() -> Result<(), String>,
    start_and_attach: impl FnOnce() -> std::io::Result<()>,
) -> Result<(), String> {
    if let Some(hosting_socket) = get_env(crate::api::SOCKET_PATH_ENV_VAR) {
        let hosting_socket = std::path::PathBuf::from(hosting_socket);
        let target_socket = resolve_target_socket();
        if canonical_socket_paths_match(&hosting_socket, &target_socket) {
            return Err("server restart: refusing to restart the server that hosts this pane — stopping it would kill this command before the new server can start. Run it from a shell outside this session: herdr --session <name> server restart".to_string());
        }
    }

    stop_server()
        .map_err(|err| format!("server restart aborted: {err}\nThe server was not restarted."))?;

    start_and_attach()
        .map_err(|err| format!("server stopped but failed to restart and attach: {err}"))
}

fn canonical_socket_paths_match(left: &std::path::Path, right: &std::path::Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
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

        restart_server_with(
            |_| None,
            || panic!("target socket is irrelevant outside a hosted pane"),
            || {
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
        let target_socket = std::env::current_dir().unwrap().canonicalize().unwrap();
        let hosting_socket = target_socket.join("src").join("..");
        assert_ne!(
            hosting_socket, target_socket,
            "fixture must require canonicalization"
        );
        let stop_called = std::cell::Cell::new(false);
        let start_called = std::cell::Cell::new(false);

        let error = restart_server_with(
            |name| {
                assert_eq!(name, crate::api::SOCKET_PATH_ENV_VAR);
                Some(hosting_socket.clone().into_os_string())
            },
            || target_socket.clone(),
            || {
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
        let events = std::cell::RefCell::new(Vec::new());

        restart_server_with(
            |name| {
                assert_eq!(name, crate::api::SOCKET_PATH_ENV_VAR);
                None
            },
            || std::env::current_dir().unwrap(),
            || {
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
        let hosting_socket = std::env::current_dir().unwrap().canonicalize().unwrap();
        let target_socket = hosting_socket
            .parent()
            .expect("test working directory has a parent")
            .to_path_buf();
        let events = std::cell::RefCell::new(Vec::new());

        restart_server_with(
            |_| Some(hosting_socket.clone().into_os_string()),
            || target_socket.clone(),
            || {
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
    fn server_restart_does_not_start_when_stop_fails() {
        let start_called = std::cell::Cell::new(false);

        let error = restart_server_with(
            |_| None,
            || panic!("target socket is irrelevant outside a hosted pane"),
            || Err("server did not stop within 15000ms".to_string()),
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
        assert_eq!(
            error,
            "server restart aborted: server did not stop within 15000ms\nThe server was not restarted."
        );
    }

    #[test]
    fn server_restart_reports_attach_failure_after_stop() {
        let stop_called = std::cell::Cell::new(false);

        let error = restart_server_with(
            |_| None,
            || panic!("target socket is irrelevant outside a hosted pane"),
            || {
                stop_called.set(true);
                Ok(())
            },
            || Err(std::io::Error::other("attach failed")),
        )
        .unwrap_err();

        assert!(stop_called.get());
        assert_eq!(
            error,
            "server stopped but failed to restart and attach: attach failed"
        );
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
