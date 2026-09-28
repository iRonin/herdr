//! Sound notifications for agent state changes.
//!
//! Embeds mp3 files in the binary and plays them via system audio tools.
//! Uses afplay (macOS), Windows MediaPlayer, or decoder-capable Linux audio
//! players — no Rust audio dependencies.

use std::io::Write;
#[cfg(not(windows))]
use std::io::{Read, Result as IoResult};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(not(windows))]
use std::time::{Duration, Instant};

use tracing::{debug, warn};

const DISABLE_SOUND_ENV: &str = "HERDR_DISABLE_SOUND";
#[cfg(any(windows, test))]
const WINDOWS_SOUND_PATH_ENV: &str = "HERDR_SOUND_PATH";
#[cfg(not(windows))]
const AUDIO_PLAYER_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(not(windows))]
const AUDIO_PLAYER_POLL_INTERVAL: Duration = Duration::from_millis(25);
const SOUND_LOCK_FILE_NAME: &str = "sound.lock";

/// Set while this process plays a sound; see `start_playback`.
static SOUND_PLAYING: AtomicBool = AtomicBool::new(false);
static SOUND_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static SOUND_DONE: &[u8] = include_bytes!("../assets/sounds/done.mp3");
static SOUND_REQUEST: &[u8] = include_bytes!("../assets/sounds/request.mp3");

/// Which notification sound to play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// Agent finished work (transitioned to Idle).
    Done,
    /// Agent needs input (transitioned to Blocked).
    Request,
}

/// Play a notification sound in a background thread.
/// Silently does nothing if no audio player is available, or if another
/// herdr sound is still playing.
pub fn play(sound: Sound, config: &crate::config::SoundConfig) {
    if sound_playback_disabled_by_env() {
        return;
    }

    start_playback(
        sound,
        config.path_for(sound),
        &SOUND_PLAYING,
        sound_lock_path(),
        run_player,
    );
}

/// Starts a playback thread unless a herdr sound is already playing, in this
/// process or in another herdr process that shares the state directory. A
/// request made while a sound plays is dropped, not queued.
///
/// Every player opens its own stream to the system audio service, and on macOS
/// about fifteen overlapping players can leave coreaudiod spinning until it is
/// restarted. `playing` limits this process to one playback thread, and an
/// exclusive lock on `lock_path` limits every herdr process to one player.
/// The lock is released when playback ends, fails or times out, and by the
/// operating system when the process exits. If the lock file cannot be used,
/// the sound still plays under the in-process limit alone.
fn start_playback<P>(
    sound: Sound,
    custom_path: Option<PathBuf>,
    playing: &'static AtomicBool,
    lock_path: PathBuf,
    player: P,
) -> Option<std::thread::JoinHandle<()>>
where
    P: Fn(&Path) -> Result<Output, String> + Send + 'static,
{
    let Some(slot) = PlayingSlot::claim(playing) else {
        debug!(sound = ?sound, "skipping sound while another herdr sound plays");
        return None;
    };
    Some(std::thread::spawn(move || {
        let _slot = slot;
        // Declared after `_slot`, so it is dropped first: the lock is released
        // before this process accepts another sound.
        let _lock = match lock_sound_file(&lock_path) {
            Ok(Some(lock)) => Some(lock),
            Ok(None) => {
                debug!(sound = ?sound, "skipping sound while another herdr process plays one");
                return;
            }
            Err(err) => {
                warn!(path = %lock_path.display(), err = %err, "sound lock unavailable, playing without the cross-process limit");
                None
            }
        };

        if let Some(path) = custom_path {
            match play_file(&path, &player) {
                Ok(()) => return,
                Err(err) => {
                    warn!(path = %path.display(), sound = ?sound, err = %err, "custom sound playback failed, falling back to built-in sound")
                }
            }
        }

        let data = match sound {
            Sound::Done => SOUND_DONE,
            Sound::Request => SOUND_REQUEST,
        };

        if let Err(err) = play_bytes(data, &player) {
            warn!(sound = ?sound, err = %err, "sound playback failed");
        }
    }))
}

