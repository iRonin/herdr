use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_GIT_REF_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSpaceMetadata {
    pub key: String,
    pub checkout_key: String,
    pub repo_name: String,
    pub repo_root: PathBuf,
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktreeInfo {
    pub repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub git_common_dir: PathBuf,
    pub is_bare: bool,
    pub is_linked_worktree: bool,
}

pub fn derive_label_from_cwd(cwd: &Path) -> String {
    workspace_auto_label(cwd, git_repo_root(cwd).as_deref()).0
}

/// The automatic workspace label for `cwd`, plus whether a project file *pinned*
/// it rather than it being derived from the filesystem.
///
/// A `.herdr/settings.toml` in the working tree (the nearest one found walking up
/// from `cwd`, capped at `repo_root` so a file cannot leak in from above the
/// repository) pins the label via `[workspace] name`. When that file is absent,
/// unreadable, or lacks the field, derivation DELEGATES to upstream's own helpers
/// and reproduces stock behaviour exactly -- including the linked-worktree rule
/// `automatic_workspace_label` implements. Do not inline that logic here.
///
/// Callers that already hold the repository root MUST pass it: this function does
/// no repository discovery of its own, so it adds no `git` invocation at the two
/// identity-fill sites.
///
/// The `pinned` flag must be carried, never inferred by comparing the returned
/// string to the directory basename -- a pinned name may legitimately equal it.
pub(crate) fn workspace_auto_label(cwd: &Path, repo_root: Option<&Path>) -> (String, bool) {
    if let Some(name) = project_workspace_name(cwd, repo_root) {
        return (name, true);
    }

    let label = match repo_root {
        Some(repo_root) => automatic_workspace_label(cwd, repo_root),
        None => fallback_label_from_cwd(cwd),
    };
    (label, false)
}

pub fn fallback_label_from_cwd(cwd: &Path) -> String {
    if let Ok(home) = std::env::var("HOME") {
        let home = Path::new(&home);
        if cwd == home {
            return "~".to_string();
        }
    }

    cwd.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| cwd.display().to_string())
}

/// Reads `[workspace] name` from the nearest `.herdr/settings.toml` found by
/// walking up from `cwd`. When `repo_root` is given, the search stops there so a
/// project file cannot leak in from above the repository; otherwise it walks to
/// the filesystem root. Returns `None` when no file is found, the file is
/// unreadable, the table is absent, or the name is empty/whitespace.
fn project_workspace_name(cwd: &Path, repo_root: Option<&Path>) -> Option<String> {
    existing_settings_path(cwd, repo_root)
        .as_deref()
        .and_then(read_workspace_name)
}

