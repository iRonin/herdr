use serde::Serialize;

use crate::client::endpoint::{EndpointCatalog, ProfileId};

const HELP: &str = "Usage:
  herdr machine list [--json]
  herdr machine add <ssh-target> --label <label> [--remote-session <name>] [--local-session <name>]...
  herdr machine rename <profile-id> --label <label>
  herdr machine scope <profile-id> (--local-session <name>... | --all-local-sessions)
  herdr machine remove <profile-id>
  herdr machine enable <profile-id>
  herdr machine disable <profile-id>

Add prepares the remote Herdr installation and starts its server before saving.
Missing or incompatible installations require approval in an interactive terminal.
Changes apply automatically to open local Herdr clients.
Removing or disabling a machine leaves its remote sessions running.
Saved machines contain only a label, SSH target, explicit Herdr session, enabled state, and
optionally the local sessions that show the machine.
A machine appears in every local session's window unless it is limited: --local-session names the
local sessions that show it (`herdr` is \"default\", `herdr --session legal` is \"legal\"; not the
remote session), scope replaces that list later, and --all-local-sessions removes the limit.
`herdr --machine` still reaches every saved machine.
SSH credentials and key material remain owned by OpenSSH.";

#[derive(Serialize)]
struct MachineListRow<'a> {
    id: &'a str,
    label: &'a str,
    target: &'a str,
    session: &'a str,
    enabled: bool,
    selected: bool,
    /// Local sessions that show the machine; empty means every local session.
    local_sessions: &'a [String],
}

pub(super) fn run_machine_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") => list(&args[1..]),
        Some("add") => add(&args[1..]),
        Some("rename") => rename(&args[1..]),
        Some("scope") => scope(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some("enable") => set_enabled(&args[1..], true),
        Some("disable") => set_enabled(&args[1..], false),
        Some("help" | "--help" | "-h") => {
            println!("{HELP}");
            Ok(0)
        }
        _ => {
            eprintln!("{HELP}");
            Ok(2)
        }
    }
}

fn list(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("usage: herdr machine list [--json]");
            return Ok(2);
        }
    };
    let catalog = load_catalog()?;
    let rows = catalog
        .ssh
        .iter()
        .map(|profile| MachineListRow {
            id: profile.id.as_str(),
            label: &profile.label,
            target: &profile.target,
            session: &profile.session,
            enabled: profile.enabled,
            selected: catalog.selected_profile.as_ref() == Some(&profile.id),
            local_sessions: &profile.local_sessions,
        })
        .collect::<Vec<_>>();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(std::io::Error::other)?
        );
        return Ok(0);
    }
    if rows.is_empty() {
        println!("No saved SSH machines.");
        return Ok(0);
    }
    for row in rows {
        let state = if row.enabled { "enabled" } else { "disabled" };
        // A sixth column only for a limited machine keeps every other line as it always was.
        let scope = if row.local_sessions.is_empty() {
            String::new()
        } else {
            format!("\tlocal sessions: {}", row.local_sessions.join(","))
        };
        println!(
            "{}\t{}\t{}\t{}\t{state}{scope}",
            row.id, row.label, row.target, row.session
        );
    }
    Ok(0)
}

#[derive(Debug, PartialEq, Eq)]
struct AddArgs {
    target: String,
    label: String,
    session: String,
    local_sessions: Vec<String>,
}

fn parse_add_args(args: &[String]) -> Result<AddArgs, String> {
    let args = super::expand_equals_args(args, &["--label", "--remote-session", "--local-session"]);
    let mut target = None;
    let mut label = None;
    let mut session = None;
    let mut local_sessions = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let (name, value) = match args[index].as_str() {
            "--label" | "--remote-session" | "--local-session" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(format!("missing value for {}", args[index]));
                };
                index += 2;
                (args[index - 2].as_str(), value.clone())
            }
            positional if !positional.starts_with('-') && target.is_none() => {
                target = Some(positional.to_owned());
                index += 1;
                continue;
            }
            unknown => {
                return Err(format!("unknown machine add option: {unknown}"));
            }
        };
        match name {
            "--label" if label.is_none() => label = Some(value),
            "--remote-session" if session.is_none() => session = Some(value),
            "--local-session" => local_sessions.push(value),
            "--remote-session" => {
                return Err("--remote-session can only be specified once".into());
            }
            "--label" => {
                return Err("--label can only be specified once".into());
            }
            _ => unreachable!("validated machine add option"),
        }
    }
    let target = target.ok_or_else(|| {
        "usage: herdr machine add <ssh-target> --label <label> [--remote-session <name>] [--local-session <name>]...".to_owned()
    })?;
    let label = label.ok_or_else(|| "--label is required".to_owned())?;
    let session = session.unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_owned());
    Ok(AddArgs {
        target,
        label,
        session,
        local_sessions,
    })
}