fn sound_lock_path() -> PathBuf {
    crate::config::state_dir().join(SOUND_LOCK_FILE_NAME)
}

/// Marks this process as playing a sound until dropped.
struct PlayingSlot(&'static AtomicBool);

impl PlayingSlot {
    fn claim(playing: &'static AtomicBool) -> Option<Self> {
        // Build the slot only on success: dropping one clears the flag, so a
        // slot built for a failed claim would release the running sound's.
        playing
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self(playing))
    }
}

impl Drop for PlayingSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Holds the cross-process sound lock until dropped.
struct SoundLock(std::fs::File);

impl Drop for SoundLock {
    fn drop(&mut self) {
        // Unlock before closing. A child process that another thread is
        // spawning shares this open file until it execs, and closing our
        // descriptor alone would leave the lock held until then.
        let _ = self.0.unlock();
    }
}

/// Returns the held lock, or `None` while another process holds it. The file
/// stays in place: removing it could let two processes lock different files
/// at the same path.
fn lock_sound_file(path: &Path) -> std::io::Result<Option<SoundLock>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(SoundLock(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(err)) => Err(err),
    }
}

fn sound_playback_disabled_by_env() -> bool {
    std::env::var_os(DISABLE_SOUND_ENV).is_some() || std::env::var_os("NEXTEST").is_some()
}

fn play_file(path: &Path, player: &dyn Fn(&Path) -> Result<Output, String>) -> Result<(), String> {
    match player(path) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(playback_error(&output)),
        Err(err) => Err(err),
    }
}

fn play_bytes(data: &[u8], player: &dyn Fn(&Path) -> Result<Output, String>) -> Result<(), String> {
    // Write to a temp file because the supported audio players need a file path.
    let tmp = temp_sound_path();
    let mut file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    file.write_all(data).map_err(|e| e.to_string())?;
    drop(file);

    let result = player(&tmp);

    let _ = std::fs::remove_file(&tmp);

    match result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(playback_error(&output)),
        Err(e) => Err(e),
    }
}

fn playback_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("player exited with {}", output.status)
    } else {
        format!("player exited with {}: {stderr}", output.status)
    }
}

fn temp_sound_path() -> PathBuf {
    let id = SOUND_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("herdr-sound-{}-{id}.mp3", std::process::id()))
}

#[cfg(windows)]
fn run_player(path: &Path) -> Result<Output, String> {
    run_windows_player(path)
}

#[cfg(target_os = "macos")]
fn run_player(path: &Path) -> Result<Output, String> {
    // Bounded like the Linux players: with one sound at a time, an afplay
    // that never exits would otherwise keep every later sound from playing.
    AudioPlayer {
        program: "afplay",
        args: &[],
    }
    .output(path)
    .map_err(|e| format!("afplay failed: {e}"))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn run_player(path: &Path) -> Result<Output, String> {
    run_linux_player(path)
}

#[cfg(any(windows, test))]
fn windows_media_player_script() -> &'static str {
    r#"
$ErrorActionPreference = 'Stop'
$Path = [Environment]::GetEnvironmentVariable('HERDR_SOUND_PATH', 'Process')
if ([string]::IsNullOrWhiteSpace($Path)) { throw 'HERDR_SOUND_PATH is not set' }
Add-Type -AssemblyName PresentationCore
Add-Type -AssemblyName WindowsBase
$resolved = (Resolve-Path -LiteralPath $Path).ProviderPath
$script:player = [System.Windows.Media.MediaPlayer]::new()
$script:frame = [System.Windows.Threading.DispatcherFrame]::new()
$script:timer = [System.Windows.Threading.DispatcherTimer]::new()
$script:timer.Interval = [TimeSpan]::FromSeconds(15)
$script:failed = $null
$script:timedOut = $false
$script:player.add_MediaOpened({ $script:player.Play() })
$script:player.add_MediaEnded({ $script:frame.Continue = $false })
$script:player.add_MediaFailed({
    param($sender, $eventArgs)
    $script:failed = $eventArgs.ErrorException
    $script:frame.Continue = $false
})
$script:timer.add_Tick({
    $script:timedOut = $true
    $script:frame.Continue = $false
})
try {
    $script:player.Open([Uri]::new($resolved))
    $script:timer.Start()
    [System.Windows.Threading.Dispatcher]::PushFrame($script:frame)
} finally {
    $script:timer.Stop()
    $script:player.Close()
}
if ($script:failed) { throw "sound media failed: $($script:failed.Message)" }
if ($script:timedOut) { throw 'sound playback timed out' }
"#
}