/// Path of the nearest `.herdr/settings.toml` found by walking up from `cwd`.
/// The search stops at `repo_root` when given (so a project file cannot leak in
/// from above the repository). Outside a repository, no settings file is ever
/// read from `$HOME` or `/` unless the walk started there: a file in either
/// place would pin every non-Git workspace beneath it, and a rename from any
/// of those workspaces would have created or updated it as the "nearest"
/// file. A workspace whose own directory is `$HOME` (or `/`) keeps its file.
fn existing_settings_path(cwd: &Path, repo_root: Option<&Path>) -> Option<PathBuf> {
    let mut current = if cwd.is_dir() {
        cwd.to_path_buf()
    } else {
        cwd.parent()?.to_path_buf()
    };
    let start = current.clone();
    let home = std::env::var("HOME").ok().map(PathBuf::from);

    loop {
        if !settings_dir_is_excluded(&current, &start, repo_root, home.as_deref()) {
            let candidate = current.join(".herdr/settings.toml");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if Some(current.as_path()) == repo_root {
            break;
        }
        if !current.pop() {
            break;
        }
    }

    None
}

/// Whether the walk refuses to read a settings file from `current`. Inside a
/// repository the repository cap is the only rule. Outside one, `$HOME` and
/// `/` are excluded unless the walk started there -- the starting directory is
/// always in range, so the home workspace itself keeps its own file.
fn settings_dir_is_excluded(
    current: &Path,
    start: &Path,
    repo_root: Option<&Path>,
    home: Option<&Path>,
) -> bool {
    if repo_root.is_some() || current == start {
        return false;
    }
    current == Path::new("/") || home == Some(current)
}

/// Where a new `.herdr/settings.toml` is created when none exists in the walk
/// range: at the repository root when inside one, otherwise at `cwd`.
fn default_settings_path(cwd: &Path, repo_root: Option<&Path>) -> PathBuf {
    repo_root
        .map(|root| root.join(".herdr/settings.toml"))
        .unwrap_or_else(|| cwd.join(".herdr/settings.toml"))
}

/// Persists `[workspace] name = name` into the project's `.herdr/settings.toml`.
///
/// Updates the nearest existing file in the walk range in place; if none
/// exists, creates one at the repository root (or `cwd` outside a repository).
/// Other tables and keys are preserved. The write is atomic (temp file +
/// rename) and best-effort at the call site: a failure (e.g. a read-only
/// checkout, or a `workspace` entry that is not a table) must not block an
/// in-memory rename.
pub(crate) fn persist_workspace_name(cwd: &Path, name: &str) -> std::io::Result<()> {
    let repo_root = git_repo_root(cwd);
    let path = existing_settings_path(cwd, repo_root.as_deref())
        .unwrap_or_else(|| default_settings_path(cwd, repo_root.as_deref()));

    let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)
        .ok()
        .and_then(|contents| contents.parse().ok())
        .unwrap_or_default();

    let workspace = match doc.entry("workspace") {
        toml_edit::Entry::Occupied(entry) => entry.into_mut().as_table_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "`workspace` is not a table in .herdr/settings.toml",
            )
        })?,
        toml_edit::Entry::Vacant(slot) => slot
            .insert(toml_edit::table())
            .as_table_mut()
            .expect("workspace entry was just inserted as a table"),
    };
    workspace["name"] = toml_edit::value(name);

    let contents = doc.to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("settings.toml")
    ));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn read_workspace_name(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }

    #[derive(serde::Deserialize)]
    struct Settings {
        #[serde(default)]
        workspace: WorkspaceSettings,
    }

    #[derive(Default, serde::Deserialize)]
    struct WorkspaceSettings {
        name: Option<String>,
    }

    let contents = std::fs::read_to_string(path).ok()?;
    let settings: Settings = toml::from_str(&contents).ok()?;
    settings
        .workspace
        .name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

pub fn git_worktree_info(cwd: &Path) -> Option<GitWorktreeInfo> {
    let repo_root = git_repo_root(cwd)?;
    let git_dir = canonicalize_best_effort_path(&git_dir_for_repo_root(&repo_root)?);
    let git_common_dir = canonicalize_best_effort_path(&git_common_dir_for_git_dir(&git_dir));
    let is_linked_worktree = git_dir != git_common_dir;
    let is_bare = git_dir_is_bare(&git_dir);

    Some(GitWorktreeInfo {
        repo_root,
        git_dir,
        git_common_dir,
        is_bare,
        is_linked_worktree,
    })
}

pub fn git_space_metadata(cwd: &Path) -> Option<GitSpaceMetadata> {
    let info = git_worktree_info(cwd)?;
    Some(git_space_metadata_from_info(&info))
}

pub(crate) fn automatic_workspace_label(cwd: &Path, repo_root: &Path) -> String {
    repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| fallback_label_from_cwd(cwd))
}

pub(super) fn git_space_metadata_from_info(info: &GitWorktreeInfo) -> GitSpaceMetadata {
    let key = canonicalize_best_effort_path(&info.git_common_dir)
        .display()
        .to_string();
    let checkout_key = canonicalize_best_effort_path(&info.repo_root)
        .display()
        .to_string();
    let common_dir_name = info
        .git_common_dir
        .file_name()
        .and_then(|name| name.to_str());
    let label_path = match common_dir_name {
        Some(".git") => info.git_common_dir.parent().unwrap_or(&info.repo_root),
        Some(".bare") => embedded_bare_repo_container(info).unwrap_or(&info.git_common_dir),
        _ => &info.git_common_dir,
    };
    let repo_name = label_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .to_string();
    GitSpaceMetadata {
        key,
        checkout_key,
        repo_name,
        repo_root: info.repo_root.clone(),
        is_linked_worktree: info.is_linked_worktree,
    }
}

fn embedded_bare_repo_container(info: &GitWorktreeInfo) -> Option<&Path> {
    let parent = info.git_common_dir.parent()?;
    let parent_git_dir = git_dir_for_repo_root(parent)?;
    (canonicalize_best_effort_path(&parent_git_dir) == info.git_common_dir).then_some(parent)
}

