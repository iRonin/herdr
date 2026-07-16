use std::path::Path;

use serde::{Deserialize, Serialize};

const MAX_SESSION_ID_LEN: usize = 512;
const MAX_SESSION_PATH_LEN: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionRef {
    pub kind: AgentSessionRefKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionRefKind {
    Id,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResumePlan {
    pub agent: String,
    pub argv: Vec<String>,
    pub dedupe_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAgentSession {
    pub source: String,
    pub agent: String,
    pub session_ref: AgentSessionRef,
}

impl AgentSessionRef {
    pub fn id(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        valid_session_id(&value).then_some(Self {
            kind: AgentSessionRefKind::Id,
            value,
        })
    }

    pub fn path(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        valid_session_path(&value).then_some(Self {
            kind: AgentSessionRefKind::Path,
            value,
        })
    }
}

pub fn session_ref_from_report(
    source: &str,
    agent: &str,
    agent_session_id: Option<String>,
    _agent_session_path: Option<String>,
) -> Option<AgentSessionRef> {
    if !is_official_agent_source(source, agent) {
        return None;
    }

    if agent == "pi" || agent == "omp" {
        return _agent_session_path
            .and_then(AgentSessionRef::path)
            .or_else(|| agent_session_id.and_then(AgentSessionRef::id));
    }

    agent_session_id.and_then(AgentSessionRef::id)
}

pub fn normalize_session_start_source(value: Option<String>) -> Option<String> {
    match value.as_deref().map(str::trim) {
        Some(source @ ("startup" | "resume" | "clear" | "compact" | "new" | "fork")) => {
            Some(source.to_string())
        }
        _ => None,
    }
}

pub fn is_reserved_native_state_source(source: &str, agent: &str) -> bool {
    matches!(
        (source, agent),
        ("herdr:claude", "claude")
            | ("herdr:codex", "codex")
            | ("herdr:copilot", "copilot")
            | ("herdr:devin", "devin")
            | ("herdr:droid", "droid")
            | ("herdr:qodercli", "qodercli")
            | ("herdr:cursor", "cursor")
    )
}

pub fn session_ref_from_snapshot(
    source: &str,
    agent: &str,
    kind: AgentSessionRefKind,
    value: &str,
) -> Option<PersistedAgentSession> {
    if !is_official_agent_source(source, agent) {
        return None;
    }
    let session_ref = match (agent, kind) {
        ("pi" | "omp", AgentSessionRefKind::Path) => AgentSessionRef::path(value)?,
        (_, AgentSessionRefKind::Id) => AgentSessionRef::id(value)?,
        _ => return None,
    };
    Some(PersistedAgentSession {
        source: source.to_string(),
        agent: agent.to_string(),
        session_ref,
    })
}

pub fn plan(source: &str, agent: &str, session_ref: &AgentSessionRef) -> Option<AgentResumePlan> {
    if !is_official_agent_source(source, agent) {
        return None;
    }

    let argv = match (source, agent, session_ref.kind) {
        ("herdr:claude", "claude", AgentSessionRefKind::Id) => {
            vec![
                "claude".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:codex", "codex", AgentSessionRefKind::Id) => {
            vec!["codex".into(), "resume".into(), session_ref.value.clone()]
        }
        ("herdr:copilot", "copilot", AgentSessionRefKind::Id) => {
            vec!["copilot".into(), format!("--resume={}", session_ref.value)]
        }
        ("herdr:devin", "devin", AgentSessionRefKind::Id) => {
            vec!["devin".into(), "--resume".into(), session_ref.value.clone()]
        }
        ("herdr:droid", "droid", AgentSessionRefKind::Id) => {
            vec!["droid".into(), "--resume".into(), session_ref.value.clone()]
        }
        ("herdr:kimi", "kimi", AgentSessionRefKind::Id) => {
            vec!["kimi".into(), "--session".into(), session_ref.value.clone()]
        }
        ("herdr:mastracode", "mastracode", AgentSessionRefKind::Id) => {
            vec![
                "mastracode".into(),
                "--thread".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:pi", "pi", AgentSessionRefKind::Path | AgentSessionRefKind::Id) => {
            vec!["pi".into(), "--session".into(), session_ref.value.clone()]
        }
        ("herdr:omp", "omp", AgentSessionRefKind::Path | AgentSessionRefKind::Id) => {
            // omp resume is `-r, --resume=<value>` (ID prefix or path); it has no
            // `--session` flag, unlike pi.
            vec!["omp".into(), format!("--resume={}", session_ref.value)]
        }
        ("herdr:hermes", "hermes", AgentSessionRefKind::Id) => {
            vec![
                "hermes".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:opencode", "opencode", AgentSessionRefKind::Id) => {
            vec![
                "opencode".into(),
                "--session".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:qodercli", "qodercli", AgentSessionRefKind::Id) => {
            vec![
                "qodercli".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:kilo", "kilo", AgentSessionRefKind::Id) => {
            vec!["kilo".into(), "--session".into(), session_ref.value.clone()]
        }
        ("herdr:cursor", "cursor", AgentSessionRefKind::Id) => {
            vec![
                "cursor-agent".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        _ => return None,
    };

    Some(AgentResumePlan {
        agent: agent.to_string(),
        argv,
        dedupe_key: dedupe_key(source, agent, session_ref),
    })
}

/// Build a resume command that replays the pane's recorded launch command,
/// appending the agent's canonical resume selector.
///
/// When a pane recorded the exact argv it was launched with (for example
/// `custom-pi --session <path> --model opus`), resuming should preserve those
/// original flags instead of rebuilding a minimal `<agent> --resume <id>`. The
/// recorded program (`argv[0]`, e.g. `custom-pi`) is kept so a wrapped or forked
/// binary resumes as itself rather than the canonical agent name. Any stale
/// session selector already present in the recorded command is stripped first
/// so the replayed command never carries two conflicting selectors.
///
/// Falls back to the minimal recipe from [`plan`] when there is no recorded
/// launch command (e.g. a hand-typed shell agent).
///
/// Only session selectors are rewritten. One-shot / print flags (e.g. `-p`,
/// `--prompt`, `--print`) are intentionally not stripped: such panes are
/// non-interactive and are already excluded from auto-resume upstream.
pub fn plan_replaying_launch_argv(
    source: &str,
    agent: &str,
    session_ref: &AgentSessionRef,
    launch_argv: Option<&[String]>,
) -> Option<AgentResumePlan> {
    let base = plan(source, agent, session_ref)?;
    let Some(launch_argv) = launch_argv.filter(|argv| !argv.is_empty()) else {
        return Some(base);
    };

    // The recipe argv is `[<canonical-bin>, <selector-tokens...>]`. Replay keeps
    // the pane's original program plus its extra flags, drops any stale session
    // selector, then re-appends the canonical selector tokens.
    let selector = &base.argv[1..];
    let recipe_flag = selector.first().map(|token| selector_flag_name(token));
    let mut argv = strip_session_selectors(agent, launch_argv, recipe_flag);
    argv.extend(selector.iter().cloned());

    Some(AgentResumePlan {
        agent: base.agent,
        argv,
        dedupe_key: base.dedupe_key,
    })
}

/// Remove any session selector the recorded launch command already carries,
/// while preserving `argv[0]` (the launched program) and every non-selector
/// flag. See [`plan_replaying_launch_argv`] for the surrounding contract.
fn strip_session_selectors(
    agent: &str,
    launch_argv: &[String],
    recipe_flag: Option<&str>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(launch_argv.len());
    let mut idx = 0usize;

    // Always keep the program itself; only selectors are rewritten.
    if let Some(program) = launch_argv.first() {
        out.push(program.clone());
        idx = 1;
    }

    while idx < launch_argv.len() {
        let token = &launch_argv[idx];
        idx += 1;
        let name = selector_flag_name(token);

        // codex resumes via a `resume <id>` subcommand rather than a flag.
        if agent == "codex" && token == "resume" {
            // Drop a following session-id value when present (skip flags).
            if launch_argv
                .get(idx)
                .is_some_and(|next| !next.starts_with('-'))
            {
                idx += 1;
            }
            continue;
        }

        if is_value_session_flag(agent, name, recipe_flag) {
            // `--flag=value` carries its value inline; `--flag value` consumes
            // the following token when it is not itself a flag.
            if !token.contains('=')
                && launch_argv
                    .get(idx)
                    .is_some_and(|next| !next.starts_with('-'))
            {
                idx += 1;
            }
            continue;
        }

        if is_bare_session_flag(agent, name) {
            continue;
        }

        out.push(token.clone());
    }

    out
}

/// Session selectors that consume a following value. Every agent's canonical
/// flag (e.g. `--session`, `--thread`, or `--resume`) comes from its recipe.
/// Only Claude and OMP additionally confirm `-r` as an alias. Codex's selector
/// is a subcommand handled separately.
fn is_value_session_flag(agent: &str, name: &str, recipe_flag: Option<&str>) -> bool {
    let is_canonical_flag = agent != "codex"
        && recipe_flag
            .is_some_and(|recipe_flag| recipe_flag.starts_with('-') && name == recipe_flag);

    is_canonical_flag || matches!((agent, name), ("claude" | "omp", "-r"))
}

/// Claude alone confirms `--continue`/`-c` as bare session selectors. Preserve
/// those tokens for every other agent unless they become a documented recipe.
fn is_bare_session_flag(agent: &str, name: &str) -> bool {
    agent == "claude" && matches!(name, "--continue" | "-c")
}

/// The flag name portion of a token, splitting an inline `--flag=value`.
fn selector_flag_name(token: &str) -> &str {
    match token.split_once('=') {
        Some((name, _)) => name,
        None => token,
    }
}

pub fn dedupe_key(source: &str, agent: &str, session_ref: &AgentSessionRef) -> String {
    format!(
        "{source}\u{0}{agent}\u{0}{:?}\u{0}{}",
        session_ref.kind, session_ref.value
    )
}

fn is_official_agent_source(source: &str, agent: &str) -> bool {
    matches!(
        (source, agent),
        ("herdr:claude", "claude")
            | ("herdr:codex", "codex")
            | ("herdr:copilot", "copilot")
            | ("herdr:devin", "devin")
            | ("herdr:droid", "droid")
            | ("herdr:kimi", "kimi")
            | ("herdr:omp", "omp")
            | ("herdr:mastracode", "mastracode")
            | ("herdr:pi", "pi")
            | ("herdr:hermes", "hermes")
            | ("herdr:opencode", "opencode")
            | ("herdr:qodercli", "qodercli")
            | ("herdr:kilo", "kilo")
            | ("herdr:cursor", "cursor")
    )
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_SESSION_ID_LEN && !value.chars().any(char::is_control)
}

fn valid_session_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_PATH_LEN
        && !value.chars().any(char::is_control)
        && Path::new(value).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute_test_path(name: &str) -> String {
        std::env::current_dir()
            .unwrap()
            .join(name)
            .display()
            .to_string()
    }

    #[test]
    fn native_state_reservation_excludes_full_lifecycle_sources() {
        assert!(is_reserved_native_state_source("herdr:claude", "claude"));
        assert!(is_reserved_native_state_source("herdr:codex", "codex"));
        assert!(is_reserved_native_state_source("herdr:devin", "devin"));
        assert!(!is_reserved_native_state_source("herdr:kimi", "kimi"));
        assert!(!is_reserved_native_state_source(
            "herdr:opencode",
            "opencode"
        ));
    }

    #[test]
    fn planner_allows_supported_agents() {
        let pi_session = absolute_test_path("pi-session.jsonl");
        let omp_session = absolute_test_path("omp-session.jsonl");
        assert_eq!(
            plan(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id("claude-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["claude", "--resume", "claude-session"]
        );
        assert_eq!(
            plan(
                "herdr:codex",
                "codex",
                &AgentSessionRef::id("codex-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["codex", "resume", "codex-session"]
        );
        assert_eq!(
            plan(
                "herdr:copilot",
                "copilot",
                &AgentSessionRef::id("copilot-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["copilot", "--resume=copilot-session"]
        );
        assert_eq!(
            plan(
                "herdr:devin",
                "devin",
                &AgentSessionRef::id("devin-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["devin", "--resume", "devin-session"]
        );
        assert_eq!(
            plan(
                "herdr:droid",
                "droid",
                &AgentSessionRef::id("droid-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["droid", "--resume", "droid-session"]
        );
        assert_eq!(
            plan(
                "herdr:kimi",
                "kimi",
                &AgentSessionRef::id("kimi-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["kimi", "--session", "kimi-session"]
        );
        assert_eq!(
            plan(
                "herdr:mastracode",
                "mastracode",
                &AgentSessionRef::id("mastracode-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["mastracode", "--thread", "mastracode-session"]
        );
        assert_eq!(
            plan(
                "herdr:pi",
                "pi",
                &AgentSessionRef::path(&pi_session).unwrap()
            )
            .unwrap()
            .argv,
            vec!["pi", "--session", pi_session.as_str()]
        );
        assert_eq!(
            plan(
                "herdr:omp",
                "omp",
                &AgentSessionRef::path(&omp_session).unwrap()
            )
            .unwrap()
            .argv,
            vec!["omp", format!("--resume={omp_session}").as_str()]
        );
        assert_eq!(
            plan(
                "herdr:hermes",
                "hermes",
                &AgentSessionRef::id("hermes-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["hermes", "--resume", "hermes-session"]
        );
        assert_eq!(
            plan(
                "herdr:opencode",
                "opencode",
                &AgentSessionRef::id("opencode-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["opencode", "--session", "opencode-session"]
        );
        assert_eq!(
            plan(
                "herdr:qodercli",
                "qodercli",
                &AgentSessionRef::id("qoder-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["qodercli", "--resume", "qoder-session"]
        );
        assert_eq!(
            plan(
                "herdr:kilo",
                "kilo",
                &AgentSessionRef::id("kilo-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["kilo", "--session", "kilo-session"]
        );
        assert_eq!(
            plan(
                "herdr:cursor",
                "cursor",
                &AgentSessionRef::id("cursor-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["cursor-agent", "--resume", "cursor-session"]
        );
    }

    #[test]
    fn planner_rejects_custom_and_unsupported_path_refs() {
        let claude_session = absolute_test_path("claude-session");
        assert!(plan(
            "custom:claude",
            "claude",
            &AgentSessionRef::id("session").unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:claude",
            "claude",
            &AgentSessionRef::path(&claude_session).unwrap()
        )
        .is_none());
    }

    #[test]
    fn report_ref_prefers_pi_and_omp_paths_and_validates_values() {
        let pi_session = absolute_test_path("pi-session.jsonl");
        let omp_session = absolute_test_path("omp-session.jsonl");
        let claude_session = absolute_test_path("claude-session");
        let copilot_session = absolute_test_path("copilot-session");
        let session_ref = session_ref_from_report(
            "herdr:pi",
            "pi",
            Some("pi-id".into()),
            Some(pi_session.clone()),
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Path);
        assert_eq!(session_ref.value, pi_session);

        assert!(session_ref_from_report("herdr:pi", "pi", Some("bad\nid".into()), None).is_none());
        assert!(
            session_ref_from_report("herdr:pi", "pi", None, Some("relative.jsonl".into()))
                .is_none()
        );
        assert!(session_ref_from_report("custom:pi", "pi", Some("pi-id".into()), None).is_none());

        let session_ref = session_ref_from_report(
            "herdr:omp",
            "omp",
            Some("omp-id".into()),
            Some(omp_session.clone()),
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Path);
        assert_eq!(session_ref.value, omp_session);

        let session_ref =
            session_ref_from_report("herdr:omp", "omp", Some("omp-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "omp-id");
        let session_ref = session_ref_from_report(
            "herdr:omp",
            "omp",
            Some("omp-id".into()),
            Some("relative.jsonl".into()),
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "omp-id");
        assert!(
            session_ref_from_report("herdr:omp", "omp", None, Some("relative.jsonl".into()))
                .is_none()
        );

        assert!(
            session_ref_from_report("herdr:claude", "claude", None, Some(claude_session)).is_none()
        );

        let session_ref =
            session_ref_from_report("herdr:copilot", "copilot", Some("copilot-id".into()), None)
                .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "copilot-id");
        assert!(
            session_ref_from_report("herdr:copilot", "copilot", None, Some(copilot_session))
                .is_none()
        );

        let session_ref =
            session_ref_from_report("herdr:devin", "devin", Some("devin-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "devin-id");

        let session_ref =
            session_ref_from_report("herdr:droid", "droid", Some("droid-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "droid-id");
        assert!(session_ref_from_report(
            "herdr:droid",
            "droid",
            None,
            Some("/tmp/droid-session".into())
        )
        .is_none());

        let session_ref =
            session_ref_from_report("herdr:kimi", "kimi", Some("kimi-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "kimi-id");

        let session_ref = session_ref_from_report(
            "herdr:mastracode",
            "mastracode",
            Some("mastracode-id".into()),
            None,
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "mastracode-id");

        let session_ref =
            session_ref_from_report("herdr:kilo", "kilo", Some("kilo-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "kilo-id");

        let session_ref =
            session_ref_from_report("herdr:qodercli", "qodercli", Some("qoder-id".into()), None)
                .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "qoder-id");
    }

    #[test]
    fn normalize_session_start_source_allows_known_values() {
        assert_eq!(
            normalize_session_start_source(Some("startup".into())),
            Some("startup".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("resume".into())),
            Some("resume".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("clear".into())),
            Some("clear".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("compact".into())),
            Some("compact".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("new".into())),
            Some("new".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("fork".into())),
            Some("fork".into())
        );
        assert_eq!(
            normalize_session_start_source(Some(" resume ".into())),
            Some("resume".into())
        );
        assert_eq!(normalize_session_start_source(Some("other".into())), None);
        assert_eq!(normalize_session_start_source(None), None);
    }

    #[test]
    fn ids_are_data_not_shell_text() {
        let id = "abc; rm -rf /";
        let codex_plan = plan("herdr:codex", "codex", &AgentSessionRef::id(id).unwrap()).unwrap();
        assert_eq!(codex_plan.argv, vec!["codex", "resume", id]);

        let copilot_plan = plan(
            "herdr:copilot",
            "copilot",
            &AgentSessionRef::id(id).unwrap(),
        )
        .unwrap();
        assert_eq!(copilot_plan.argv, vec!["copilot", "--resume=abc; rm -rf /"]);

        let devin_plan = plan("herdr:devin", "devin", &AgentSessionRef::id(id).unwrap()).unwrap();
        assert_eq!(devin_plan.argv, vec!["devin", "--resume", id]);
    }

    #[test]
    fn planner_rejects_path_refs_for_id_only_agents() {
        let hermes_session = absolute_test_path("hermes-session");
        let opencode_session = absolute_test_path("opencode-session");
        let kilo_session = absolute_test_path("kilo-session");
        let copilot_session = absolute_test_path("copilot-session");
        let devin_session = absolute_test_path("devin-session");
        assert!(plan(
            "herdr:hermes",
            "hermes",
            &AgentSessionRef::path(&hermes_session).unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:opencode",
            "opencode",
            &AgentSessionRef::path(&opencode_session).unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:kilo",
            "kilo",
            &AgentSessionRef::path(&kilo_session).unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:copilot",
            "copilot",
            &AgentSessionRef::path(&copilot_session).unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:devin",
            "devin",
            &AgentSessionRef::path(&devin_session).unwrap()
        )
        .is_none());
        assert!(session_ref_from_snapshot(
            "herdr:mastracode",
            "mastracode",
            AgentSessionRefKind::Id,
            "mastracode-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:hermes",
            "hermes",
            AgentSessionRefKind::Id,
            "hermes-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:opencode",
            "opencode",
            AgentSessionRefKind::Id,
            "opencode-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:kilo",
            "kilo",
            AgentSessionRefKind::Id,
            "kilo-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:copilot",
            "copilot",
            AgentSessionRefKind::Id,
            "copilot-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:devin",
            "devin",
            AgentSessionRefKind::Id,
            "devin-session"
        )
        .is_some());
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn replay_without_launch_argv_matches_minimal_plan() {
        // No recorded launch command: fall back to today's minimal recipe,
        // byte-for-byte identical to plan().
        for (source, agent, session) in [
            ("herdr:claude", "claude", "claude-session"),
            ("herdr:codex", "codex", "codex-session"),
            ("herdr:copilot", "copilot", "copilot-session"),
            ("herdr:cursor", "cursor", "cursor-session"),
        ] {
            let session_ref = AgentSessionRef::id(session).unwrap();
            let base = plan(source, agent, &session_ref).unwrap();
            assert_eq!(
                plan_replaying_launch_argv(source, agent, &session_ref, None).unwrap(),
                base,
                "{agent}: None launch_argv should equal minimal plan()"
            );
            assert_eq!(
                plan_replaying_launch_argv(source, agent, &session_ref, Some(&[])).unwrap(),
                base,
                "{agent}: empty launch_argv should equal minimal plan()"
            );
        }
    }

    #[test]
    fn replay_unsupported_agent_is_none_even_with_launch_argv() {
        let launch = argv(&["claude", "--model", "opus"]);
        assert!(plan_replaying_launch_argv(
            "custom:claude",
            "claude",
            &AgentSessionRef::id("session").unwrap(),
            Some(&launch),
        )
        .is_none());
    }

    #[test]
    fn replay_rewrites_every_agents_canonical_selector() {
        for (source, agent, launch) in [
            (
                "herdr:claude",
                "claude",
                argv(&["claude", "--resume", "old", "--model", "opus"]),
            ),
            (
                "herdr:codex",
                "codex",
                argv(&["codex", "resume", "old", "--model", "opus"]),
            ),
            (
                "herdr:copilot",
                "copilot",
                argv(&["copilot", "--resume=old", "--model", "opus"]),
            ),
            (
                "herdr:devin",
                "devin",
                argv(&["devin", "--resume", "old", "--model", "opus"]),
            ),
            (
                "herdr:droid",
                "droid",
                argv(&["droid", "--resume", "old", "--model", "opus"]),
            ),
            (
                "herdr:kimi",
                "kimi",
                argv(&["kimi", "--session", "old", "--model", "opus"]),
            ),
            (
                "herdr:mastracode",
                "mastracode",
                argv(&["mastracode", "--thread", "old", "--model", "opus"]),
            ),
            (
                "herdr:pi",
                "pi",
                argv(&["pi", "--session", "old", "--model", "opus"]),
            ),
            (
                "herdr:omp",
                "omp",
                argv(&["omp", "--resume=old", "--model", "opus"]),
            ),
            (
                "herdr:hermes",
                "hermes",
                argv(&["hermes", "--resume", "old", "--model", "opus"]),
            ),
            (
                "herdr:opencode",
                "opencode",
                argv(&["opencode", "--session", "old", "--model", "opus"]),
            ),
            (
                "herdr:qodercli",
                "qodercli",
                argv(&["qodercli", "--resume", "old", "--model", "opus"]),
            ),
            (
                "herdr:kilo",
                "kilo",
                argv(&["kilo", "--session", "old", "--model", "opus"]),
            ),
            (
                "herdr:cursor",
                "cursor",
                argv(&["cursor-agent", "--resume", "old", "--model", "opus"]),
            ),
        ] {
            let session_ref = AgentSessionRef::id(format!("fresh-{agent}")).unwrap();
            let canonical = plan(source, agent, &session_ref).unwrap();
            let mut expected = vec![launch[0].clone(), "--model".into(), "opus".into()];
            expected.extend(canonical.argv[1..].iter().cloned());

            assert_eq!(
                plan_replaying_launch_argv(source, agent, &session_ref, Some(&launch))
                    .unwrap()
                    .argv,
                expected,
                "{agent}: stale canonical selector must be replaced exactly once"
            );
        }
    }

    #[test]
    fn replay_preserves_extra_flags_and_appends_single_selector() {
        // claude launched with extra flags, no stale selector.
        let launch = argv(&[
            "claude",
            "--dangerously-skip-permissions",
            "--model",
            "opus",
        ]);
        let resume = plan_replaying_launch_argv(
            "herdr:claude",
            "claude",
            &AgentSessionRef::id("sess-1").unwrap(),
            Some(&launch),
        )
        .unwrap();
        assert_eq!(
            resume.argv,
            argv(&[
                "claude",
                "--dangerously-skip-permissions",
                "--model",
                "opus",
                "--resume",
                "sess-1",
            ])
        );
        // Agent + dedupe_key stay tied to the canonical recipe, not launch_argv.
        assert_eq!(resume.agent, "claude");
        assert_eq!(
            resume.dedupe_key,
            dedupe_key(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id("sess-1").unwrap()
            )
        );
    }

    #[test]
    fn replay_preserves_one_shot_and_print_flags() {
        let launch = argv(&[
            "claude",
            "-p",
            "one-shot prompt",
            "--prompt",
            "second prompt",
            "--print",
        ]);

        assert_eq!(
            plan_replaying_launch_argv(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id("fresh-session").unwrap(),
                Some(&launch),
            )
            .unwrap()
            .argv,
            argv(&[
                "claude",
                "-p",
                "one-shot prompt",
                "--prompt",
                "second prompt",
                "--print",
                "--resume",
                "fresh-session",
            ])
        );
    }

    #[test]
    fn replay_preserves_unconfirmed_and_cross_agent_selector_flags() {
        for (source, agent, launch) in [
            (
                "herdr:hermes",
                "hermes",
                argv(&[
                    "hermes",
                    "--continue",
                    "named",
                    "-c",
                    "config-value",
                    "-r",
                    "foreign",
                ]),
            ),
            (
                "herdr:opencode",
                "opencode",
                argv(&["opencode", "-s", "old", "--resume", "foreign"]),
            ),
            (
                "herdr:kilo",
                "kilo",
                argv(&["kilo", "-s", "old", "--resume", "foreign"]),
            ),
            (
                "herdr:codex",
                "codex",
                argv(&["codex", "--last", "-r", "foreign", "--continue"]),
            ),
            (
                "herdr:pi",
                "pi",
                argv(&[
                    "pi",
                    "--resume",
                    "foreign",
                    "-r",
                    "foreign-short",
                    "--continue",
                    "-c",
                ]),
            ),
            (
                "herdr:copilot",
                "copilot",
                argv(&["copilot", "-r", "foreign", "--continue"]),
            ),
        ] {
            let session_ref = AgentSessionRef::id(format!("fresh-{agent}")).unwrap();
            let canonical = plan(source, agent, &session_ref).unwrap();
            let mut expected = launch.clone();
            expected.extend(canonical.argv[1..].iter().cloned());

            assert_eq!(
                plan_replaying_launch_argv(source, agent, &session_ref, Some(&launch))
                    .unwrap()
                    .argv,
                expected,
                "{agent}: only its explicit supported selectors may be rewritten"
            );
        }
    }

    #[test]
    fn replay_omp_strips_confirmed_short_resume_selector() {
        let launch = argv(&["omp", "-r", "old-session", "--model", "opus"]);

        assert_eq!(
            plan_replaying_launch_argv(
                "herdr:omp",
                "omp",
                &AgentSessionRef::id("fresh-session").unwrap(),
                Some(&launch),
            )
            .unwrap()
            .argv,
            argv(&["omp", "--model", "opus", "--resume=fresh-session",])
        );
    }

    #[test]
    fn replay_strips_stale_claude_resume_and_r_and_continue() {
        // Stale `--resume <old>` is stripped, then the fresh selector appended.
        let with_resume = argv(&["claude", "--resume", "old-id", "--model", "opus"]);
        assert_eq!(
            plan_replaying_launch_argv(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id("new-id").unwrap(),
                Some(&with_resume),
            )
            .unwrap()
            .argv,
            argv(&["claude", "--model", "opus", "--resume", "new-id"])
        );

        // Short `-r <old>` form.
        let with_r = argv(&["claude", "-r", "old-id", "--model", "opus"]);
        assert_eq!(
            plan_replaying_launch_argv(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id("new-id").unwrap(),
                Some(&with_r),
            )
            .unwrap()
            .argv,
            argv(&["claude", "--model", "opus", "--resume", "new-id"])
        );

        // Bare `--continue` / `-c` (no value) is stripped.
        let with_continue = argv(&["claude", "--continue", "--model", "opus"]);
        assert_eq!(
            plan_replaying_launch_argv(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id("new-id").unwrap(),
                Some(&with_continue),
            )
            .unwrap()
            .argv,
            argv(&["claude", "--model", "opus", "--resume", "new-id"])
        );
        let with_c = argv(&["claude", "-c", "--model", "opus"]);
        assert_eq!(
            plan_replaying_launch_argv(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id("new-id").unwrap(),
                Some(&with_c),
            )
            .unwrap()
            .argv,
            argv(&["claude", "--model", "opus", "--resume", "new-id"])
        );
    }

    #[test]
    fn replay_handles_bare_resume_without_value() {
        // `--resume` with no following id (next token is a flag) drops only the
        // flag, keeping the trailing option intact.
        let launch = argv(&["claude", "--resume", "--model", "opus"]);
        assert_eq!(
            plan_replaying_launch_argv(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id("new-id").unwrap(),
                Some(&launch),
            )
            .unwrap()
            .argv,
            argv(&["claude", "--model", "opus", "--resume", "new-id"])
        );
    }

    #[test]
    fn replay_codex_strips_resume_subcommand_but_preserves_config() {
        // codex's `resume <old>` subcommand is stripped; `-c key=value`
        // (--config) is intentionally preserved.
        let launch = argv(&["codex", "-c", "model=o3", "resume", "old-id"]);
        assert_eq!(
            plan_replaying_launch_argv(
                "herdr:codex",
                "codex",
                &AgentSessionRef::id("new-id").unwrap(),
                Some(&launch),
            )
            .unwrap()
            .argv,
            argv(&["codex", "-c", "model=o3", "resume", "new-id"])
        );

        // `--config=key=value` inline form is also preserved.
        let launch_long = argv(&["codex", "--config=model=o3", "--dangerously-bypass"]);
        assert_eq!(
            plan_replaying_launch_argv(
                "herdr:codex",
                "codex",
                &AgentSessionRef::id("new-id").unwrap(),
                Some(&launch_long),
            )
            .unwrap()
            .argv,
            argv(&[
                "codex",
                "--config=model=o3",
                "--dangerously-bypass",
                "resume",
                "new-id",
            ])
        );
    }

    #[test]
    fn replay_pi_keeps_forked_binary_and_avoids_double_session() {
        // An alternate pi executable with a stale `--session <old>` must remain
        // the launched program while replay replaces the selector exactly once.
        let new_path = absolute_test_path("pi-new-session.jsonl");
        let old_path = absolute_test_path("pi-old-session.jsonl");
        let launch = argv(&["custom-pi", "--session", &old_path, "--model", "opus"]);
        let resume = plan_replaying_launch_argv(
            "herdr:pi",
            "pi",
            &AgentSessionRef::path(&new_path).unwrap(),
            Some(&launch),
        )
        .unwrap();
        assert_eq!(
            resume.argv,
            argv(&["custom-pi", "--model", "opus", "--session", &new_path])
        );
        // Exactly one `--session` selector survives.
        assert_eq!(resume.argv.iter().filter(|t| *t == "--session").count(), 1);
        // agent stays `pi` so detection/labeling is unchanged.
        assert_eq!(resume.agent, "pi");
    }

    #[test]
    fn replay_copilot_inline_resume_is_deduped() {
        // copilot's recipe uses the inline `--resume=<id>` form; a stale inline
        // selector must be dropped, not doubled.
        let launch = argv(&["copilot", "--resume=old-id", "--banner"]);
        assert_eq!(
            plan_replaying_launch_argv(
                "herdr:copilot",
                "copilot",
                &AgentSessionRef::id("new-id").unwrap(),
                Some(&launch),
            )
            .unwrap()
            .argv,
            argv(&["copilot", "--banner", "--resume=new-id"])
        );
    }

    #[test]
    fn replay_omp_strips_stale_resume_inline() {
        let new_path = absolute_test_path("omp-new.jsonl");
        let launch = argv(&["omp", "--resume=old-value", "--flag"]);
        assert_eq!(
            plan_replaying_launch_argv(
                "herdr:omp",
                "omp",
                &AgentSessionRef::path(&new_path).unwrap(),
                Some(&launch),
            )
            .unwrap()
            .argv,
            argv(&["omp", "--flag", &format!("--resume={new_path}")])
        );
    }

    #[test]
    fn replay_survives_repeated_restarts() {
        // The carried-forward launch_argv is the ORIGINAL user launch. Each
        // restart recomputes the selector from the current session_ref, so the
        // extra flags survive indefinitely with exactly one selector every time.
        let original = argv(&["claude", "--model", "opus"]);
        for session in ["sess-1", "sess-2", "sess-3", "sess-4"] {
            let resume = plan_replaying_launch_argv(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id(session).unwrap(),
                Some(&original),
            )
            .unwrap();
            assert_eq!(
                resume.argv,
                argv(&["claude", "--model", "opus", "--resume", session]),
                "cycle {session}: flags preserved, exactly one fresh selector"
            );
        }
    }

    #[test]
    fn replay_is_idempotent_when_previous_output_is_fed_back() {
        // Stronger idempotence: even if a restart carried forward the REPLAYED
        // argv (which already ends in `--resume <old>`) instead of the original,
        // re-planning must strip the stale selector and re-append a single fresh
        // one. Chaining the output back as input for 4 cycles proves no
        // selector/flag accumulation and a stable, bounded argv.
        let mut carried = argv(&["claude", "--model", "opus"]);
        let base_len = carried.len();
        for session in ["sess-1", "sess-2", "sess-3", "sess-4"] {
            let resume = plan_replaying_launch_argv(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id(session).unwrap(),
                Some(&carried),
            )
            .unwrap();
            assert_eq!(
                resume.argv,
                argv(&["claude", "--model", "opus", "--resume", session]),
                "cycle {session}: stale selector stripped, single fresh one appended"
            );
            assert_eq!(
                resume.argv.iter().filter(|t| *t == "--resume").count(),
                1,
                "cycle {session}: no `--resume` accumulation"
            );
            // argv length stays bounded at base flags + one `--resume <id>`.
            assert_eq!(resume.argv.len(), base_len + 2);
            carried = resume.argv;
        }
    }

    #[test]
    fn replay_pi_is_idempotent_across_repeated_restarts() {
        // When the ORIGINAL custom executable launch carries a `--session`
        // selector, chaining replayed argv through 4 cycles must keep that
        // executable, preserve `--model opus`, and never add a second selector.
        let paths = [
            absolute_test_path("pi-s1.jsonl"),
            absolute_test_path("pi-s2.jsonl"),
            absolute_test_path("pi-s3.jsonl"),
            absolute_test_path("pi-s4.jsonl"),
        ];
        let mut carried = argv(&[
            "custom-pi",
            "--session",
            &absolute_test_path("pi-original.jsonl"),
            "--model",
            "opus",
        ]);
        for path in &paths {
            let resume = plan_replaying_launch_argv(
                "herdr:pi",
                "pi",
                &AgentSessionRef::path(path).unwrap(),
                Some(&carried),
            )
            .unwrap();
            assert_eq!(
                resume.argv,
                argv(&["custom-pi", "--model", "opus", "--session", path])
            );
            assert_eq!(
                resume.argv.iter().filter(|t| *t == "--session").count(),
                1,
                "no `--session` accumulation for {path}"
            );
            assert_eq!(resume.argv.first().map(String::as_str), Some("custom-pi"));
            carried = resume.argv;
        }
    }

    #[test]
    fn replay_keeps_wrapper_program_and_env_prefix() {
        // A wrapped launch (env prefix) is replayed verbatim with the selector
        // appended; the resume still targets the recorded program chain.
        let launch = argv(&["env", "FOO=bar", "claude", "--model", "opus"]);
        assert_eq!(
            plan_replaying_launch_argv(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id("sess-1").unwrap(),
                Some(&launch),
            )
            .unwrap()
            .argv,
            argv(&["env", "FOO=bar", "claude", "--model", "opus", "--resume", "sess-1"])
        );
    }
}
