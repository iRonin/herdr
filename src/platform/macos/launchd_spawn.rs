//! Starts the server daemon outside the launching terminal app.
//!
//! A process started from a terminal stays in that terminal app's launchd job
//! and resource coalition, even after `setsid`. When the app quits, macOS
//! removes its job and kills what is left inside it, so a server started from
//! Ghostty died with every pane on Cmd-Q (herdr#4920).
//!
//! The daemon is therefore submitted as a one-shot job in the user's GUI
//! launchd domain, which gives it a coalition of its own. The job forks the real
//! server out of the job, so the server is not a running GUI job that logout
//! would stop; it keeps the lifetime a directly spawned server had.
//!
//! The job's own process does not exit, though: it stays as an idle keeper for
//! as long as any server it started (or a live-handoff replacement of one)
//! runs. A coalition whose launchd job process has exited has no owner, and
//! LaunchServices then hands it to the first GUI app that checks in from it:
//! LibreOffice or a browser started in a pane. That app's record then lists
//! the server and every pane process, and force-quitting it from the Dock (or
//! Background Task Management refusing it) kills them all. While the job's
//! process lives, no app ever owns the coalition and quitting one ends only
//! that app.

use std::ffi::{CString, OsStr};
use std::io::{self, Write};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LAUNCHCTL: &str = "/bin/launchctl";
const LABEL_PREFIX: &str = "dev.herdr.server.";
/// Tells the job process to fork the server out of the job and stay as its
/// keeper; never inherited by panes.
const DETACH_ENV: &str = "HERDR_LAUNCHD_DETACH";
/// `<fd>:<keeper pid>` for a live-handoff replacement: the keeper pipe's write
/// end it inherited. Consumed and removed at startup, like `DETACH_ENV`, and
/// never written into a launchd job.
const KEEPER_ENV: &str = "HERDR_LAUNCHD_KEEPER";
/// Test-only: leave the job the old way (job process exits at once), so a test
/// can show in the same run that the keeper is what makes the difference.
#[cfg(test)]
const TEST_NO_KEEPER_ENV: &str = "HERDR_LAUNCHD_TEST_NO_KEEPER";
/// Finished jobs stay loaded until logout; older ones are removed on the next launch.
const STALE_JOB_AGE: Duration = Duration::from_secs(60);

/// Submits `command` (program, arguments, environment changes, working
/// directory) as a run-once job in the caller's GUI launchd domain. Fails, so
/// the caller can spawn directly, when there is no GUI domain or the command
/// cannot be represented exactly in a launchd job. Returns the job label.
pub(crate) fn spawn_launchd_job(command: &Command) -> io::Result<String> {
    if std::env::var_os("SSH_CONNECTION").is_some() {
        // SSH logins already run outside any terminal app; keep them out of
        // the desktop session too.
        return Err(io::Error::other("started over SSH"));
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let label = format!("{LABEL_PREFIX}{}.{}", std::process::id(), now.as_nanos());
    let working_dir = match command.get_current_dir() {
        Some(dir) => Some(dir.to_path_buf()),
        None => std::env::current_dir().ok(),
    };
    let plist = job_plist(&label, command, working_dir.as_deref(), current_umask())?;
    remove_stale_jobs(&domain, now);

    let dir = PrivateDir::create()?;
    let plist_path = dir.path().join("job.plist");
    write_private_file(&plist_path, plist.as_bytes())?;
    launchctl(&["bootstrap", &domain, &plist_path.to_string_lossy()])?;
    Ok(label)
}

/// The launchd job process that stays while this server runs, and this
/// server's copy of the pipe whose last closed write end lets it exit.
#[derive(Clone, Copy, Debug)]
struct Keeper {
    pid: libc::pid_t,
    write_fd: RawFd,
}

static KEEPER: OnceLock<Keeper> = OnceLock::new();

/// The pid of the launchd job process kept alive for this server, if any.
pub(crate) fn launcher_keeper_pid() -> Option<u32> {
    KEEPER.get().map(|keeper| keeper.pid as u32)
}

/// Called first thing in the server process. Inside the launchd job, forks
/// the real server out of the job into its own session; the job process stays
/// as the keeper (see the module comment). A live-handoff replacement instead
/// takes over the keeper pipe it inherited.
/// Returns a warning to log once logging is up.
pub(crate) fn leave_launchd_job() -> Option<String> {
    // Both markers are read and removed first. A launchd job inherits no
    // descriptors, so inside a job a keeper variable can only be a stale copy
    // (a pane of a rolled-back older server keeps one): it is ignored, and the
    // job is left as always.
    let keeper = std::env::var_os(KEEPER_ENV);
    std::env::remove_var(KEEPER_ENV);
    if std::env::var_os(DETACH_ENV).is_none() {
        return keeper.and_then(|value| adopt_inherited_keeper(&value));
    }
    std::env::remove_var(DETACH_ENV);
    // launchd names the job here; panes should see what a terminal shell sees.
    std::env::set_var("XPC_SERVICE_NAME", "0");
    #[cfg(test)]
    if std::env::var_os(TEST_NO_KEEPER_ENV).is_some() {
        std::env::remove_var(TEST_NO_KEEPER_ENV);
        return leave_without_keeper();
    }
    leave_with_keeper()
}

/// The server leaves the job by a double fork, so its parent is launchd, as
/// with a directly spawned daemon, and the keeper never has to reap it. The
/// job process keeps only the read end of a pipe and exits when the last write
/// end closes: when this server and any handoff replacement it passed the
/// write end to have exited.
fn leave_with_keeper() -> Option<String> {
    let mut fds: [RawFd; 2] = [-1; 2];
    // SAFETY: pipe writes two descriptors into the array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        let err = io::Error::last_os_error();
        return leave_without_keeper().or(Some(format!(
            "server left its launchd job without a keeper (pipe failed: {err}); quitting an app started in a pane can stop this server"
        )));
    }
    let [read_fd, write_fd] = fds;
    // Close-on-exec on both ends: no program this server runs inherits the
    // write end, so none of them can keep the keeper alive after the server.
    // Only a live-handoff replacement gets it (`pass_keeper_to_handoff`).
    set_cloexec(read_fd, true);
    set_cloexec(write_fd, true);
    // SAFETY: getpid has no preconditions.
    let keeper_pid = unsafe { libc::getpid() };
    // SAFETY: runs before the server creates threads. The parent becomes the
    // keeper and never returns; the child forks again and exits, and the
    // grandchild continues as the server. No destructors run in the processes
    // that leave through _exit.
    match unsafe { libc::fork() } {
        -1 => {
            let err = io::Error::last_os_error();
            close_fd(read_fd);
            close_fd(write_fd);
            // Keep serving from the job process; logout will stop this server.
            Some(format!("server could not leave its launchd job: {err}"))
        }
        0 => {
            close_fd(read_fd);
            let mut warning = None;
            // SAFETY: as above, still single-threaded.
            match unsafe { libc::fork() } {
                0 => {}
                // The intermediate process: the keeper reaps it.
                pid if pid > 0 => unsafe { libc::_exit(0) },
                // No second fork: this process serves, with the keeper as parent.
                _ => {
                    warning = Some(format!(
                        "server could not leave its launchd job by a double fork ({}); the keeper is its parent",
                        io::Error::last_os_error()
                    ));
                }
            }
            let _ = KEEPER.set(Keeper {
                pid: keeper_pid,
                write_fd,
            });
            // SAFETY: plain syscall.
            if unsafe { libc::setsid() } < 0 {
                let session = format!(
                    "server could not start its own session after leaving launchd: {}",
                    io::Error::last_os_error()
                );
                return Some(match warning {
                    Some(first) => format!("{first}; {session}"),
                    None => session,
                });
            }
            warning
        }
        intermediate => {
            close_fd(write_fd);
            run_keeper(read_fd, intermediate)
        }
    }
}