pub(super) fn canonicalize_best_effort_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn git_common_dir_for_git_dir(git_dir: &Path) -> PathBuf {
    let commondir = git_dir.join("commondir");
    let Ok(contents) = std::fs::read_to_string(commondir) else {
        return git_dir.to_path_buf();
    };
    let path = Path::new(contents.trim());
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        git_dir.join(path)
    }
}

/// Outcome of reading one Git ref file, classified without collapsing metadata
/// errors so callers can distinguish "this ref does not exist" from "this ref
/// exists (or cannot be ruled out) but its content must not be trusted".
/// `Path::exists()` cannot distinguish them because it returns `false` on
/// metadata errors. When opening reports `NotFound` or `NotADirectory`,
/// `symlink_metadata` distinguishes a genuinely absent path from a dangling
/// symlink without following the final link.
pub(super) enum RefFileRead {
    Content(String),
    Absent,
    Unavailable,
}

pub(super) fn read_git_ref_file_state(path: &Path) -> RefFileRead {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return match std::fs::symlink_metadata(path) {
                // The directory entry exists but its symlink target is missing
                // or traverses a non-directory. Git treats the loose ref as
                // broken and does not fall back to an older packed ref.
                Ok(_) => RefFileRead::Unavailable,
                Err(metadata_error)
                    if metadata_error.kind() == std::io::ErrorKind::NotFound
                        || metadata_error.kind() == std::io::ErrorKind::NotADirectory =>
                {
                    RefFileRead::Absent
                }
                Err(_) => RefFileRead::Unavailable,
            };
        }
        // Permission or I/O errors: the ref may exist, so its identity is
        // unavailable rather than absent.
        Err(_) => return RefFileRead::Unavailable,
    };
    let mut contents = String::new();
    if file
        .take((MAX_GIT_REF_FILE_BYTES + 1) as u64)
        .read_to_string(&mut contents)
        .is_err()
        || contents.len() > MAX_GIT_REF_FILE_BYTES
    {
        return RefFileRead::Unavailable;
    }
    RefFileRead::Content(contents)
}

pub(super) fn read_git_ref_file(path: &Path) -> Option<String> {
    match read_git_ref_file_state(path) {
        RefFileRead::Content(contents) => Some(contents),
        RefFileRead::Absent | RefFileRead::Unavailable => None,
    }
}

pub fn git_branch(cwd: &Path) -> Option<String> {
    let repo_root = git_repo_root(cwd)?;
    let git_dir = git_dir_for_repo_root(&repo_root)?;
    let git_common_dir = git_common_dir_for_git_dir(&git_dir);
    if git_ref_storage_is_reftable(&git_common_dir) {
        return git_symbolic_head_short(&repo_root);
    }

    let head = read_git_ref_file(&git_dir.join("HEAD"))?;
    parse_git_head_branch(&head)
}

pub(super) fn git_dir_for_repo_root(repo_root: &Path) -> Option<PathBuf> {
    let git_path = repo_root.join(".git");
    if git_path.is_dir() {
        return Some(git_path);
    }

    if let Ok(gitdir) = std::fs::read_to_string(&git_path) {
        if let Some(relative) = gitdir.trim().strip_prefix("gitdir:").map(str::trim) {
            let resolved = Path::new(relative);
            return Some(if resolved.is_absolute() {
                resolved.to_path_buf()
            } else {
                repo_root.join(resolved)
            });
        }
    }

    if path_is_git_dir_layout(repo_root) && git_dir_is_bare(repo_root) {
        return Some(repo_root.to_path_buf());
    }

    None
}

fn path_is_git_dir_layout(path: &Path) -> bool {
    path.join("HEAD").is_file() && path.join("objects").is_dir() && path.join("refs").is_dir()
}

pub(super) fn git_symbolic_head_full(repo_root: &Path) -> Option<String> {
    git_trimmed_stdout(repo_root, &["symbolic-ref", "--quiet", "HEAD"])
}

