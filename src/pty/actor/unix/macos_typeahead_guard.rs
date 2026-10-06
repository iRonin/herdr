//! Keeps unread pane input from wrapping the macOS tty's input ring.
//!
//! XNU keeps a tty's unread input in 1024-byte rings that restart at their first slot
//! whenever they are empty (`putc()` in bsd/kern/tty_subr.c). When a program returns the tty
//! from raw to canonical mode (bash's readline does this before running every command), the
//! kernel re-feeds the unread input through its line discipline (`ttypend()` in
//! bsd/kern/tty.c), writing it back into the ring it is reading from and restarting at the
//! first slot after every line end. If the unread input wraps past the ring's end, the reads
//! catch up with those writes and the program receives the current line's first characters
//! repeated instead of the typed text.
//!
//! The re-input is harmless while the unread input does not wrap, so the guard holds pane
//! input only when it could: it keeps an upper bound on the slots used since the tty's input
//! queues were last known to be empty, writes freely while that bound stays under
//! `RING_BUDGET`, and otherwise reads the tty's modes and unread count from the master side
//! before writing more. Keys and small pastes never reach a check.

use std::{
    collections::VecDeque,
    os::fd::RawFd,
    time::{Duration, Instant},
};

/// Upper bound on the ring slots unread input may use: a margin under the 1024-slot ring.
const RING_BUDGET: usize = 1000;
/// How many written bytes are remembered to find the current line's start.
const TAIL: usize = 1024;
/// First re-check after the guard starts holding.
const RECHECK_MIN: Duration = Duration::from_millis(2);
/// Re-checks back off to this period while the unread count does not move.
const RECHECK_MAX: Duration = Duration::from_millis(32);
/// `_POSIX_VDISABLE` on macOS.
const VDISABLE: libc::cc_t = 0xff;