fn add(args: &[String]) -> std::io::Result<i32> {
    let AddArgs {
        target,
        label,
        session,
        local_sessions,
    } = match parse_add_args(args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return Ok(2);
        }
    };
    let mut catalog = load_catalog()?;
    // Checks every field, the local sessions included, before ssh is touched.
    match catalog.add_ssh_scoped(
        label.clone(),
        &target,
        session.clone(),
        local_sessions.clone(),
    ) {
        Ok(_) => {}
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    }
    if let Err(error) = crate::remote::prepare_saved_ssh(&target, &session) {
        eprintln!("error: {error}; machine was not saved");
        crate::remote::print_saved_ssh_error_hint(&error, &target);
        return Ok(1);
    }
    // Setup can wait for human approval. Do not overwrite catalog edits made meanwhile.
    let mut catalog = load_catalog().map_err(|error| {
        std::io::Error::other(format!(
            "remote prepared, but machine was not saved: {error}"
        ))
    })?;
    let id = match catalog.add_ssh_scoped(label, target, session, local_sessions) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    };
    store_catalog(&catalog).map_err(|error| {
        std::io::Error::other(format!(
            "remote prepared, but machine was not saved: {error}"
        ))
    })?;
    println!("Saved SSH machine {id}. Remote server is ready.");
    println!("Open Herdr clients connect automatically.");
    Ok(0)
}

const SCOPE_USAGE: &str =
    "usage: herdr machine scope <profile-id> (--local-session <name>... | --all-local-sessions)";

#[derive(Debug, PartialEq, Eq)]
struct ScopeArgs {
    id: String,
    /// Empty: every local session.
    local_sessions: Vec<String>,
}

fn parse_scope_args(args: &[String]) -> Result<ScopeArgs, String> {
    let args = super::expand_equals_args(args, &["--local-session"]);
    let mut id = None;
    let mut local_sessions = Vec::new();
    let mut all = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--local-session" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --local-session".into());
                };
                local_sessions.push(value.clone());
                index += 2;
            }
            "--all-local-sessions" if !all => {
                all = true;
                index += 1;
            }
            positional if !positional.starts_with('-') && id.is_none() => {
                id = Some(positional.to_owned());
                index += 1;
            }
            _ => return Err(SCOPE_USAGE.into()),
        }
    }
    // Exactly one of: a list of sessions, or all of them.
    match (id, all, local_sessions.is_empty()) {
        (Some(id), true, true) | (Some(id), false, false) => Ok(ScopeArgs { id, local_sessions }),
        _ => Err(SCOPE_USAGE.into()),
    }
}

fn scope(args: &[String]) -> std::io::Result<i32> {
    let ScopeArgs {
        id: raw_id,
        local_sessions,
    } = match parse_scope_args(args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return Ok(2);
        }
    };
    let id = match ProfileId::parse(raw_id) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    };
    let mut catalog = load_catalog()?;
    match catalog.set_local_sessions(&id, local_sessions.clone()) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("machine profile {id} was not found");
            return Ok(1);
        }
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    }
    store_catalog(&catalog)?;
    if local_sessions.is_empty() {
        println!("SSH machine {id} now appears in every local session.");
    } else {
        println!(
            "SSH machine {id} now appears only in local sessions: {}.",
            local_sessions.join(", ")
        );
    }
    Ok(0)
}

fn rename(args: &[String]) -> std::io::Result<i32> {
    let args = super::expand_equals_args(args, &["--label"]);
    let [raw_id, flag, label] = args.as_slice() else {
        eprintln!("usage: herdr machine rename <profile-id> --label <label>");
        return Ok(2);
    };
    if flag != "--label" {
        eprintln!("usage: herdr machine rename <profile-id> --label <label>");
        return Ok(2);
    }
    let id = match ProfileId::parse(raw_id.clone()) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    };
    let mut catalog = load_catalog()?;
    match catalog.rename_ssh(&id, label) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("machine profile {id} was not found");
            return Ok(1);
        }
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    }
    store_catalog(&catalog)?;
    println!("Renamed SSH machine {id}.");
    Ok(0)
}

fn remove(args: &[String]) -> std::io::Result<i32> {
    let Some(id) = one_profile_id(args, "usage: herdr machine remove <profile-id>")? else {
        return Ok(2);
    };
    let mut catalog = load_catalog()?;
    let previous_selection = catalog.selected_profile.clone();
    if !catalog.remove_ssh(&id) {
        eprintln!("machine profile {id} was not found");
        return Ok(1);
    }
    store_catalog(&catalog)?;
    if catalog.selected_profile != previous_selection {
        catalog.store_selection().map_err(std::io::Error::other)?;
    }
    println!("Removed SSH machine {id}.");
    Ok(0)
}

fn set_enabled(args: &[String], enabled: bool) -> std::io::Result<i32> {
    let action = if enabled { "enable" } else { "disable" };
    let usage = format!("usage: herdr machine {action} <profile-id>");
    let Some(id) = one_profile_id(args, &usage)? else {
        return Ok(2);
    };
    let mut catalog = load_catalog()?;
    let previous_selection = catalog.selected_profile.clone();
    if !catalog.set_enabled(&id, enabled) {
        eprintln!("machine profile {id} was not found");
        return Ok(1);
    }
    store_catalog(&catalog)?;
    if catalog.selected_profile != previous_selection {
        catalog.store_selection().map_err(std::io::Error::other)?;
    }
    println!(
        "{} SSH machine {id}.",
        if enabled { "Enabled" } else { "Disabled" }
    );
    Ok(0)
}