fn git_symbolic_head_short(repo_root: &Path) -> Option<String> {
    git_trimmed_stdout(repo_root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
}

pub(super) fn git_rev_parse_verify(repo_root: &Path, revision: &str) -> Option<String> {
    git_trimmed_stdout(repo_root, &["rev-parse", "--verify", revision])
}

pub(super) fn git_ref_storage_is_reftable(git_common_dir: &Path) -> bool {
    read_git_config_value(&git_common_dir.join("config"), "extensions", "refstorage")
        .is_some_and(|value| value.eq_ignore_ascii_case("reftable"))
}

fn git_dir_is_bare(git_dir: &Path) -> bool {
    read_git_config_value(&git_dir.join("config"), "core", "bare")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

fn parse_git_head_branch(head: &str) -> Option<String> {
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    (!branch.is_empty()).then(|| branch.to_string())
}

fn read_git_config_value(path: &Path, section: &str, key: &str) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut in_section = false;
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section_name) = simple_git_config_section(line) {
            in_section = section_name.eq_ignore_ascii_case(section);
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(key) {
            return Some(strip_git_config_comment(value).trim().to_string());
        }
    }
    None
}

fn simple_git_config_section(line: &str) -> Option<&str> {
    let section = line.strip_prefix('[')?.split_once(']')?.0.trim();
    (!section.contains('"')).then_some(section)
}

fn strip_git_config_comment(value: &str) -> &str {
    let value = value.trim();
    for marker in ['#', ';'] {
        if let Some((prefix, _)) = value.split_once(marker) {
            if prefix.chars().next_back().is_some_and(char::is_whitespace) {
                return prefix;
            }
        }
    }
    value
}