/// Builds the same player script with a shorter timer for tests. Production
/// keeps the fixed 15 second bound in `windows_media_player_script`.
#[cfg(test)]
fn windows_media_player_script_with_timeout(timeout_seconds: u64) -> String {
    windows_media_player_script().replace(
        "FromSeconds(15)",
        &format!("FromSeconds({timeout_seconds})"),
    )
}

#[cfg(any(windows, test))]
fn windows_player_command(path: &Path, script: &str) -> Command {
    let mut command = crate::noninteractive_process::command("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .env(WINDOWS_SOUND_PATH_ENV, path);
    command
}

#[cfg(windows)]
fn run_windows_player(path: &Path) -> Result<Output, String> {
    windows_player_command(path, windows_media_player_script())
        .output()
        .map_err(|e| format!("Windows MediaPlayer playback failed: {e}"))
}

#[cfg(not(windows))]
#[derive(Debug, Clone, Copy)]
struct AudioPlayer {
    program: &'static str,
    args: &'static [&'static str],
}

#[cfg(not(windows))]
impl AudioPlayer {
    fn output(self, path: &Path) -> std::io::Result<Output> {
        self.output_with_timeout(path, AUDIO_PLAYER_TIMEOUT)
    }

    fn output_with_timeout(self, path: &Path, timeout: Duration) -> std::io::Result<Output> {
        let mut child = Command::new(self.program)
            .args(self.args)
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let Some(stdout) = child.stdout.take() else {
            terminate_and_reap(&mut child)?;
            return Err(std::io::Error::other("audio player stdout was not piped"));
        };
        let Some(stderr) = child.stderr.take() else {
            terminate_and_reap(&mut child)?;
            return Err(std::io::Error::other("audio player stderr was not piped"));
        };
        let stdout_reader = read_output(stdout);
        let stderr_reader = read_output(stderr);
        let deadline = Instant::now() + timeout;

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let (stdout, stderr) = finish_output(stdout_reader, stderr_reader)?;
                    return Ok(Output {
                        status,
                        stdout,
                        stderr,
                    });
                }
                Ok(None) => {}
                Err(wait_err) => {
                    let cleanup_result = terminate_and_reap(&mut child);
                    let _ = finish_output(stdout_reader, stderr_reader);
                    cleanup_result?;
                    return Err(wait_err);
                }
            }

            let now = Instant::now();
            if now >= deadline {
                let cleanup_result = terminate_and_reap(&mut child);
                let _ = finish_output(stdout_reader, stderr_reader);
                cleanup_result?;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("{} playback timed out after {timeout:?}", self.program),
                ));
            }

            std::thread::sleep((deadline - now).min(AUDIO_PLAYER_POLL_INTERVAL));
        }
    }
}

#[cfg(not(windows))]
fn read_output<R>(mut reader: R) -> std::thread::JoinHandle<IoResult<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut output = Vec::new();
        reader.read_to_end(&mut output)?;
        Ok(output)
    })
}