/// The launchd job process from here on: nothing but the read end of the pipe
/// and stdin, stdout and stderr on /dev/null. No logging, no exec, no threads.
fn run_keeper(read_fd: RawFd, intermediate: libc::pid_t) -> ! {
    close_other_fds(read_fd);
    // SAFETY: open/dup2/close on a literal path and the standard descriptors.
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for std_fd in 0..3 {
                libc::dup2(null, std_fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
    }
    // SAFETY: waits for the child this process forked.
    while unsafe { libc::waitpid(intermediate, std::ptr::null_mut(), 0) } < 0 {
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
    }
    let mut byte = 0u8;
    loop {
        // SAFETY: reads at most one byte into a local.
        let read = unsafe { libc::read(read_fd, (&raw mut byte).cast(), 1) };
        if read == 0 {
            break;
        }
        if read < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
    }
    // SAFETY: ends the keeper without running anything else.
    unsafe { libc::_exit(0) }
}

/// The behaviour before the keeper: the job process forks the server and
/// exits at once. Kept for the case where no pipe can be made, and for tests.
fn leave_without_keeper() -> Option<String> {
    // SAFETY: runs before the server creates threads; the child continues as
    // the server and the parent exits without running destructors.
    match unsafe { libc::fork() } {
        // SAFETY: plain syscall in the new child.
        0 if unsafe { libc::setsid() } < 0 => Some(format!(
            "server could not start its own session after leaving launchd: {}",
            io::Error::last_os_error()
        )),
        0 => None,
        // Keep serving from the job process; logout will stop this server.
        -1 => Some(format!(
            "server could not leave its launchd job: {}",
            io::Error::last_os_error()
        )),
        _ => unsafe { libc::_exit(0) },
    }
}

/// A live-handoff replacement keeps the keeper alive with the write end it
/// inherited. Anything else is not adopted, with a warning.
fn adopt_inherited_keeper(value: &OsStr) -> Option<String> {
    match inherited_keeper(value) {
        Ok(keeper) => {
            let _ = KEEPER.set(keeper);
            None
        }
        Err(warning) => Some(warning),
    }
}

fn inherited_keeper(value: &OsStr) -> Result<Keeper, String> {
    let parsed = value.to_str().and_then(|text| {
        let (fd, pid) = text.split_once(':')?;
        Some((fd.parse::<RawFd>().ok()?, pid.parse::<libc::pid_t>().ok()?))
    });
    let Some((write_fd, pid)) = parsed.filter(|(fd, pid)| *fd > 2 && *pid > 1) else {
        return Err(format!("ignoring a malformed {KEEPER_ENV} value {value:?}"));
    };
    // Inherited without close-on-exec; restore it in every case, adopted or
    // not, so no program this server runs inherits the descriptor.
    set_cloexec(write_fd, true);
    if !is_pipe(write_fd) {
        return Err(format!(
            "ignoring {KEEPER_ENV}: descriptor {write_fd} is not an open pipe"
        ));
    }
    // SAFETY: fcntl reads the descriptor's status flags.
    let access = unsafe { libc::fcntl(write_fd, libc::F_GETFL) };
    if access < 0 || access & libc::O_ACCMODE != libc::O_WRONLY {
        return Err(format!(
            "ignoring {KEEPER_ENV}: descriptor {write_fd} is not a pipe's write end"
        ));
    }
    // A real keeper is alive and shares this server's coalition by
    // construction (the server was forked from it).
    // SAFETY: getpid has no preconditions; signal 0 only probes.
    let (own, alive) = unsafe { (libc::getpid(), libc::kill(pid, 0) == 0) };
    let same_coalition =
        resource_coalition_of(pid).is_some_and(|id| Some(id) == resource_coalition_of(own));
    if !alive || !same_coalition {
        return Err(format!(
            "ignoring {KEEPER_ENV}: the keeper {pid} is gone; quitting an app started in a pane can stop this server"
        ));
    }
    Ok(Keeper { pid, write_fd })
}

/// The resource coalition `pid` belongs to, if it can be read.
fn resource_coalition_of(pid: libc::pid_t) -> Option<u64> {
    const PROC_PIDCOALITIONINFO: libc::c_int = 20;
    // struct proc_pidcoalitioninfo: coalition_id[2], then three reserved words.
    let mut info = [0u64; 5];
    // SAFETY: the buffer is the size proc_pidcoalitioninfo needs.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDCOALITIONINFO,
            0,
            info.as_mut_ptr().cast(),
            std::mem::size_of_val(&info) as libc::c_int,
        )
    };
    (read > 0).then_some(info[0])
}

/// Lets a live-handoff replacement inherit this server's keeper pipe: the
/// write end loses close-on-exec in the replacement's child process only, and
/// its number and the keeper's pid travel in `KEEPER_ENV`. Without a keeper,
/// nothing changes.
pub(crate) fn pass_keeper_to_handoff(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    let Some(keeper) = KEEPER.get().copied() else {
        return;
    };
    command.env(KEEPER_ENV, format!("{}:{}", keeper.write_fd, keeper.pid));
    let fd = keeper.write_fd;
    // SAFETY: fcntl is async-signal-safe and changes only the child's copy.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn set_cloexec(fd: RawFd, on: bool) {
    // SAFETY: fcntl on a descriptor this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            let flags = if on {
                flags | libc::FD_CLOEXEC
            } else {
                flags & !libc::FD_CLOEXEC
            };
            libc::fcntl(fd, libc::F_SETFD, flags);
        }
    }
}

fn close_fd(fd: RawFd) {
    // SAFETY: closes a descriptor this process owns.
    unsafe {
        libc::close(fd);
    }
}

fn is_pipe(fd: RawFd) -> bool {
    // SAFETY: fstat fills a zeroed stat for a descriptor number.
    unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut stat) == 0 && (stat.st_mode & libc::S_IFMT) == libc::S_IFIFO
    }
}

/// Closes every descriptor of this process except 0, 1, 2 and `keep`.
fn close_other_fds(keep: RawFd) {
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(256);
    loop {
        let capacity = fds.capacity();
        // SAFETY: proc_pidinfo writes at most `capacity` entries into the buffer.
        let bytes = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                (capacity * entry) as libc::c_int,
            )
        };
        if bytes <= 0 {
            close_fds_below_limit(keep);
            return;
        }
        let count = bytes as usize / entry;
        if count < capacity {
            // SAFETY: proc_pidinfo initialised `count` entries.
            unsafe { fds.set_len(count) };
            break;
        }
        // The list may have been cut short: ask again with twice the room.
        fds = Vec::with_capacity(capacity * 2);
    }
    for info in fds {
        if info.proc_fd > 2 && info.proc_fd != keep {
            close_fd(info.proc_fd);
        }
    }
}

/// When the descriptor list cannot be read: closes every number from 3 up to
/// the soft descriptor limit (launchd's 256 here; the keeper forks before the
/// server raises it), except `keep`.
fn close_fds_below_limit(keep: RawFd) {
    // SAFETY: getrlimit fills a zeroed struct.
    let limit = unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
            limit.rlim_cur
        } else {
            libc::rlim_t::MAX
        }
    };
    let end = limit.min(65_536) as RawFd;
    for fd in 3..end {
        if fd != keep {
            close_fd(fd);
        }
    }
}