pub(super) struct TypeaheadGuard {
    /// False once the master turns out not to be a tty: there is no ring to protect.
    enabled: bool,
    /// Set while a live handoff drains the queued writes: the guard is off, as before it existed.
    pub(super) suspended: bool,
    /// Upper bound on the ring slots used since both input queues were last known to be
    /// empty. `None` when unknown.
    ring_bound: Option<usize>,
    /// Bytes at the front of the queued writes that may be written without another check.
    granted: usize,
    /// The last bytes written, to find where the current line starts.
    tail: VecDeque<u8>,
    /// True while `tail` holds every byte written since the queues were known to be empty.
    tail_from_start: bool,
    hold_until: Option<Instant>,
    recheck: Duration,
    last_unread: Option<usize>,
    /// Set when input is queued: a held signal character must be looked for again.
    pub(super) rescan_signals: bool,
    #[cfg(test)]
    pub(super) checks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl TypeaheadGuard {
    /// `fresh`: the pty was just opened, so nothing has entered its input queues yet. A pty
    /// received in a live handoff may hold input already, so its first write checks.
    pub(super) fn new(fresh: bool) -> Self {
        Self {
            enabled: true,
            suspended: false,
            ring_bound: fresh.then_some(0),
            granted: 0,
            tail: VecDeque::with_capacity(TAIL),
            tail_from_start: fresh,
            hold_until: None,
            recheck: RECHECK_MIN,
            last_unread: None,
            rescan_signals: false,
            #[cfg(test)]
            checks: Default::default(),
        }
    }

    /// When the guard will look again, while it is holding input.
    pub(super) fn hold_deadline(&self) -> Option<Instant> {
        self.hold_until.filter(|_| self.enabled && !self.suspended)
    }

    /// Ends a hold early, for example after a held signal character flushed the tty.
    pub(super) fn release_hold(&mut self) {
        self.hold_until = None;
        self.recheck = RECHECK_MIN;
        self.last_unread = None;
    }

    /// The queued bytes changed other than by appending or writing: forget the grant.
    pub(super) fn reset_grant(&mut self) {
        self.granted = 0;
    }

    /// How many of the queued bytes (`pending`, from the front) may be written now, or `None`
    /// to hold them. Never `Some(0)`.
    pub(super) fn allowance(
        &mut self,
        now: Instant,
        fd: RawFd,
        pending: impl Iterator<Item = u8> + Clone,
    ) -> Option<usize> {
        if !self.enabled || self.suspended {
            return Some(usize::MAX);
        }
        if self.granted > 0 {
            return Some(self.granted);
        }
        if let Some(bound) = self.ring_bound {
            let fits = fit(pending.clone(), RING_BUDGET.saturating_sub(bound));
            if fits > 0 {
                self.release_hold();
                self.granted = fits;
                return Some(fits);
            }
        }
        if self.hold_until.is_some_and(|until| now < until) {
            return None;
        }
        self.check(now, fd, pending)
    }

    /// Records bytes the tty accepted.
    pub(super) fn record_written(&mut self, bytes: &[u8]) {
        self.granted = self.granted.saturating_sub(bytes.len());
        if let Some(bound) = self.ring_bound.as_mut() {
            *bound = bound.saturating_add(bytes.iter().map(|byte| weight(*byte)).sum());
        }
        for byte in bytes {
            if self.tail.len() == TAIL {
                self.tail.pop_front();
                self.tail_from_start = false;
            }
            self.tail.push_back(*byte);
        }
    }

    fn check(
        &mut self,
        now: Instant,
        fd: RawFd,
        pending: impl Iterator<Item = u8> + Clone,
    ) -> Option<usize> {
        #[cfg(test)]
        self.checks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let observed = (|| {
            let before = modes(fd)?;
            let unread = unread_input(fd)?;
            let after = modes(fd)?;
            Ok::<_, std::io::Error>((before, unread, after))
        })();
        let (before, unread, after) = match observed {
            Ok(observed) => observed,
            Err(err) => {
                if err.raw_os_error() == Some(libc::ENOTTY) {
                    // Not a tty: there is no ring to protect.
                    self.enabled = false;
                    return Some(usize::MAX);
                }
                // The tty cannot be observed: write as before the guard existed.
                return self.grant(pending.count(), None);
            }
        };
        if !same_modes(&before, &after) {
            return self.hold(now, unread);
        }
        if after.c_lflag & libc::ICANON != 0 {
            // Canonical: the reader sees complete lines only, so 0 means the canonical queue
            // is empty. A partial line may still sit in the raw queue, invisible.
            if unread != 0 {
                return self.hold(now, unread);
            }
            let line_ends = LineEnds::new(&after);
            let partial = self.partial_line(&line_ends);
            let within_budget = partial
                .map(|partial| fit(pending.clone(), RING_BUDGET.saturating_sub(partial)))
                .unwrap_or(0);
            // The current line is always allowed through its end: the raw queue restarts at
            // the ring's first slot after every line end and the kernel caps it at MAX_INPUT,
            // and one line moved into the empty canonical queue cannot wrap either. Without
            // this, a long unfinished line would hold its own Backspace, Ctrl-U and Enter.
            let current_line = line_ends.through_next_possible_end(pending);
            return self.grant(within_budget.max(current_line), partial);
        }
        if after.c_cc[libc::VMIN] > 1 && after.c_cc[libc::VTIME] == 0 {
            // FIONREAD reads 0 below VMIN, so emptiness cannot be seen; holding could starve
            // a reader waiting for VMIN bytes. Write as before the guard existed.
            return self.grant(pending.count(), None);
        }
        if unread != 0 {
            return self.hold(now, unread);
        }
        // Raw, and both queues are empty: the next byte enters the ring's first slot.
        self.grant(fit(pending, RING_BUDGET), Some(0))
    }

    fn grant(&mut self, bytes: usize, ring_bound: Option<usize>) -> Option<usize> {
        self.ring_bound = ring_bound;
        self.release_hold();
        self.granted = bytes.max(1);
        Some(self.granted)
    }

    fn hold(&mut self, now: Instant, unread: usize) -> Option<usize> {
        self.recheck = if self.last_unread == Some(unread) {
            (self.recheck * 2).min(RECHECK_MAX)
        } else {
            RECHECK_MIN
        };
        self.last_unread = Some(unread);
        self.hold_until = Some(now + self.recheck);
        self.granted = 0;
        None
    }

    /// An upper bound on the slots the current, unfinished line uses: the bytes written after
    /// the last certain line end. `None` when the tail holds no line end and is not complete.
    fn partial_line(&self, line_ends: &LineEnds) -> Option<usize> {
        let mut quoted = false;
        let mut since_end = 0;
        let mut found_end = false;
        for (index, byte) in self.tail.iter().enumerate() {
            let ends = line_ends.certainly_ends(*byte, &mut quoted);
            // Without the bytes before the tail, its first byte might have been quoted.
            if ends && (index > 0 || self.tail_from_start) {
                found_end = true;
                since_end = 0;
            } else {
                since_end += weight(*byte);
            }
        }
        (found_end || self.tail_from_start).then_some(since_end)
    }
}

/// Ring slots one input byte can take: 0xff takes two when the tty marks parity (PARMRK).
fn weight(byte: u8) -> usize {
    if byte == 0xff {
        2
    } else {
        1
    }
}

/// How many bytes of `bytes` fit in `budget` ring slots.
fn fit(bytes: impl Iterator<Item = u8>, budget: usize) -> usize {
    let mut used = 0;
    let mut count = 0;
    for byte in bytes {
        used += weight(byte);
        if used > budget {
            break;
        }
        count += 1;
    }
    count
}

fn modes(fd: RawFd) -> std::io::Result<libc::termios> {
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(termios)
}

fn unread_input(fd: RawFd) -> std::io::Result<usize> {
    let mut count: libc::c_int = 0;
    if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut count) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

/// The modes the guard's decision depends on. PENDIN is left out: FIONREAD clears it.
fn same_modes(a: &libc::termios, b: &libc::termios) -> bool {
    let lflags = libc::ICANON | libc::IEXTEN | libc::EXTPROC | libc::ISIG;
    let iflags = libc::ICRNL | libc::IGNCR | libc::INLCR | libc::ISTRIP | libc::IXON;
    a.c_lflag & lflags == b.c_lflag & lflags
        && a.c_iflag & iflags == b.c_iflag & iflags
        && a.c_cc == b.c_cc
}

/// Which input bytes end a line in canonical mode, after XNU's `ttyinput()`.
struct LineEnds {
    strip: bool,
    extproc: bool,
    lnext: Option<u8>,
    igncr: bool,
    icrnl: bool,
    inlcr: bool,
    /// VEOF, VEOL, and VEOL2 with IEXTEN.
    delimiters: [Option<u8>; 3],
    /// Characters the line discipline acts on before queueing them: never line ends.
    specials: [Option<u8>; 11],
}

impl LineEnds {
    fn new(termios: &libc::termios) -> Self {
        let cc = |index: usize| Some(termios.c_cc[index]).filter(|ch| *ch != VDISABLE);
        let iexten = termios.c_lflag & libc::IEXTEN != 0;
        Self {
            strip: termios.c_iflag & libc::ISTRIP != 0,
            extproc: termios.c_lflag & libc::EXTPROC != 0,
            lnext: cc(libc::VLNEXT).filter(|_| iexten),
            igncr: termios.c_iflag & libc::IGNCR != 0,
            icrnl: termios.c_iflag & libc::ICRNL != 0,
            inlcr: termios.c_iflag & libc::INLCR != 0,
            delimiters: [
                cc(libc::VEOF),
                cc(libc::VEOL),
                cc(libc::VEOL2).filter(|_| iexten),
            ],
            specials: [
                cc(libc::VINTR),
                cc(libc::VQUIT),
                cc(libc::VSUSP),
                cc(libc::VSTART),
                cc(libc::VSTOP),
                cc(libc::VDISCARD),
                cc(libc::VERASE),
                cc(libc::VKILL),
                cc(libc::VWERASE),
                cc(libc::VREPRINT),
                cc(libc::VSTATUS),
            ],
        }
    }