#[cfg(not(windows))]
fn finish_output(
    stdout_reader: std::thread::JoinHandle<IoResult<Vec<u8>>>,
    stderr_reader: std::thread::JoinHandle<IoResult<Vec<u8>>>,
) -> IoResult<(Vec<u8>, Vec<u8>)> {
    let stdout = stdout_reader
        .join()
        .map_err(|_| std::io::Error::other("audio player stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| std::io::Error::other("audio player stderr reader panicked"))??;
    Ok((stdout, stderr))
}

#[cfg(not(windows))]
fn terminate_and_reap(child: &mut std::process::Child) -> std::io::Result<()> {
    if let Err(kill_err) = child.kill() {
        if child.try_wait()?.is_none() {
            return Err(kill_err);
        }
    }
    child.wait().map(|_| ())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn linux_audio_players() -> &'static [AudioPlayer] {
    // Do not add bare aplay here. It does not decode MP3 and plays MP3 bytes as raw PCM.
    &[
        AudioPlayer {
            program: "paplay",
            args: &[],
        },
        AudioPlayer {
            program: "pw-play",
            args: &[],
        },
        AudioPlayer {
            program: "ffplay",
            args: &["-nodisp", "-autoexit", "-loglevel", "quiet"],
        },
        AudioPlayer {
            program: "mpg123",
            args: &["-q"],
        },
        AudioPlayer {
            program: "mpv",
            args: &["--no-video", "--really-quiet"],
        },
    ]
}

#[cfg(not(any(windows, target_os = "macos")))]
fn run_linux_player(path: &Path) -> Result<Output, String> {
    let mut errors = Vec::new();

    for player in linux_audio_players() {
        match player.output(path) {
            Ok(output) if output.status.success() => return Ok(output),
            Ok(output) => errors.push(player_error(*player, &output)),
            Err(err) => errors.push(format!("{} failed: {err}", player.program)),
        }
    }

    Err(format!(
        "no mp3-capable audio player available: {}",
        errors.join("; ")
    ))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn player_error(player: AudioPlayer, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();

    if stderr.is_empty() {
        format!("{} exited with {}", player.program, output.status)
    } else {
        format!("{} exited with {}: {stderr}", player.program, output.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    #[test]
    fn temp_sound_paths_are_unique() {
        assert_ne!(temp_sound_path(), temp_sound_path());
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn linux_audio_players_are_mp3_capable() {
        let programs: Vec<&str> = linux_audio_players()
            .iter()
            .map(|player| player.program)
            .collect();

        assert_eq!(programs, ["paplay", "pw-play", "ffplay", "mpg123", "mpv"]);
        assert!(!programs.contains(&"aplay"));
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn linux_audio_player_does_not_wait_forever() {
        let pid_path = temp_sound_path().with_extension("pid");
        let player = AudioPlayer {
            program: "sh",
            args: &[
                "-c",
                "printf '%s' \"$$\" > \"$1\"; exec sleep 2",
                "herdr-sound-timeout-test",
            ],
        };
        let result = player.output_with_timeout(&pid_path, Duration::from_millis(100));
        let pid = std::fs::read_to_string(&pid_path)
            .expect("hanging test player should record its process ID");
        let _ = std::fs::remove_file(pid_path);

        let err = result.expect_err("hanging audio player should time out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        let status = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("test should inspect the timed-out player PID");
        assert!(
            !status.success(),
            "timed-out audio player should be terminated and reaped"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_audio_player_preserves_completed_output() {
        let player = AudioPlayer {
            program: "sh",
            args: &[
                "-c",
                "i=0; while [ \"$i\" -lt 8192 ]; do printf 0123456789abcdef; i=$((i + 1)); done; i=0; while [ \"$i\" -lt 8192 ]; do printf fedcba9876543210; i=$((i + 1)); done >&2; exit 7",
                "herdr-sound-output-test",
            ],
        };

        let output = player
            .output_with_timeout(Path::new("unused.mp3"), Duration::from_secs(5))
            .expect("completed audio player should return its output");

        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout.len(), 131_072);
        assert_eq!(output.stderr.len(), 131_072);
        assert!(output.stdout.starts_with(b"0123456789abcdef"));
        assert!(output.stderr.starts_with(b"fedcba9876543210"));
    }

    #[test]
    fn windows_media_player_uses_process_environment_and_dispatcher() {
        let script = windows_media_player_script();
        let path = Path::new(r"C:\sound dir\döne.mp3");
        let command = windows_player_command(path, script);
        let env_path = command.get_envs().find_map(|(key, value)| {
            (key == std::ffi::OsStr::new(WINDOWS_SOUND_PATH_ENV))
                .then_some(value)
                .flatten()
        });

        assert!(script.contains("GetEnvironmentVariable('HERDR_SOUND_PATH', 'Process')"));
        assert!(!script.contains("param([string]$Path)"));
        assert!(script.contains("Resolve-Path -LiteralPath $Path"));
        assert!(script.contains("Dispatcher]::PushFrame"));
        assert!(script.contains("add_MediaEnded"));
        assert!(script.contains("add_MediaFailed"));
        assert!(script.contains("FromSeconds(15)"));
        assert!(windows_media_player_script_with_timeout(2).contains("FromSeconds(2)"));
        assert_eq!(env_path, Some(path.as_os_str()));
        assert!(!command.get_args().any(|arg| arg == path.as_os_str()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_media_player_reports_invalid_media() {
        let _lock = crate::integration::integration_env_lock();
        let path = temp_sound_path();
        std::fs::write(&path, b"not an mp3").unwrap();
        let script = windows_media_player_script_with_timeout(2);
        // Whether the host media stack raises MediaFailed before the playback
        // timeout is environment-dependent, so use a short test-only timer and
        // accept either terminal error instead of the production 15 second wait.
        let output = windows_player_command(&path, &script).output().unwrap();
        let _ = std::fs::remove_file(path);

        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("sound media failed") || stderr.contains("sound playback timed out"),
            "stderr should report why playback stopped: {stderr}"
        );
    }

    /// Counts fake players, and how many of them ran at the same time.
    #[derive(Default)]
    struct FakePlayers {
        running: AtomicUsize,
        most_at_once: AtomicUsize,
        started: AtomicUsize,
    }

    impl FakePlayers {
        fn enter(&self) {
            self.started.fetch_add(1, Ordering::SeqCst);
            let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_at_once.fetch_max(running, Ordering::SeqCst);
        }

        fn leave(&self) {
            self.running.fetch_sub(1, Ordering::SeqCst);
        }

        fn started(&self) -> usize {
            self.started.load(Ordering::SeqCst)
        }

        fn most_at_once(&self) -> usize {
            self.most_at_once.load(Ordering::SeqCst)
        }
    }

    /// A private directory for one test's lock file, removed when dropped.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "herdr-sound-test-{}-{}-{name}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create the test directory");
            Self(path)
        }

        fn lock_path(&self) -> PathBuf {
            self.0.join(SOUND_LOCK_FILE_NAME)
        }

        fn custom_sound(&self) -> PathBuf {
            self.0.join("custom.mp3")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Kills and reaps a helper process if the test ends early.
    struct KillOnDrop(std::process::Child);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A separate in-process flag per test, so parallel tests never share one.
    fn test_playing_flag() -> &'static AtomicBool {
        Box::leak(Box::new(AtomicBool::new(false)))
    }

    fn player_output(code: i32) -> Output {
        #[cfg(unix)]
        let status = std::os::unix::process::ExitStatusExt::from_raw(code << 8);
        #[cfg(windows)]
        let status = std::os::windows::process::ExitStatusExt::from_raw(code as u32);
        Output {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    /// Sends `requests` sound requests at the same moment. Each fake player
    /// runs for 300 ms; returns once every started playback has finished.
    fn fire_simultaneous_requests(
        requests: usize,
        lock_path: &Path,
        custom_sound: &Path,
    ) -> Arc<FakePlayers> {
        let playing = test_playing_flag();
        let players = Arc::new(FakePlayers::default());
        let barrier = Arc::new(Barrier::new(requests));
        let callers: Vec<_> = (0..requests)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let players = Arc::clone(&players);
                let lock_path = lock_path.to_path_buf();
                let custom_sound = custom_sound.to_path_buf();
                std::thread::spawn(move || {
                    barrier.wait();
                    start_playback(
                        Sound::Done,
                        Some(custom_sound),
                        playing,
                        lock_path,
                        move |_| {
                            players.enter();
                            std::thread::sleep(Duration::from_millis(300));
                            players.leave();
                            Ok(player_output(0))
                        },
                    )
                })
            })
            .collect();
        for caller in callers {
            if let Some(playback) = caller.join().expect("request thread") {
                playback.join().expect("playback thread");
            }
        }
        players
    }

    #[test]
    fn concurrent_sound_requests_run_one_player_at_a_time() {
        let dir = TestDir::new("concurrent");

        let players = fire_simultaneous_requests(16, &dir.lock_path(), &dir.custom_sound());

        assert!(players.started() >= 1, "one of the requests must play");
        assert_eq!(
            players.most_at_once(),
            1,
            "players running at the same time ({} started for 16 simultaneous requests)",
            players.started()
        );
    }

    #[test]
    fn concurrent_sound_requests_stay_one_at_a_time_when_the_lock_file_is_unusable() {
        let dir = TestDir::new("unusable-lock");
        let not_a_directory = dir.0.join("not-a-directory");
        std::fs::write(&not_a_directory, b"").expect("put a file where the lock directory goes");
        let lock_path = not_a_directory.join(SOUND_LOCK_FILE_NAME);
        assert!(
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)
                .is_err(),
            "control: the lock file must be impossible to create"
        );

        let players = fire_simultaneous_requests(16, &lock_path, &dir.custom_sound());

        assert!(
            players.started() >= 1,
            "sound must still play when the lock file is unusable"
        );
        assert_eq!(
            players.most_at_once(),
            1,
            "players running at the same time ({} started for 16 simultaneous requests)",
            players.started()
        );
    }

    const HOLDER_DIR_ENV: &str = "HERDR_SOUND_TEST_HOLDER_DIR";

    #[test]
    fn sound_playing_in_another_process_blocks_players_until_that_process_dies() {
        if let Some(dir) = std::env::var_os(HOLDER_DIR_ENV) {
            // Helper process: play a sound whose fake player never finishes and
            // leave a marker once it plays. The parent test kills this process.
            let dir = PathBuf::from(dir);
            let marker = dir.join("holder-playing");
            let playback = start_playback(
                Sound::Done,
                Some(dir.join("custom.mp3")),
                test_playing_flag(),
                dir.join(SOUND_LOCK_FILE_NAME),
                move |_| {
                    std::fs::write(&marker, b"playing").expect("write the playing marker");
                    std::thread::sleep(Duration::from_secs(60));
                    Ok(player_output(0))
                },
            )
            .expect("the helper process is not playing anything yet");
            let _ = playback.join();
            return;
        }

        let dir = TestDir::new("cross-process");
        let mut holder = KillOnDrop(
            std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "sound::tests::sound_playing_in_another_process_blocks_players_until_that_process_dies",
                    "--nocapture",
                ])
                .env(HOLDER_DIR_ENV, &dir.0)
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("start the helper process"),
        );
        let marker = dir.0.join("holder-playing");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !marker.exists() {
            if let Some(status) = holder.0.try_wait().expect("poll the helper process") {
                panic!("the helper process exited before playing: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "the helper process never started playing"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let playing = test_playing_flag();
        let players = Arc::new(FakePlayers::default());
        let request = || {
            let players = Arc::clone(&players);
            let playback = start_playback(
                Sound::Done,
                Some(dir.custom_sound()),
                playing,
                dir.lock_path(),
                move |_| {
                    players.enter();
                    players.leave();
                    Ok(player_output(0))
                },
            );
            if let Some(playback) = playback {
                playback.join().expect("playback thread");
            }
        };

        request();
        assert_eq!(
            players.started(),
            0,
            "a player started while another herdr process was playing"
        );

        // On Unix this is SIGKILL: no code in the helper runs after it, so only
        // the operating system can release the helper's lock.
        holder.0.kill().expect("kill the helper process");
        holder.0.wait().expect("reap the helper process");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            request();
            if players.started() > 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "no sound played after the playing process died"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn sound_lock_is_released_while_a_spawning_child_shares_the_lock_file() {
        let dir = TestDir::new("shared-lock-file");
        let lock = lock_sound_file(&dir.lock_path())
            .expect("open the lock file")
            .expect("nothing else holds the lock");
        // A child that another thread is spawning holds a copy of every open
        // file until it execs; a duplicated descriptor shares the file the same way.
        let shared = lock.0.try_clone().expect("duplicate the lock descriptor");

        drop(lock);

        assert!(
            lock_sound_file(&dir.lock_path())
                .expect("open the lock file")
                .is_some(),
            "the lock outlived its playback while another descriptor shared the file"
        );
        drop(shared);
    }

    #[derive(Debug, Clone, Copy)]
    enum Ending {
        Finished,
        ExitedWithError,
        FailedToStart,
        TimedOut,
    }

    /// Runs upstream's real timeout path: the player is killed and reaped.
    #[cfg(not(windows))]
    fn timed_out_player(path: &Path) -> Result<Output, String> {
        AudioPlayer {
            program: "sh",
            args: &["-c", "exec sleep 5", "herdr-sound-timeout-test"],
        }
        .output_with_timeout(path, Duration::from_millis(100))
        .map_err(|err| err.to_string())
    }

    /// The Windows player enforces its limit in PowerShell and reports an error.
    #[cfg(windows)]
    fn timed_out_player(_path: &Path) -> Result<Output, String> {
        Err("sound playback timed out".to_string())
    }

    #[test]
    fn sound_lock_is_released_after_success_error_and_timeout() {
        let dir = TestDir::new("release");
        let playing = test_playing_flag();
        for ending in [
            Ending::Finished,
            Ending::ExitedWithError,
            Ending::FailedToStart,
            Ending::TimedOut,
        ] {
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (finish_tx, finish_rx) = std::sync::mpsc::channel::<()>();
            let calls = Arc::new(AtomicUsize::new(0));
            let player_calls = Arc::clone(&calls);
            let playback = start_playback(
                Sound::Request,
                Some(dir.custom_sound()),
                playing,
                dir.lock_path(),
                move |path| {
                    player_calls.fetch_add(1, Ordering::SeqCst);
                    let _ = started_tx.send(());
                    // Keep playing until the test has made its second request.
                    let _ = finish_rx.recv();
                    match ending {
                        Ending::Finished => Ok(player_output(0)),
                        Ending::ExitedWithError => Ok(player_output(1)),
                        Ending::FailedToStart => Err("no audio player available".to_string()),
                        Ending::TimedOut => timed_out_player(path),
                    }
                },
            )
            .unwrap_or_else(|| panic!("{ending:?}: this process still counts as playing"));
            started_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("{ending:?}: the player did not start"));

            let second = Arc::new(FakePlayers::default());
            let second_players = Arc::clone(&second);
            let extra = start_playback(
                Sound::Done,
                Some(dir.custom_sound()),
                playing,
                dir.lock_path(),
                move |_| {
                    second_players.enter();
                    second_players.leave();
                    Ok(player_output(0))
                },
            );
            if let Some(extra) = extra {
                extra.join().expect("second playback thread");
            }
            assert_eq!(
                second.started(),
                0,
                "{ending:?}: a second player started while a sound was playing"
            );

            drop(finish_tx);
            playback.join().expect("playback thread");
            let expected_calls = if matches!(ending, Ending::Finished) {
                1
            } else {
                2
            };
            assert_eq!(
                calls.load(Ordering::SeqCst),
                expected_calls,
                "{ending:?}: a failed custom sound must fall back to the built-in sound once"
            );
        }

        let players = Arc::new(FakePlayers::default());
        let next = Arc::clone(&players);
        start_playback(
            Sound::Done,
            Some(dir.custom_sound()),
            playing,
            dir.lock_path(),
            move |_| {
                next.enter();
                next.leave();
                Ok(player_output(0))
            },
        )
        .expect("the in-process flag was released after the last ending")
        .join()
        .expect("playback thread");
        assert_eq!(
            players.started(),
            1,
            "no sound played after a timed-out sound"
        );
    }
}