fn one_profile_id(args: &[String], usage: &str) -> std::io::Result<Option<ProfileId>> {
    let [raw] = args else {
        eprintln!("{usage}");
        return Ok(None);
    };
    match ProfileId::parse(raw.clone()) {
        Ok(id) => Ok(Some(id)),
        Err(error) => {
            eprintln!("error: {error}");
            Ok(None)
        }
    }
}

fn load_catalog() -> std::io::Result<EndpointCatalog> {
    EndpointCatalog::load().map_err(std::io::Error::other)
}

fn store_catalog(catalog: &EndpointCatalog) -> std::io::Result<()> {
    catalog.store_profiles().map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_parser_preserves_values_across_argument_orders() {
        for (args, session) in [
            (vec!["--label", "coder", "workstation.coder"], "default"),
            (vec!["workstation.coder", "--label", "coder"], "default"),
            (
                vec![
                    "--remote-session",
                    "agents",
                    "workstation.coder",
                    "--label",
                    "coder",
                ],
                "agents",
            ),
            (
                vec![
                    "--label=coder",
                    "--remote-session=agents",
                    "workstation.coder",
                ],
                "agents",
            ),
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(
                parse_add_args(&args).unwrap(),
                AddArgs {
                    target: "workstation.coder".into(),
                    label: "coder".into(),
                    session: session.into(),
                    local_sessions: Vec::new(),
                },
                "{args:?}"
            );
        }
    }

    #[test]
    fn add_parser_collects_repeatable_local_sessions_in_any_spelling_and_order() {
        for args in [
            vec![
                "--local-session",
                "default",
                "--label",
                "coder",
                "--local-session=legal",
                "workstation.coder",
            ],
            vec![
                "workstation.coder",
                "--label=coder",
                "--local-session=default",
                "--local-session",
                "legal",
            ],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let parsed = parse_add_args(&args).unwrap();
            assert_eq!(parsed.local_sessions, ["default", "legal"], "{args:?}");
            assert_eq!(
                parsed.session, "default",
                "the remote session is not touched"
            );
        }
        let args = ["workstation.coder", "--label", "coder", "--local-session"].map(str::to_owned);
        assert!(parse_add_args(&args).is_err());
    }

    #[test]
    fn scope_parser_wants_a_machine_and_exactly_a_list_or_all() {
        let parse = |args: &[&str]| {
            parse_scope_args(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
        };
        assert_eq!(
            parse(&["abc", "--local-session", "default", "--local-session=legal"]).unwrap(),
            ScopeArgs {
                id: "abc".into(),
                local_sessions: vec!["default".into(), "legal".into()],
            }
        );
        // Flags before the machine, and the empty list meaning "every local session".
        assert_eq!(
            parse(&["--all-local-sessions", "abc"]).unwrap(),
            ScopeArgs {
                id: "abc".into(),
                local_sessions: Vec::new(),
            }
        );
        for bad in [
            vec![],
            vec!["abc"],
            vec!["--local-session", "default"],
            vec!["--all-local-sessions"],
            vec!["abc", "--all-local-sessions", "--local-session", "default"],
            vec!["abc", "def", "--local-session", "default"],
            vec!["abc", "--local-session"],
            vec!["abc", "--all-local-sessions", "--all-local-sessions"],
            vec!["abc", "--local-sessions", "default"],
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn add_parser_rejects_incomplete_duplicate_and_extra_arguments() {
        for args in [
            vec![],
            vec!["--label", "coder"],
            vec!["workstation.coder"],
            vec!["workstation.coder", "--label"],
            vec!["workstation.coder", "--label", "coder", "--remote-session"],
            vec!["--label", "coder", "--label", "other", "workstation.coder"],
            vec![
                "workstation.coder",
                "--label",
                "coder",
                "--remote-session",
                "a",
                "--remote-session",
                "b",
            ],
            vec!["--label", "coder", "workstation.coder", "other-host"],
            vec!["--unknown", "workstation.coder", "--label", "coder"],
            vec!["--label", "--remote-session", "agents", "workstation.coder"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(parse_add_args(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn profile_id_parser_rejects_target_text() {
        assert!(one_profile_id(&["build.example".into()], "usage")
            .unwrap()
            .is_none());
    }

    #[test]
    fn list_rows_do_not_have_credential_fields() {
        let encoded = serde_json::to_string(&MachineListRow {
            id: "0123456789abcdef0123456789abcdef",
            label: "Build",
            target: "dev@build",
            session: "agents",
            enabled: true,
            selected: false,
            local_sessions: &["default".to_owned()],
        })
        .unwrap();
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("key"));
        assert!(
            encoded.contains(r#""local_sessions":["default"]"#),
            "{encoded}"
        );
    }
}
