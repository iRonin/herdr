use clap::{Arg, ArgGroup, Command};

use super::{flag, json_flag, option, repeatable_option};

pub(super) fn command() -> Command {
    Command::new("machine")
        .about("Manage saved SSH machines")
        .subcommand(
            Command::new("list")
                .about("List saved SSH machines")
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("add")
                .about("Prepare the remote Herdr server and save an SSH machine")
                .arg(
                    Arg::new("ssh-target")
                        .value_name("SSH_TARGET")
                        .required(true),
                )
                .arg(
                    option("label", "LABEL")
                        .required(true)
                        .help("Set the machine label shown in the sidebar"),
                )
                .arg(
                    option("remote-session", "NAME")
                        .help("Set the explicit Herdr session on the remote machine"),
                )
                .arg(repeatable_option("local-session", "NAME").help(
                    "Show the machine only in this local session (repeatable; default: every local session)",
                )),
        )
        .subcommand(
            profile_command("rename", "Rename a saved SSH machine").arg(
                option("label", "LABEL")
                    .required(true)
                    .help("Set the machine label shown in the sidebar"),
            ),
        )
        .subcommand(
            profile_command(
                "scope",
                "Limit a saved SSH machine to some local sessions, or to all of them",
            )
            .arg(
                repeatable_option("local-session", "NAME")
                    .help("Show the machine only in this local session (repeatable)"),
            )
            .arg(
                flag("all-local-sessions")
                    .help("Show the machine in every local session again")
                    .conflicts_with("local-session"),
            )
            .group(
                ArgGroup::new("scope")
                    .args(["local-session", "all-local-sessions"])
                    .required(true),
            ),
        )
        .subcommand(profile_command("remove", "Remove a saved SSH machine"))
        .subcommand(profile_command("enable", "Enable a saved SSH machine"))
        .subcommand(profile_command("disable", "Disable a saved SSH machine"))
}

fn profile_command(name: &'static str, about: &'static str) -> Command {
    Command::new(name).about(about).arg(
        Arg::new("profile-id")
            .value_name("PROFILE_ID")
            .required(true),
    )
}