/// A fresh directory only this user can enter, removed with its contents.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn create() -> io::Result<Self> {
        let template = std::env::temp_dir().join("herdr-launchd.XXXXXXXX");
        let mut bytes = CString::new(template.as_os_str().as_bytes())
            .map_err(|_| io::Error::other("temporary path contains a NUL byte"))?
            .into_bytes_with_nul();
        // SAFETY: mkdtemp edits the NUL-terminated template in place and
        // creates the directory with mode 0700, failing if it exists.
        let created = unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) };
        if created.is_null() {
            return Err(io::Error::last_os_error());
        }
        bytes.pop();
        Ok(Self(PathBuf::from(OsStr::from_bytes(&bytes))))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)
}

fn launchctl(args: &[&str]) -> io::Result<String> {
    let output = Command::new(LAUNCHCTL)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(io::Error::other(format!(
            "launchctl {} failed ({}): {}",
            args.first().copied().unwrap_or_default(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn job_is_running(target: &str) -> bool {
    launchctl(&["print", target]).is_ok_and(|info| {
        info.lines()
            .any(|line| line.trim_start().starts_with("pid = "))
    })
}

/// Removes finished herdr server jobs from earlier launches. Running jobs and
/// recent ones (which may still be starting) are left alone.
fn remove_stale_jobs(domain: &str, now: Duration) {
    let Ok(list) = launchctl(&["list"]) else {
        return;
    };
    for label in stale_job_labels(&list, now) {
        let target = format!("{domain}/{label}");
        if job_is_running(&target) {
            continue;
        }
        if let Err(err) = launchctl(&["bootout", &target]) {
            tracing::debug!(%err, label, "could not remove finished server job");
        }
    }
}

fn stale_job_labels(list_output: &str, now: Duration) -> Vec<&str> {
    list_output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let pid = fields.next()?.trim();
            let label = fields.nth(1)?.trim();
            let started = label
                .strip_prefix(LABEL_PREFIX)?
                .rsplit_once('.')?
                .1
                .parse::<u128>()
                .ok()?;
            let stale =
                pid == "-" && now.as_nanos().saturating_sub(started) >= STALE_JOB_AGE.as_nanos();
            stale.then_some(label)
        })
        .collect()
}

/// Panes inherit the server's umask; launchd would otherwise apply its own.
/// Reading it briefly sets the process mask to 077.
fn current_umask() -> libc::mode_t {
    // SAFETY: umask only swaps the process mask; it is restored immediately.
    unsafe {
        let mask = libc::umask(0o077);
        libc::umask(mask);
        mask
    }
}

fn job_plist(
    label: &str,
    command: &Command,
    working_dir: Option<&Path>,
    umask: libc::mode_t,
) -> io::Result<String> {
    let mut plist = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<plist version=\"1.0\">\n<dict>\n",
    ));
    push_key_string(&mut plist, "Label", label)?;
    plist.push_str("<key>ProgramArguments</key>\n<array>\n");
    push_string(&mut plist, command.get_program())?;
    for arg in command.get_args() {
        push_string(&mut plist, arg)?;
    }
    plist.push_str("</array>\n<key>EnvironmentVariables</key>\n<dict>\n");
    for (key, value) in resolved_environment(command) {
        // A keeper pipe never reaches a job; a copy in the caller's
        // environment would only be stale.
        if key == KEEPER_ENV {
            continue;
        }
        push_key(&mut plist, &key)?;
        push_string(&mut plist, &value)?;
    }
    push_key_string(&mut plist, DETACH_ENV, "1")?;
    plist.push_str("</dict>\n");
    if let Some(dir) = working_dir {
        push_key(&mut plist, OsStr::new("WorkingDirectory"))?;
        push_string(&mut plist, dir.as_os_str())?;
    }
    for key in ["StandardInPath", "StandardOutPath", "StandardErrorPath"] {
        push_key_string(&mut plist, key, "/dev/null")?;
    }
    // Run once and never restart. The forked server must survive the job
    // process exiting, and keeps normal scheduling priority.
    plist.push_str(concat!(
        "<key>RunAtLoad</key>\n<true/>\n",
        "<key>KeepAlive</key>\n<false/>\n",
        "<key>AbandonProcessGroup</key>\n<true/>\n",
    ));
    push_key_string(&mut plist, "ProcessType", "Interactive")?;
    plist.push_str(&format!("<key>Umask</key>\n<integer>{umask}</integer>\n"));
    plist.push_str("</dict>\n</plist>\n");
    Ok(plist)
}

fn resolved_environment(command: &Command) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut env: Vec<_> = std::env::vars_os().collect();
    for (key, value) in command.get_envs() {
        env.retain(|(existing, _)| existing != key);
        if let Some(value) = value {
            env.push((key.to_os_string(), value.to_os_string()));
        }
    }
    env
}

fn push_key_string(plist: &mut String, key: &str, value: &str) -> io::Result<()> {
    push_key(plist, OsStr::new(key))?;
    push_string(plist, OsStr::new(value))
}

fn push_key(plist: &mut String, key: &OsStr) -> io::Result<()> {
    plist.push_str("<key>");
    push_escaped(plist, key)?;
    plist.push_str("</key>\n");
    Ok(())
}

fn push_string(plist: &mut String, value: &OsStr) -> io::Result<()> {
    plist.push_str("<string>");
    push_escaped(plist, value)?;
    plist.push_str("</string>\n");
    Ok(())
}