fn git_trimmed_stdout(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = crate::noninteractive_process::command("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let stdout = stdout.trim();
    (!stdout.is_empty()).then(|| stdout.to_string())
}

pub(super) fn git_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };

    loop {
        if git_dir_for_repo_root(&current)
            .map(|git_dir| git_dir.join("HEAD").is_file())
            .unwrap_or(false)
        {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub(super) fn read_ref_oid(common_dir: &Path, full_ref: &str) -> Option<String> {
    let loose_ref = common_dir.join(full_ref);
    match read_git_ref_file_state(&loose_ref) {
        RefFileRead::Content(contents) => {
            let oid = contents.trim();
            if oid.is_empty() {
                // An empty loose ref is present but broken. Git does not fall
                // back to an older same-name packed ref in this case.
                return None;
            }
            return Some(oid.to_string());
        }
        // A loose ref that exists — or whose existence cannot be ruled out
        // because of a metadata or I/O error — must not fall back to
        // packed-refs: that could resurrect a stale same-name OID into the
        // status fingerprint. Report the ref as unavailable instead.
        RefFileRead::Unavailable => return None,
        RefFileRead::Absent => {}
    }

    let packed_refs = std::fs::read_to_string(common_dir.join("packed-refs")).ok()?;
    for line in packed_refs.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let oid = parts.next()?;
        let name = parts.next()?;
        if name == full_ref {
            return Some(oid.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::workspace::git::test_support::run_git;

    fn temp_test_dir(name: &str) -> PathBuf {
        let unique = format!(
            "herdr-workspace-tests-{}-{}-{}",
            name,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Points HOME at a temp directory for the duration of a test and restores
    /// the real value on drop, so no test ever reads or writes the real home
    /// directory. Tests using this MUST hold `env_lock()` first (the suite runs
    /// single-threaded, but the lock also encodes the intent).
    struct HomeDirGuard {
        previous: Option<std::ffi::OsString>,
    }

    /// The shared env-test lock, recovered from poisoning so one failing env
    /// test does not mask the verdicts of the others with PoisonError.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    impl HomeDirGuard {
        fn point_at(home: &Path) -> Self {
            let previous = std::env::var_os("HOME");
            std::env::set_var("HOME", home);
            Self { previous }
        }
    }

    impl Drop for HomeDirGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn git_branch_reads_head_from_standard_repo() {
        let root = temp_test_dir("standard-repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        assert_eq!(git_branch(&root).as_deref(), Some("main"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_loose_ref_is_unavailable_not_absent() {
        let root = temp_test_dir("oversized-loose-ref");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted \naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .unwrap();
        let loose = refs_dir.join("main");
        std::fs::write(&loose, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(loose)
            .unwrap()
            .set_len(8 * 1024 * 1024)
            .unwrap();

        let oid = read_ref_oid(&root, "refs/heads/main");
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            oid, None,
            "an oversized loose ref must make the ref unavailable, not fall back to the stale packed OID"
        );
    }

    #[test]
    fn empty_or_whitespace_loose_ref_is_unavailable_not_absent() {
        let root = temp_test_dir("empty-loose-ref");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .unwrap();
        let loose = refs_dir.join("main");

        for contents in ["", " \n\t"] {
            std::fs::write(&loose, contents).unwrap();
            assert_eq!(
                read_ref_oid(&root, "refs/heads/main"),
                None,
                "an empty or whitespace-only loose ref must not fall back to the stale packed OID"
            );
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_loose_ref_is_unavailable_not_absent() {
        use std::os::unix::fs::symlink;

        let root = temp_test_dir("dangling-symlink-loose-ref");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .unwrap();
        symlink("missing-target", refs_dir.join("main")).unwrap();

        let oid = read_ref_oid(&root, "refs/heads/main");
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            oid, None,
            "a dangling loose ref must not fall back to the stale packed OID"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_through_file_is_unavailable_not_absent() {
        use std::os::unix::fs::symlink;

        let root = temp_test_dir("dangling-symlink-through-file");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .unwrap();
        std::fs::write(refs_dir.join("target-parent"), "not a directory").unwrap();
        symlink("target-parent/nested", refs_dir.join("main")).unwrap();

        let oid = read_ref_oid(&root, "refs/heads/main");
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            oid, None,
            "a dangling loose ref whose target traverses a file must not fall back to the stale packed OID"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loop_loose_ref_is_unavailable_not_absent() {
        use std::os::unix::fs::symlink;

        let root = temp_test_dir("symlink-loop-loose-ref");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .unwrap();
        // Unlike chmod(000), a symlink loop produces an open error even as root.
        // It exercises the same unavailable-ref branch as permission errors.
        let loose_ref = refs_dir.join("main");
        symlink("main", &loose_ref).unwrap();
        let open_error = std::fs::File::open(&loose_ref).unwrap_err();
        let oid = read_ref_oid(&root, "refs/heads/main");
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(open_error.raw_os_error(), Some(libc::ELOOP));
        assert_eq!(
            oid, None,
            "a loose ref behind an open error must be unavailable, not fall back to the stale packed OID"
        );
    }

    #[test]
    fn ref_path_through_a_file_still_reads_packed_refs() {
        let root = temp_test_dir("ref-path-through-file");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).unwrap();
        // refs/heads/main is a file, so refs/heads/main/nested cannot exist as
        // a loose ref; the packed entry is the legitimate source.
        std::fs::write(
            refs_dir.join("main"),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        )
        .unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main/nested\n",
        )
        .unwrap();

        let oid = read_ref_oid(&root, "refs/heads/main/nested");
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            oid.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn absent_loose_ref_still_reads_packed_refs() {
        let root = temp_test_dir("packed-only-ref");
        std::fs::create_dir_all(root.join("refs/heads")).unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .unwrap();

        let oid = read_ref_oid(&root, "refs/heads/main");
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            oid.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn oversized_git_head_is_rejected() {
        let root = temp_test_dir("oversized-head");
        let git_dir = root.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let head = git_dir.join("HEAD");
        std::fs::write(&head, "ref: refs/heads/main\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(head)
            .unwrap()
            .set_len(60 * 1024 * 1024)
            .unwrap();

        let branch = git_branch(&root);
        let branch_len = branch.as_ref().map(String::len);
        std::fs::remove_dir_all(root).unwrap();

        assert!(
            branch.is_none(),
            "oversized Git HEAD produced branch with {branch_len:?} bytes"
        );
    }

    #[test]
    fn git_branch_reads_head_from_worktree_gitdir_file() {
        let root = temp_test_dir("worktree");
        let worktree_git_dir = root.join(".bare/worktrees/feature");
        std::fs::create_dir_all(&worktree_git_dir).unwrap();
        std::fs::write(root.join(".git"), "gitdir: .bare/worktrees/feature\n").unwrap();
        std::fs::write(worktree_git_dir.join("HEAD"), "ref: refs/heads/feature\n").unwrap();

        assert_eq!(git_branch(&root).as_deref(), Some("feature"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_branch_returns_none_for_detached_head() {
        let root = temp_test_dir("detached-head");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "3e1b9a8d\n").unwrap();

        assert_eq!(git_branch(&root), None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_branch_reads_symbolic_head_from_reftable_repo() {
        let root = temp_test_dir("reftable-branch");
        let root_arg = root.to_string_lossy().to_string();
        let output = std::process::Command::new("git")
            .args(["init", "--ref-format=reftable", "-b", "main", &root_arg])
            .output()
            .unwrap();
        if !output.status.success() {
            std::fs::remove_dir_all(root).unwrap();
            return;
        }

        assert_eq!(git_branch(&root).as_deref(), Some("main"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_repo_root_ignores_invalid_git_marker() {
        let base = temp_test_dir("invalid-git-root");
        let cwd = base.join("workspace");
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        assert_eq!(git_repo_root(&cwd), None);

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_repo_root_ignores_standalone_non_bare_git_dir_layout() {
        let root = temp_test_dir("standalone-non-bare-git-dir");
        std::fs::write(root.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(root.join("objects")).unwrap();
        std::fs::create_dir_all(root.join("refs")).unwrap();
        std::fs::write(root.join("config"), "[core]\n\tbare = false\n").unwrap();

        assert_eq!(git_repo_root(&root.join("refs")), None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_space_metadata_supports_standalone_bare_repo() {
        let bare = temp_test_dir("bare-space");
        run_git(&bare, &["init", "--bare", "."]);
        let nested = bare.join("refs");

        let info = git_worktree_info(&nested).expect("bare repo should be discovered");
        assert!(info.is_bare);
        assert!(!info.is_linked_worktree);
        assert_eq!(info.git_dir, canonicalize_best_effort_path(&bare));

        let metadata = git_space_metadata(&nested).expect("bare repo should map to a git space");
        assert_eq!(
            canonicalize_best_effort_path(&metadata.repo_root),
            canonicalize_best_effort_path(&bare)
        );
        assert!(!metadata.is_linked_worktree);

        std::fs::remove_dir_all(bare).unwrap();
    }

    #[test]
    fn bare_source_and_linked_checkout_share_repo_name_but_not_auto_label() {
        let (base, bare, checkout) =
            crate::workspace::git::test_support::create_bare_repo_with_linked_worktree(
                "bare-linked-labels",
            );

        let bare_space = git_space_metadata(&bare).unwrap();
        let checkout_space = git_space_metadata(&checkout).unwrap();
        let bare_auto_label = automatic_workspace_label(&bare, &bare_space.repo_root);
        let checkout_auto_label = automatic_workspace_label(&checkout, &checkout_space.repo_root);

        assert_eq!(bare_space.key, checkout_space.key);
        assert_eq!(bare_space.repo_name, ".bare");
        assert_eq!(checkout_space.repo_name, bare_space.repo_name);
        assert_eq!(bare_auto_label, bare.file_name().unwrap().to_str().unwrap());
        assert_eq!(
            checkout_auto_label,
            checkout.file_name().unwrap().to_str().unwrap()
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn embedded_dot_bare_source_and_checkout_use_container_repo_name() {
        let base = temp_test_dir("embedded-dot-bare");
        let seed = base.join("seed");
        let repo = base.join("reported-repo");
        let bare = repo.join(".bare");
        let checkout = repo.join("develop");
        std::fs::create_dir_all(&seed).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&seed, &["init", "--quiet"]);
        run_git(&seed, &["config", "user.email", "herdr@example.invalid"]);
        run_git(&seed, &["config", "user.name", "Herdr Test"]);
        run_git(
            &seed,
            &["commit", "--quiet", "--allow-empty", "-m", "initial"],
        );
        run_git(
            &base,
            &[
                "clone",
                "--quiet",
                "--bare",
                seed.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        std::fs::write(repo.join(".git"), "gitdir: ./.bare\n").unwrap();
        run_git(
            &bare,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "develop",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );

        let source = git_space_metadata(&repo).unwrap();
        let linked = git_space_metadata(&checkout).unwrap();

        assert_eq!(source.repo_name, "reported-repo");
        assert_eq!(linked.repo_name, source.repo_name);

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_space_metadata_marks_bare_dot_git_repo() {
        let root = temp_test_dir("bare-dot-git");
        run_git(&root, &["init", "--bare", ".git"]);

        let info = git_worktree_info(&root).expect("bare .git repo should be discovered");
        assert!(info.is_bare);
        assert!(!info.is_linked_worktree);
        assert_eq!(
            info.git_dir,
            canonicalize_best_effort_path(&root.join(".git"))
        );

        let metadata = git_space_metadata(&root).expect("bare .git repo should map to a git space");
        assert_eq!(
            canonicalize_best_effort_path(&metadata.repo_root),
            canonicalize_best_effort_path(&root)
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn derive_label_prefers_repo_root_name() {
        let root = temp_test_dir("label-repo");
        let nested = root.join("nested");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            derive_label_from_cwd(&nested),
            root.file_name().and_then(|name| name.to_str()).unwrap()
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn derive_label_uses_path_name_outside_git() {
        let root = temp_test_dir("label-plain");
        let label = root.file_name().and_then(|name| name.to_str()).unwrap();

        assert_eq!(derive_label_from_cwd(Path::new(&root)), label);

        std::fs::remove_dir_all(root).unwrap();
    }

    fn write_git_repo_marker(root: &Path) {
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    #[test]
    fn derive_label_falls_back_when_settings_name_absent_or_empty() {
        let root = temp_test_dir("settings-fallback");
        write_git_repo_marker(&root);
        std::fs::create_dir_all(root.join(".herdr")).unwrap();
        let expected = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();

        // No [workspace] table.
        std::fs::write(root.join(".herdr/settings.toml"), "[other]\nkey = 1\n").unwrap();
        assert_eq!(derive_label_from_cwd(&root), expected);

        // Empty/whitespace name.
        std::fs::write(
            root.join(".herdr/settings.toml"),
            "[workspace]\nname = \"  \"\n",
        )
        .unwrap();
        assert_eq!(derive_label_from_cwd(&root), expected);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn derive_label_ignores_settings_above_the_repo_root() {
        let base = temp_test_dir("settings-above-repo");
        // A project file ABOVE the repository must not leak into it: the walk
        // stops at the repository root (inclusive), so this file is never read.
        std::fs::create_dir_all(base.join(".herdr")).unwrap();
        std::fs::write(
            base.join(".herdr/settings.toml"),
            "[workspace]\nname = \"outer-pin\"\n",
        )
        .unwrap();
        let repo = base.join("repo");
        write_git_repo_marker(&repo);
        let nested = repo.join("sub");
        std::fs::create_dir_all(&nested).unwrap();

        let expected = repo.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            derive_label_from_cwd(&nested),
            expected,
            "a settings file above the repository root must be ignored"
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn derive_label_ignores_malformed_settings() {
        let root = temp_test_dir("settings-malformed");
        write_git_repo_marker(&root);
        std::fs::create_dir_all(root.join(".herdr")).unwrap();
        std::fs::write(
            root.join(".herdr/settings.toml"),
            "[workspace\nname = \"x\n",
        )
        .unwrap();

        let expected = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        assert_eq!(derive_label_from_cwd(&root), expected);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persist_workspace_name_creates_file_at_repo_root() {
        let root = temp_test_dir("persist-create");
        let nested = root.join("sub");
        write_git_repo_marker(&root);
        std::fs::create_dir_all(&nested).unwrap();

        persist_workspace_name(&nested, "created").unwrap();

        let written = std::fs::read_to_string(root.join(".herdr/settings.toml")).unwrap();
        assert!(written.contains("[workspace]"));
        assert!(written.contains("name = \"created\""));
        // round-trip: derive finds it from the nested cwd.
        assert_eq!(derive_label_from_cwd(&nested), "created");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persist_workspace_name_updates_existing_preserving_other_keys() {
        let root = temp_test_dir("persist-update");
        write_git_repo_marker(&root);
        std::fs::create_dir_all(root.join(".herdr")).unwrap();
        std::fs::write(
            root.join(".herdr/settings.toml"),
            "[other]\nkey = \"keep-me\"\n[workspace]\nname = \"old\"\n",
        )
        .unwrap();

        persist_workspace_name(&root, "new").unwrap();

        let written = std::fs::read_to_string(root.join(".herdr/settings.toml")).unwrap();
        assert!(
            written.contains("keep-me"),
            "unrelated keys must be preserved"
        );
        assert!(written.contains("name = \"new\""));
        assert!(!written.contains("name = \"old\""));
        assert_eq!(derive_label_from_cwd(&root), "new");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persist_workspace_name_in_non_git_dir() {
        let root = temp_test_dir("persist-nogit");
        persist_workspace_name(&root, "plain").unwrap();

        let written = std::fs::read_to_string(root.join(".herdr/settings.toml")).unwrap();
        assert!(written.contains("name = \"plain\""));
        assert_eq!(derive_label_from_cwd(&root), "plain");

        std::fs::remove_dir_all(root).unwrap();
    }

    // THE HOME TRAP. Outside a repository the walk used to climb to the
    // filesystem root, so a ~/.herdr/settings.toml (written by renaming any
    // workspace whose cwd was the home directory) silently pinned EVERY later
    // non-git workspace under home, and a rename from below home updated the
    // home file in place. The walk must never read a settings file from HOME
    // or / unless it started there.
    #[test]
    fn derive_label_ignores_home_settings_below_home() {
        let _env = env_lock();
        let home = temp_test_dir("settings-home-below");
        let _guard = HomeDirGuard::point_at(&home);
        std::fs::create_dir_all(home.join(".herdr")).unwrap();
        std::fs::write(
            home.join(".herdr/settings.toml"),
            "[workspace]\nname = \"home-pin\"\n",
        )
        .unwrap();
        let cwd = home.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        let expected = cwd.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            derive_label_from_cwd(&cwd),
            expected,
            "a home-directory settings file must not pin a workspace below home"
        );

        std::fs::remove_dir_all(home).unwrap();
    }

    // OVERSHOOT CONTROL: parent folders BETWEEN the cwd and home still apply,
    // so a non-git project folder with subfolders keeps working.
    #[test]
    fn derive_label_uses_parent_settings_below_home() {
        let _env = env_lock();
        let home = temp_test_dir("settings-home-parent");
        let _guard = HomeDirGuard::point_at(&home);
        let proj = home.join("proj");
        std::fs::create_dir_all(proj.join(".herdr")).unwrap();
        std::fs::write(
            proj.join(".herdr/settings.toml"),
            "[workspace]\nname = \"proj-pin\"\n",
        )
        .unwrap();
        let cwd = proj.join("sub");
        std::fs::create_dir_all(&cwd).unwrap();

        assert_eq!(derive_label_from_cwd(&cwd), "proj-pin");

        std::fs::remove_dir_all(home).unwrap();
    }

    // The home workspace itself keeps its own file: the walk's starting
    // directory is always in range.
    #[test]
    fn derive_label_uses_home_settings_for_the_home_workspace() {
        let _env = env_lock();
        let home = temp_test_dir("settings-home-root");
        let _guard = HomeDirGuard::point_at(&home);
        std::fs::create_dir_all(home.join(".herdr")).unwrap();
        std::fs::write(
            home.join(".herdr/settings.toml"),
            "[workspace]\nname = \"home-pin\"\n",
        )
        .unwrap();

        assert_eq!(derive_label_from_cwd(&home), "home-pin");

        std::fs::remove_dir_all(home).unwrap();
    }

    // WRITE SIDE of the same trap: a rename below home must not update the
    // home file as the "nearest existing" one; it creates the project file at
    // the workspace's own directory instead.
    #[test]
    fn persist_workspace_name_below_home_does_not_touch_home_settings() {
        let _env = env_lock();
        let home = temp_test_dir("settings-home-persist");
        let _guard = HomeDirGuard::point_at(&home);
        std::fs::create_dir_all(home.join(".herdr")).unwrap();
        std::fs::write(
            home.join(".herdr/settings.toml"),
            "[workspace]\nname = \"home-pin\"\n",
        )
        .unwrap();
        let cwd = home.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        persist_workspace_name(&cwd, "renamed").unwrap();

        let home_file = std::fs::read_to_string(home.join(".herdr/settings.toml")).unwrap();
        assert!(
            home_file.contains("home-pin") && !home_file.contains("renamed"),
            "the home file must be left alone: {home_file}"
        );
        let work_file = std::fs::read_to_string(cwd.join(".herdr/settings.toml"))
            .expect("the rename should create the project file at the workspace cwd");
        assert!(work_file.contains("name = \"renamed\""), "{work_file}");
        assert_eq!(derive_label_from_cwd(&cwd), "renamed");

        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn git_rev_parse_verify_reads_reftable_refs() {
        let root = temp_test_dir("reftable-ref-oid");
        let root_arg = root.to_string_lossy().to_string();
        let output = std::process::Command::new("git")
            .args(["init", "--ref-format=reftable", "-b", "main", &root_arg])
            .output()
            .unwrap();
        if !output.status.success() {
            std::fs::remove_dir_all(root).unwrap();
            return;
        }

        run_git(&root, &["config", "user.email", "herdr@example.invalid"]);
        run_git(&root, &["config", "user.name", "Herdr Test"]);
        run_git(&root, &["commit", "--allow-empty", "-m", "initial"]);

        let head_oid = git_rev_parse_verify(&root, "HEAD").unwrap();

        assert_eq!(
            git_rev_parse_verify(&root, "refs/heads/main").as_deref(),
            Some(head_oid.as_str())
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