    /// True only when the kernel certainly treats `byte` as a line end. Errs towards false:
    /// the result bounds the current line from above.
    fn certainly_ends(&self, byte: u8, quoted: &mut bool) -> bool {
        let mut ch = if self.strip { byte & 0x7f } else { byte };
        if !self.extproc {
            if std::mem::take(quoted) {
                return false;
            }
            if Some(ch) == self.lnext {
                *quoted = true;
                return false;
            }
            if self.specials.contains(&Some(ch)) {
                return false;
            }
            if ch == b'\r' {
                if self.igncr {
                    return false;
                }
                if self.icrnl {
                    ch = b'\n';
                }
            } else if ch == b'\n' && self.inlcr {
                ch = b'\r';
            }
        }
        ch == b'\n' || self.delimiters.contains(&Some(ch))
    }

    /// True when the kernel might treat `byte` as a line end. Errs towards true (a quoted line
    /// end counts too): the current line's allowance must end no later than the kernel's.
    fn possibly_ends(&self, byte: u8) -> bool {
        let ch = if self.strip { byte & 0x7f } else { byte };
        ch == b'\n' || ch == b'\r' || self.delimiters.contains(&Some(ch))
    }

    /// The bytes of `pending` up to and including the first byte that might end the current
    /// line; all of them if none might.
    fn through_next_possible_end(&self, pending: impl Iterator<Item = u8>) -> usize {
        let mut count = 0;
        for byte in pending {
            count += 1;
            if self.possibly_ends(byte) {
                break;
            }
        }
        count
    }
}