/// Escapes for XML text. Rejects what a plist cannot carry byte-for-byte
/// (invalid UTF-8, control characters other than tab and newline, CR) so the
/// caller falls back to a direct spawn instead of silently altering a value.
fn push_escaped(plist: &mut String, value: &OsStr) -> io::Result<()> {
    let text = value
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "value is not UTF-8"))?;
    for ch in text.chars() {
        match ch {
            '&' => plist.push_str("&amp;"),
            '<' => plist.push_str("&lt;"),
            '>' => plist.push_str("&gt;"),
            '"' => plist.push_str("&quot;"),
            '\t' | '\n' => plist.push(ch),
            c if (c as u32) < 0x20 || c == '\u{7f}' || c == '\u{fffe}' || c == '\u{ffff}' => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "value contains a control character",
                ));
            }
            c => plist.push(c),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn only_old_finished_herdr_jobs_are_stale() {
        let now = Duration::from_secs(1_000);
        let old = Duration::from_secs(900).as_nanos();
        let recent = Duration::from_secs(990).as_nanos();
        let list = format!(
            "PID\tStatus\tLabel\n\
             -\t0\tdev.herdr.server.12.{old}\n\
             42\t0\tdev.herdr.server.13.{old}\n\
             -\t0\tdev.herdr.server.14.{recent}\n\
             -\t0\tcom.example.other.{old}\n"
        );

        assert_eq!(
            stale_job_labels(&list, now),
            vec![format!("dev.herdr.server.12.{old}").as_str()]
        );
    }

    #[test]
    fn plist_escapes_values_and_never_restarts() {
        let mut command = Command::new("/opt/her<dr>");
        command
            .arg("a&b")
            .env("HERDR_TEST_VALUE", "\"q\"\tx")
            .env_remove("HOME");
        let plist = job_plist(
            "dev.herdr.server.1.2",
            &command,
            Some(Path::new("/tmp")),
            0o077,
        )
        .expect("plist");

        assert!(plist.contains("<string>/opt/her&lt;dr&gt;</string>"));
        assert!(plist.contains("<string>a&amp;b</string>"));
        assert!(plist.contains("<key>HERDR_TEST_VALUE</key>\n<string>&quot;q&quot;\tx</string>"));
        assert!(!plist.contains("<key>HOME</key>"));
        assert!(plist.contains(&format!("<key>{DETACH_ENV}</key>")));
        assert!(plist.contains("<key>KeepAlive</key>\n<false/>"));
        assert!(plist.contains("<key>WorkingDirectory</key>\n<string>/tmp</string>"));
        assert!(plist.contains("<key>Umask</key>\n<integer>63</integer>"));
    }

    #[test]
    fn values_a_plist_would_alter_are_rejected() {
        for value in [
            std::ffi::OsString::from("prompt \u{1b}[0m"),
            std::ffi::OsString::from("line\rbreak"),
            std::ffi::OsString::from_vec(vec![0x66, 0xff]),
        ] {
            let mut command = Command::new("/bin/true");
            command.env("HERDR_TEST_VALUE", &value);
            assert!(
                job_plist("dev.herdr.server.1.2", &command, None, 0o022).is_err(),
                "{value:?}"
            );
        }
    }

    #[test]
    fn private_dir_is_owner_only_and_removed() {
        use std::os::unix::fs::PermissionsExt;

        let path = {
            let dir = PrivateDir::create().expect("dir");
            let mode = std::fs::metadata(dir.path())
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
            let file = dir.path().join("job.plist");
            write_private_file(&file, b"x").expect("write");
            let mode = std::fs::metadata(&file).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            assert!(
                write_private_file(&file, b"y").is_err(),
                "never reuses a file"
            );
            dir.path().to_path_buf()
        };
        assert!(!path.exists());
    }

    fn try_coalition(pid: libc::pid_t) -> Option<u64> {
        const PROC_PIDCOALITIONINFO: libc::c_int = 20;
        let mut info = [0u64; 5];
        let size = std::mem::size_of_val(&info) as libc::c_int;
        // SAFETY: the buffer is large enough for proc_pidcoalitioninfo.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                PROC_PIDCOALITIONINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        (read > 0).then_some(info[0])
    }

    fn resource_coalition(pid: u32) -> u64 {
        try_coalition(pid as libc::pid_t)
            .unwrap_or_else(|| panic!("coalition lookup failed for {pid}"))
    }

    /// Every signal these tests send goes through here: never to pid 0 or 1,
    /// which would reach the test's own process group (cargo, the shell) or
    /// launchd.
    fn signal_pid(pid: libc::pid_t, signal: libc::c_int) {
        assert!(pid > 1, "refusing to signal pid {pid}");
        // SAFETY: a process the calling test started, checked above.
        unsafe { libc::kill(pid, signal) };
    }

    #[test]
    fn a_detached_child_stays_in_the_callers_coalition() {
        // The failure this module avoids: setsid does not leave the coalition.
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("sleep");
        command.arg("5");
        // SAFETY: setsid is async-signal-safe and touches only the child.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = command.spawn().expect("spawn");
        let coalition = resource_coalition(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(coalition, resource_coalition(std::process::id()));
    }

    const DETACH_HELPER_OUT: &str = "HERDR_LAUNCHD_DETACH_HELPER_OUT";
    /// Makes `detach_helper` put a stale keeper variable into its own
    /// environment before it leaves the job.
    const DETACH_HELPER_STALE_KEEPER: &str = "HERDR_LAUNCHD_DETACH_HELPER_STALE_KEEPER";
    /// A keeper variable no launchd job can hold for real: descriptor 9 of a
    /// job is never a keeper pipe.
    const STALE_KEEPER_VALUE: &str = "9:4242";

    /// Body of the launchd job in `detached_server_outlives_its_launcher_job`.
    #[test]
    #[ignore = "runs only as the launchd job of another test"]
    fn detach_helper() {
        let Some(out) = std::env::var_os(DETACH_HELPER_OUT) else {
            return;
        };
        if std::env::var_os(DETACH_HELPER_STALE_KEEPER).is_some() {
            std::env::set_var(KEEPER_ENV, STALE_KEEPER_VALUE);
        }
        let warning = leave_launchd_job();
        // The launcher exits right after forking; wait until that is visible.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        // SAFETY: plain process query.
        while unsafe { libc::getppid() } != 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        // SAFETY: plain process queries.
        let (pid, ppid, sid) = unsafe { (libc::getpid(), libc::getppid(), libc::getsid(0)) };
        let line = format!(
            "{pid} {ppid} {sid} {} {} {} {}",
            std::env::var("XPC_SERVICE_NAME").unwrap_or_default(),
            std::env::var_os(DETACH_ENV).is_some(),
            warning.is_some(),
            std::env::var_os(KEEPER_ENV).is_some(),
        );
        let _ = std::fs::write(out, line);
        std::thread::sleep(Duration::from_secs(30));
    }

    /// A pane-like child: its own session, below the server.
    fn pane_like_child(mut command: Command) -> io::Result<std::process::Child> {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches only the child.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        command.spawn()
    }

    fn pane_like_sleeper() -> io::Result<std::process::Child> {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        pane_like_child(command)
    }

    /// Gates every test that submits a launchd job. Such a test goes through
    /// `spawn_launchd_job`, which, like every herdr start, first boots out every
    /// FINISHED `dev.herdr.server.*` label older than 60 s, whoever started it:
    /// the live servers' finished launcher labels included. That is harmless (a
    /// finished label has no process left, and a running one, such as a
    /// keeper's, is skipped), but these tests do not touch only their own jobs.
    fn gui_domain_available() -> Option<libc::uid_t> {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        (launchctl(&["print", &format!("gui/{uid}")]).is_ok()
            && std::env::var_os("SSH_CONNECTION").is_none())
        .then_some(uid)
    }

    fn wait_for_output(out: &Path) -> String {
        // Longer than `wait_for_ready`: a helper writes only after its children are ready.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::fs::metadata(out).map_or(true, |meta| meta.len() == 0)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::read_to_string(out).expect("job output")
    }

    /// One `detach_helper` job: what it wrote, whether its job was still
    /// running just before its bootout, and what was left of the server after.
    struct DetachRun {
        written: String,
        launcher_running: bool,
        alive_after_bootout: bool,
        coalition: Option<u64>,
    }

    fn run_detach_job(uid: libc::uid_t, with_keeper: bool, env: &[(&str, &str)]) -> DetachRun {
        let dir = PrivateDir::create().expect("dir");
        let out = dir.path().join("out");
        let mut command = Command::new(std::env::current_exe().expect("test binary"));
        command
            .args([
                "--exact",
                "platform::macos::launchd_spawn::tests::detach_helper",
            ])
            .args(["--ignored", "--test-threads=1"])
            .env(DETACH_HELPER_OUT, &out);
        if !with_keeper {
            command.env(NO_KEEPER, "1");
        }
        for (key, value) in env {
            command.env(key, value);
        }

        let label = spawn_launchd_job(&command).expect("launchd job");
        let target = format!("gui/{uid}/{label}");
        let written = std::panic::catch_unwind(|| wait_for_output(&out));
        let Ok(written) = written else {
            let _ = launchctl(&["bootout", &target]);
            panic!("the detach helper wrote nothing");
        };
        if with_keeper {
            // The job's process stays as the keeper while the server runs.
            std::thread::sleep(Duration::from_millis(300));
        } else {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while job_is_running(&target) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let launcher_running = job_is_running(&target);
        // Removing the job must not take the detached server with it.
        let _ = launchctl(&["bootout", &target]);
        std::thread::sleep(Duration::from_millis(300));

        let pid: libc::pid_t = written
            .split(' ')
            .next()
            .and_then(|pid| pid.parse().ok())
            .unwrap_or(0);
        let alive_after_bootout = alive(pid);
        let coalition = try_coalition(pid);
        if alive_after_bootout {
            signal_pid(pid, libc::SIGKILL);
        }
        DetachRun {
            written,
            launcher_running,
            alive_after_bootout,
            coalition,
        }
    }

    fn assert_detached(run: &DetachRun) {
        let written = &run.written;
        let fields: Vec<&str> = written.split(' ').collect();
        assert_eq!(fields.len(), 7, "the helper wrote every field: {written}");
        assert!(
            run.alive_after_bootout,
            "the detached server survives: {written}"
        );
        assert_eq!(fields[1], "1", "reparented to launchd: {written}");
        assert_eq!(fields[2], fields[0], "leads its own session: {written}");
        assert_eq!(fields[3], "0", "job name hidden from panes: {written}");
        assert_eq!(fields[4], "false", "detach marker consumed: {written}");
        assert_eq!(fields[5], "false", "no detach warning: {written}");
        assert_eq!(fields[6], "false", "keeper variable removed: {written}");
        assert!(
            run.coalition
                .is_some_and(|id| id != resource_coalition(std::process::id())),
            "outside the caller's coalition: {written}"
        );
    }

    /// Both ways a server leaves its job. Without the keeper (servers of
    /// older builds, or one whose pipe failed) the job finishes, and every herdr
    /// start boots out such finished jobs: that must not take the server.
    /// Needs a GUI login session; skipped only where the gui domain is absent.
    #[test]
    fn detached_server_outlives_its_launcher_job() {
        let Some(uid) = gui_domain_available() else {
            eprintln!("skipping: no gui launchd domain for this test process");
            return;
        };
        let finished = run_detach_job(uid, false, &[]);
        assert!(
            !finished.launcher_running,
            "the launcher job finished: {}",
            finished.written
        );
        assert_detached(&finished);

        let kept = run_detach_job(uid, true, &[]);
        assert!(
            kept.launcher_running,
            "the launcher job stays as the keeper: {}",
            kept.written
        );
        assert_detached(&kept);
    }

    /// A keeper variable left in a client's environment (a pane of a
    /// rolled-back older server keeps one) must not stop the server from
    /// leaving its job, whether it reaches the job from the client or is in
    /// the job's own environment. Needs a GUI login session.
    #[test]
    fn a_stale_keeper_variable_never_keeps_a_server_in_its_job() {
        let Some(uid) = gui_domain_available() else {
            eprintln!("skipping: no gui launchd domain for this test process");
            return;
        };
        let from_client = run_detach_job(uid, true, &[(KEEPER_ENV, STALE_KEEPER_VALUE)]);
        let in_job = run_detach_job(uid, true, &[(DETACH_HELPER_STALE_KEEPER, "1")]);
        for run in [&from_client, &in_job] {
            assert!(
                run.launcher_running,
                "the launcher job stays as the keeper: {}",
                run.written
            );
            assert_detached(run);
        }
    }

    #[test]
    fn a_job_never_carries_a_keeper_variable() {
        let mut command = Command::new("/bin/echo");
        command
            .env(KEEPER_ENV, STALE_KEEPER_VALUE)
            .env("HERDR_LAUNCHD_TEST", "yes");
        let plist = job_plist("dev.herdr.server.1.2", &command, None, 0o022).expect("plist");
        assert!(
            plist.contains("<key>HERDR_LAUNCHD_TEST</key>"),
            "the environment reaches the job: {plist}"
        );
        assert!(!plist.contains(KEEPER_ENV), "{plist}");
    }

    /// Needs a GUI login session; skipped only where the gui domain is absent.
    #[test]
    fn job_runs_outside_the_callers_coalition_and_is_not_restarted() {
        let Some(uid) = gui_domain_available() else {
            eprintln!("skipping: no gui launchd domain for this test process");
            return;
        };
        let dir = PrivateDir::create().expect("dir");
        let out = dir.path().join("out");
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("printf '%s %s %s' $$ \"$HERDR_LAUNCHD_TEST\" \"${HERDR_LAUNCHD_DETACH-unset}\" > out; exec sleep 30")
            .env("HERDR_LAUNCHD_TEST", "yes")
            .current_dir(dir.path());

        let label = spawn_launchd_job(&command).expect("launchd job");
        let written = wait_for_output(&out);
        let mut fields = written.split(' ');
        let pid: u32 = fields.next().and_then(|pid| pid.parse().ok()).expect("pid");
        let job_coalition = resource_coalition(pid);
        let own_coalition = resource_coalition(std::process::id());

        signal_pid(pid as libc::pid_t, libc::SIGKILL);
        std::thread::sleep(Duration::from_millis(500));
        let target = format!("gui/{uid}/{label}");
        let restarted = job_is_running(&target);
        let _ = launchctl(&["bootout", &target]);

        assert_eq!(fields.next(), Some("yes"), "environment reached the job");
        assert_eq!(fields.next(), Some("1"), "the job is told to detach");
        assert_ne!(job_coalition, own_coalition, "job has its own coalition");
        assert!(!restarted, "launchd does not restart the server");
    }

    // ---- The keeper: a pane's GUI app must never own the server's coalition ----

    const KEEPER_HELPER_OUT: &str = "HERDR_KEEPER_HELPER_OUT";
    const KEEPER_HELPER_MODE: &str = "HERDR_KEEPER_HELPER_MODE";
    /// A file a helper's child creates once its setup is done.
    const PANE_READY: &str = "HERDR_KEEPER_PANE_READY";
    const CHECKIN_HELPER: &str = "HERDR_KEEPER_CHECKIN_HELPER";
    /// Same literal as `TEST_NO_KEEPER_ENV` in the module (kept literal here so
    /// these tests also build, and fail, against code without the keeper).
    const NO_KEEPER: &str = "HERDR_LAUNCHD_TEST_NO_KEEPER";

    fn alive(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only probes.
        pid > 0 && unsafe { libc::kill(pid, 0) } == 0
    }

    fn helper_command(test: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test binary"));
        command
            .args([
                "--exact",
                &format!("platform::macos::launchd_spawn::tests::{test}"),
            ])
            .args(["--ignored", "--test-threads=1"]);
        command
    }

    /// Waits until a child has created its ready file. The tests signal the
    /// server or the child as soon as the helper's line is written, so the
    /// helper writes it only after this (a child that never got ready is
    /// written as pid 0, which the tests' `pid > 1` checks reject).
    fn wait_for_ready(ready: &Path) -> bool {
        wait_until(Duration::from_secs(10), || ready.exists())
    }

    /// Body of the server launchd job in the keeper tests: leaves the job as a
    /// real server does, then starts the children its mode asks for, waits
    /// until they are ready, and writes `<server pid> <child pid> <child pid>`.
    #[test]
    #[ignore = "runs only as the launchd job of another test"]
    fn keeper_helper() {
        let (Some(out), Some(mode)) = (
            std::env::var_os(KEEPER_HELPER_OUT),
            std::env::var(KEEPER_HELPER_MODE).ok(),
        ) else {
            return;
        };
        let _ = leave_launchd_job();
        let children: Vec<u32> = match mode.as_str() {
            // a pane process and a GUI app started from a pane
            "app" => {
                // The app must check in only after the job's process (or, with
                // the keeper, the intermediate one) has exited, as a pane's app
                // does: while the job's process still runs, no app gets the
                // coalition, and the arm without the keeper would prove nothing.
                // SAFETY: plain process query.
                let left = wait_until(Duration::from_secs(10), || unsafe { libc::getppid() } == 1);
                let pane = pane_like_sleeper().map_or(0, |child| child.id());
                let app = if left {
                    helper_command("checkin_helper")
                        .env(CHECKIN_HELPER, "1")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .map_or(0, |child| child.id())
                } else {
                    0
                };
                vec![pane, app]
            }
            // an ordinary child of the server and a real pane child on a pty
            "children" => {
                let ordinary = Command::new("/bin/sleep")
                    .arg("30")
                    .spawn()
                    .map_or(0, |child| child.id());
                let pty = portable_pty::native_pty_system()
                    .openpty(portable_pty::PtySize::default())
                    .expect("pty");
                // Ignores the hangup the pty gets when the server dies, so it
                // is still alive when the test looks at the keeper. The shell
                // sets that itself (portable-pty resets signal dispositions in
                // the child) and then reports ready: the test kills the server
                // only after that.
                let ready = Path::new(&out).with_extension("pane-ready");
                let mut pane = portable_pty::CommandBuilder::new("/bin/sh");
                pane.arg("-c");
                pane.arg(format!(
                    "trap '' HUP; : > \"${PANE_READY}\"; exec /bin/sleep 30"
                ));
                pane.env(PANE_READY, &ready);
                let pane = pty.slave.spawn_command(pane).expect("pane child");
                let pane_pid = if wait_for_ready(&ready) {
                    pane.process_id().unwrap_or(0)
                } else {
                    0
                };
                // Keep the pty open so the pane is not hung up.
                std::mem::forget(pty);
                std::mem::forget(pane);
                vec![ordinary, pane_pid]
            }
            // a live-handoff replacement, and a detached daemon that is not one
            "handoff" => {
                let spawn = |args: &[&str]| {
                    let mut command = Command::new("/bin/sh");
                    command
                        .args(["-c", "exec /bin/sleep 30", "sh"])
                        .args(args)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null());
                    crate::platform::detach_server_daemon_command(&mut command);
                    command.spawn().map_or(0, |child| child.id())
                };
                let replacement = spawn(&["server", "--handoff-import", "socket", "token"]);
                let other = spawn(&["server"]);
                vec![replacement, other]
            }
            // a pane-like child; both it and this server log every stop signal
            "bootout" => {
                let out = Path::new(&out);
                log_signals_to(&out.with_extension("server-signals"));
                let ready = out.with_extension("pane-ready");
                let mut pane = helper_command("signal_logger_helper");
                pane.env(SIGNAL_LOG_HELPER, out.with_extension("pane-signals"))
                    .env(PANE_READY, &ready)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let pane = pane_like_child(pane).map_or(0, |child| child.id());
                vec![if wait_for_ready(&ready) { pane } else { 0 }, 0]
            }
            _ => return,
        };
        // SAFETY: plain process query.
        let pid = unsafe { libc::getpid() };
        let line = format!("{pid} {} {}", children[0], children[1]);
        let _ = std::fs::write(out, line);
        std::thread::sleep(Duration::from_secs(30));
    }

    const SIGNAL_LOG_HELPER: &str = "HERDR_KEEPER_SIGNAL_LOG";
    static SIGNAL_LOG_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

    extern "C" fn log_signal(sig: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
        let fd = SIGNAL_LOG_FD.load(std::sync::atomic::Ordering::Relaxed);
        if fd < 0 {
            return;
        }
        // SAFETY: the kernel passes a valid siginfo with SA_SIGINFO.
        let sender = if info.is_null() {
            0
        } else {
            unsafe { (*info).si_pid }
        };
        let mut line = [0u8; 48];
        let mut len = 0;
        let mut push = |bytes: &[u8]| {
            for &byte in bytes {
                if len < line.len() {
                    line[len] = byte;
                    len += 1;
                }
            }
        };
        let number = |value: i64, push: &mut dyn FnMut(&[u8])| {
            let mut digits = [0u8; 20];
            let mut at = digits.len();
            let mut rest = value.unsigned_abs();
            loop {
                at -= 1;
                digits[at] = b'0' + (rest % 10) as u8;
                rest /= 10;
                if rest == 0 {
                    break;
                }
            }
            push(&digits[at..]);
        };
        push(b"sig ");
        number(i64::from(sig), &mut push);
        push(b" from ");
        number(i64::from(sender), &mut push);
        push(b"; ");
        // SAFETY: write is async-signal-safe; the buffer is a local.
        unsafe { libc::write(fd, line.as_ptr().cast(), len) };
    }

    /// Logs every stop signal this process gets, with its sender, and keeps
    /// running (so the test sees both what arrived and whether it was fatal).
    fn log_signals_to(path: &Path) {
        use std::os::fd::IntoRawFd;
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        else {
            return;
        };
        SIGNAL_LOG_FD.store(file.into_raw_fd(), std::sync::atomic::Ordering::Relaxed);
        for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
            // SAFETY: installs a handler that only calls async-signal-safe code.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                let handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                    log_signal;
                action.sa_sigaction = handler as usize;
                action.sa_flags = libc::SA_SIGINFO;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }

    /// A pane process that logs every stop signal it gets.
    #[test]
    #[ignore = "runs only as a child of keeper_helper"]
    fn signal_logger_helper() {
        let Some(path) = std::env::var_os(SIGNAL_LOG_HELPER) else {
            return;
        };
        log_signals_to(Path::new(&path));
        // The handlers are in place: the test may signal this pane from now on.
        if let Some(ready) = std::env::var_os(PANE_READY) {
            let _ = std::fs::File::create(ready);
        }
        std::thread::sleep(Duration::from_secs(30));
    }

    #[repr(C)]
    struct ProcessSerialNumber {
        high: u32,
        low: u32,
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn TransformProcessType(psn: *const ProcessSerialNumber, transform: u32) -> i32;
    }

    /// A GUI app as a pane starts one: checks in with LaunchServices (as a
    /// UIElement, so no Dock icon), the way soffice or Chrome does.
    #[test]
    #[ignore = "runs only as a child of keeper_helper"]
    fn checkin_helper() {
        if std::env::var_os(CHECKIN_HELPER).is_none() {
            return;
        }
        const CURRENT_PROCESS: u32 = 2;
        const TO_UI_ELEMENT: u32 = 4;
        let psn = ProcessSerialNumber {
            high: 0,
            low: CURRENT_PROCESS,
        };
        // SAFETY: a valid serial number for the current process.
        unsafe { TransformProcessType(&psn, TO_UI_ELEMENT) };
        std::thread::sleep(Duration::from_secs(30));
    }

    /// One run of the server helper as a launchd job. Dropping it stops what
    /// it started and boots out its label, also when a test panics.
    struct HelperJob {
        _dir: PrivateDir,
        out: PathBuf,
        target: String,
        label: String,
        pids: Vec<libc::pid_t>,
        /// The server's coalition: only processes still in it are signalled,
        /// so a reused pid is never hit.
        coalition: Option<u64>,
        stopped: std::cell::Cell<bool>,
    }

    impl HelperJob {
        fn start(uid: libc::uid_t, mode: &str, with_keeper: bool) -> Self {
            let dir = PrivateDir::create().expect("dir");
            let out = dir.path().join("out");
            let mut command = helper_command("keeper_helper");
            command
                .env(KEEPER_HELPER_OUT, &out)
                .env(KEEPER_HELPER_MODE, mode);
            if !with_keeper {
                command.env(NO_KEEPER, "1");
            }
            let label = spawn_launchd_job(&command).expect("launchd job");
            // From here on, Drop boots out the label even if a step below panics.
            let mut job = Self {
                _dir: dir,
                out,
                target: format!("gui/{uid}/{label}"),
                label,
                pids: Vec::new(),
                coalition: None,
                stopped: std::cell::Cell::new(false),
            };
            let written = wait_for_output(&job.out);
            job.pids = written
                .split(' ')
                .map(|pid| pid.trim().parse().expect("pid"))
                .collect();
            job.coalition = job
                .pids
                .first()
                .copied()
                .filter(|&pid| pid > 1)
                .and_then(try_coalition);
            job
        }

        fn listed(&self) -> bool {
            launchctl(&["list"]).is_ok_and(|list| {
                list.lines()
                    .any(|line| line.split('\t').nth(2) == Some(self.label.as_str()))
            })
        }

        fn signals(&self, who: &str) -> String {
            std::fs::read_to_string(self.out.with_extension(format!("{who}-signals")))
                .unwrap_or_default()
        }

        /// The pid launchd shows for the job: the keeper while it runs.
        fn job_pid(&self) -> Option<libc::pid_t> {
            let list = launchctl(&["list"]).ok()?;
            list.lines().find_map(|line| {
                let mut fields = line.split('\t');
                let pid = fields.next()?;
                (fields.nth(1)? == self.label).then(|| pid.parse().ok())?
            })
        }

        /// Kills this job's own processes and its keeper (pid above 1 and
        /// still in the server's coalition), then boots out its own label.
        /// Runs once; Drop calls it too.
        fn stop(&self) {
            if self.stopped.replace(true) {
                return;
            }
            let own = |pid: libc::pid_t| {
                pid > 1 && self.coalition.is_some() && try_coalition(pid) == self.coalition
            };
            let keeper = self.job_pid();
            for pid in self.pids.iter().copied().chain(keeper) {
                if own(pid) {
                    signal_pid(pid, libc::SIGKILL);
                }
            }
            std::thread::sleep(Duration::from_millis(200));
            let _ = launchctl(&["bootout", &self.target]);
        }
    }

    impl Drop for HelperJob {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// (descriptor, PROX_FDTYPE_*) of every open descriptor of `pid`, sorted.
    fn open_fd_types(pid: libc::pid_t) -> Vec<(i32, u32)> {
        let entry = std::mem::size_of::<libc::proc_fdinfo>();
        let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(64);
        // SAFETY: proc_pidinfo writes at most the buffer's capacity.
        let bytes = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                (fds.capacity() * entry) as libc::c_int,
            )
        };
        if bytes <= 0 {
            return Vec::new();
        }
        // SAFETY: proc_pidinfo initialised this many entries.
        unsafe { fds.set_len(bytes as usize / entry) };
        let mut list: Vec<(i32, u32)> = fds
            .iter()
            .map(|info| (info.proc_fd, info.proc_fdtype))
            .collect();
        list.sort_unstable();
        list
    }

    fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + limit;
        while !done() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    fn lsappinfo() -> String {
        Command::new("/usr/bin/lsappinfo")
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default()
    }

    /// Does any LaunchServices app record list `pid` as one of its coalition's
    /// processes (`coalition: <id> { <pid> ... }`)?
    fn an_app_owns_coalition_of(pid: libc::pid_t) -> bool {
        let pid = pid.to_string();
        lsappinfo().lines().any(|line| {
            line.trim_start().starts_with("coalition:")
                && line
                    .split(['{', '}'])
                    .nth(1)
                    .is_some_and(|pids| pids.split_whitespace().any(|p| p == pid))
        })
    }

    /// What happens when a GUI app started from a pane is quit the way the
    /// Dock's Force Quit does it (`lsappinfo kill -coalition`). Without the
    /// keeper (same run, test switch) the app owns the server's coalition and
    /// the kill takes the server and the pane with it; with it, neither.
    /// Needs a GUI login session; skipped only where the gui domain is absent.
    #[test]
    fn a_pane_app_never_owns_the_server_coalition() {
        let Some(uid) = gui_domain_available() else {
            eprintln!("skipping: no gui launchd domain for this test process");
            return;
        };
        let own = resource_coalition(std::process::id());
        let mut seen = Vec::new();
        for with_keeper in [false, true] {
            let job = HelperJob::start(uid, "app", with_keeper);
            let (server, pane, app) = (job.pids[0], job.pids[1], job.pids[2]);
            assert!(
                server > 1 && pane > 1 && app > 1,
                "the helper started every process: {:?}",
                job.pids
            );
            let server_coalition = resource_coalition(server as u32);
            // The coalition kill below must only ever reach this job's
            // coalition, never this test's own (a terminal, or a herdr server).
            let safe = server_coalition != own
                && app > 0
                && resource_coalition(app as u32) == server_coalition;
            let checked_in = wait_until(Duration::from_secs(10), || {
                lsappinfo().contains(&format!("pid = {app} "))
            });
            let owned = an_app_owns_coalition_of(server);
            if safe && checked_in {
                let _ = Command::new("/usr/bin/lsappinfo")
                    .args(["kill", "-coalition", &app.to_string()])
                    .output();
                std::thread::sleep(Duration::from_millis(1500));
            }
            seen.push((
                with_keeper,
                safe,
                checked_in,
                owned,
                alive(server),
                alive(pane),
            ));
            job.stop();
        }
        let [old, kept] = [seen[0], seen[1]];
        assert!(
            old.1 && kept.1,
            "each job ran in its own coalition: {seen:?}"
        );
        assert!(
            old.2 && kept.2,
            "the app checked in with LaunchServices: {seen:?}"
        );
        assert!(
            old.3 && !old.4 && !old.5,
            "without the keeper the app owns the server's coalition and its kill takes server and pane: {seen:?}"
        );
        assert!(
            !kept.3 && kept.4 && kept.5,
            "with the keeper no app owns the coalition; server and pane survive: {seen:?}"
        );
    }

    /// The keeper runs exactly as long as a server holds its pipe: neither an
    /// ordinary child of the server nor a pane keeps it, a live-handoff
    /// replacement does, and a detached daemon that is not one does not.
    /// Needs a GUI login session; skipped only where the gui domain is absent.
    #[test]
    fn the_launcher_job_lives_exactly_as_long_as_its_servers() {
        let Some(uid) = gui_domain_available() else {
            eprintln!("skipping: no gui launchd domain for this test process");
            return;
        };

        let job = HelperJob::start(uid, "children", true);
        let (server, ordinary, pane) = (job.pids[0], job.pids[1], job.pids[2]);
        assert!(
            server > 1 && ordinary > 1 && pane > 1,
            "the helper started every process: {:?}",
            job.pids
        );
        let keeper = job.job_pid();
        let keeper_in_coalition = keeper
            .is_some_and(|k| resource_coalition(k as u32) == resource_coalition(server as u32));
        let keeper_fds = keeper.map(open_fd_types).unwrap_or_default();
        signal_pid(server, libc::SIGKILL);
        let keeper_left = wait_until(Duration::from_secs(3), || job.job_pid().is_none());
        let children = (alive(ordinary), alive(pane));
        job.stop();
        assert!(
            keeper.is_some_and(|k| k != server) && keeper_in_coalition,
            "the job's process stays as the keeper while the server runs: {keeper:?}"
        );
        assert_eq!(
            keeper_fds
                .iter()
                .map(|(fd, _)| *fd)
                .take(3)
                .collect::<Vec<_>>(),
            vec![0, 1, 2],
            "the keeper keeps its standard descriptors: {keeper_fds:?}"
        );
        assert!(
            keeper_fds.len() == 4 && keeper_fds[3].1 == libc::PROX_FDTYPE_PIPE as u32,
            "the keeper holds nothing else but the pipe: {keeper_fds:?}"
        );
        assert_eq!(children, (true, true), "children outlived the server");
        assert!(
            keeper_left,
            "neither an ordinary child nor a pane keeps the keeper alive"
        );

        let job = HelperJob::start(uid, "handoff", true);
        let (server, replacement, other) = (job.pids[0], job.pids[1], job.pids[2]);
        assert!(
            server > 1 && replacement > 1 && other > 1,
            "the helper started every process: {:?}",
            job.pids
        );
        let keeper = job.job_pid();
        signal_pid(server, libc::SIGKILL);
        std::thread::sleep(Duration::from_millis(1500));
        let kept_by_replacement = job.job_pid() == keeper && keeper.is_some();
        signal_pid(replacement, libc::SIGKILL);
        let keeper_left = wait_until(Duration::from_secs(3), || job.job_pid().is_none());
        let other_alive = alive(other);
        job.stop();
        assert!(
            kept_by_replacement,
            "a live-handoff replacement keeps the keeper after the server exits: {keeper:?}"
        );
        assert!(other_alive, "the other daemon was still running");
        assert!(
            keeper_left,
            "a detached daemon that is not a replacement does not keep the keeper"
        );
    }

    /// What logout or a person's `launchctl bootout` does to a server whose
    /// keeper is RUNNING: the keeper and the job go; the server and its panes
    /// stay and get no signal. Needs a GUI login session; skipped only where
    /// the gui domain is absent.
    #[test]
    fn bootout_of_a_running_keeper_leaves_the_server_and_its_panes() {
        let Some(uid) = gui_domain_available() else {
            eprintln!("skipping: no gui launchd domain for this test process");
            return;
        };
        let job = HelperJob::start(uid, "bootout", true);
        let (server, pane) = (job.pids[0], job.pids[1]);
        assert!(
            server > 1 && pane > 1,
            "the helper started a server and a pane: {:?}",
            job.pids
        );
        let keeper = job.job_pid();
        let running_before = job_is_running(&job.target);
        let _ = launchctl(&["bootout", &job.target]);
        std::thread::sleep(Duration::from_secs(2));
        let (server_alive, pane_alive) = (alive(server), alive(pane));
        let keeper_gone = keeper.is_some_and(|keeper| !alive(keeper));
        let label_gone = !job.listed();
        let signals = format!(
            "server: [{}] pane: [{}]",
            job.signals("server"),
            job.signals("pane")
        );
        // Positive control for the empty logs: a signal sent on purpose is
        // recorded, with this test as its sender, so an empty log means no
        // signal arrived, not a logger that cannot write.
        let sent = format!("sig {} from {}; ", libc::SIGHUP, std::process::id());
        for pid in [server, pane] {
            if alive(pid) {
                signal_pid(pid, libc::SIGHUP);
            }
        }
        let recorded = wait_until(Duration::from_secs(3), || {
            job.signals("server").contains(&sent) && job.signals("pane").contains(&sent)
        });
        let control = format!(
            "server: [{}] pane: [{}]",
            job.signals("server"),
            job.signals("pane")
        );
        job.stop();
        assert!(
            running_before && keeper.is_some_and(|keeper| keeper != server),
            "the keeper was running before the bootout: {keeper:?}"
        );
        assert!(
            server_alive && pane_alive,
            "server and pane survive the bootout: {signals}"
        );
        assert!(keeper_gone, "the bootout ended the keeper");
        assert!(label_gone, "the bootout removed the job");
        assert_eq!(
            signals, "server: [] pane: []",
            "no signal reached the server or the pane"
        );
        assert!(
            recorded,
            "the logs record a signal sent on purpose: {control}"
        );
    }

    /// A replacement adopts only the write end of an open pipe whose keeper is
    /// alive and in its own coalition, and restores close-on-exec in every case.
    #[test]
    fn a_replacement_takes_over_only_an_open_pipe() {
        use std::os::fd::AsRawFd;
        let cloexec = |fd: RawFd| {
            // SAFETY: reads the descriptor flags.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            flags >= 0 && flags & libc::FD_CLOEXEC != 0
        };
        let mut fds = [0; 2];
        // SAFETY: pipe writes two descriptors.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let [read_end, write_end] = fds;
        // A live process in this test's coalition stands in for the keeper.
        let mut stand_in = Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("keeper stand-in");
        let keeper = stand_in.id();
        // A keeper that is gone: a child already reaped.
        let mut gone = Command::new("/usr/bin/true").spawn().expect("child");
        let _ = gone.wait();
        let gone = gone.id();

        set_cloexec(write_end, false);
        let adopted =
            inherited_keeper(OsStr::new(&format!("{write_end}:{keeper}"))).expect("write end");
        assert_eq!(
            (adopted.write_fd, adopted.pid),
            (write_end, keeper as libc::pid_t)
        );
        assert!(cloexec(write_end), "close-on-exec restored");

        set_cloexec(read_end, false);
        let read = inherited_keeper(OsStr::new(&format!("{read_end}:{keeper}")));
        let read_cloexec = cloexec(read_end);
        set_cloexec(write_end, false);
        let dead = inherited_keeper(OsStr::new(&format!("{write_end}:{gone}")));
        let dead_cloexec = cloexec(write_end);
        let _ = stand_in.kill();
        let _ = stand_in.wait();
        assert!(read.is_err(), "the read end is refused");
        assert!(
            read_cloexec,
            "close-on-exec set on a refused descriptor too"
        );
        assert!(dead.is_err(), "a keeper that is gone is refused");
        assert!(
            dead_cloexec,
            "close-on-exec set on a refused descriptor too"
        );

        assert!(inherited_keeper(OsStr::new("0:4242")).is_err());
        assert!(inherited_keeper(OsStr::new(&format!("{write_end}:1"))).is_err());
        assert!(inherited_keeper(OsStr::new("x:y")).is_err());
        let file = std::fs::File::open("/dev/null").expect("file");
        let not_pipe = format!("{}:{keeper}", file.as_raw_fd());
        assert!(inherited_keeper(OsStr::new(&not_pipe)).is_err());
        close_fd(read_end);
        close_fd(write_end);
    }

    /// A live process in another coalition is never adopted as the keeper:
    /// a real keeper shares its server's coalition by construction. Needs a
    /// GUI login session for a job in a coalition of its own.
    #[test]
    fn a_replacement_refuses_a_keeper_in_another_coalition() {
        let Some(uid) = gui_domain_available() else {
            eprintln!("skipping: no gui launchd domain for this test process");
            return;
        };
        let job = HelperJob::start(uid, "children", true);
        let server = job.pids[0];
        assert!(server > 1, "the helper started a server: {:?}", job.pids);
        let foreign = try_coalition(server);
        let own = try_coalition(std::process::id() as libc::pid_t);
        let mut fds = [0; 2];
        // SAFETY: pipe writes two descriptors.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let [read_end, write_end] = fds;
        let adopted = inherited_keeper(OsStr::new(&format!("{write_end}:{server}")));
        let server_alive = alive(server);
        close_fd(read_end);
        close_fd(write_end);
        job.stop();
        assert!(
            server_alive && foreign.is_some() && own.is_some() && foreign != own,
            "the stand-in keeper was alive in another coalition: {foreign:?} vs {own:?}"
        );
        assert!(adopted.is_err(), "a keeper in another coalition is refused");
    }
}
