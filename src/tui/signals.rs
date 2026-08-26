//! Restoring the terminal when the TUI is killed rather than quit
//! (bd-8lz.4.7).
//!
//! Quitting, Ctrl-C (raw mode delivers it as a key, not a signal) and a panic
//! all leave through [`super::restore_with`]. A signal does not: without a
//! handler the process dies with raw mode on, the alternate screen entered,
//! the cursor hidden and ANY-MOTION mouse tracking armed — after which mouse
//! movement is injected into the user's shell as literal text.
//!
//! [`HANDLED`] carries which signals are covered and which are knowingly not.
//!
//! Everything below the [`install`] call runs *inside a signal handler*, so it
//! is held to async-signal-safety: no allocation, no locks, no `std::io`, no
//! formatting. What is left is `write`, `tcsetattr`, `raise` and atomic loads
//! — all on POSIX's async-signal-safe list. The escape bytes are `const` and
//! the terminal-state flags are already `AtomicBool`s, which is what makes
//! that possible.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use super::{ALT_ON, MOUSE_ON, RAW_ON};

/// The signals worth handling: every catchable way the process is *ended*
/// from outside while the terminal is armed — `kill`, `kill -INT`,
/// `kill -QUIT`, and the hangup a closed terminal window sends.
///
/// What is deliberately not here:
/// - SIGKILL and SIGSTOP cannot be caught at all, so a `kill -9` still leaves
///   the terminal dirty — a property of the signal, not a gap here.
/// - SIGTSTP is catchable but *resumable*: handling it would mean restoring on
///   the way down and re-arming from a SIGCONT handler on the way back, and
///   half of that pair is worse than neither. Out of scope for this bead; an
///   external `kill -TSTP` still suspends with the alternate screen up.
///
/// Raw mode clears `ISIG`, so an interactive Ctrl-C / Ctrl-\ / Ctrl-Z reaches
/// the TUI as a key and never gets here — these cover the external sender.
const HANDLED: [libc::c_int; 4] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];

/// DECTCEM on. First, and whatever else was armed: ratatui hides the cursor on
/// every draw and leaving the alternate screen does not bring it back.
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
/// Exactly what `crossterm::event::DisableMouseCapture` writes; asserted
/// against it in the tests, since drift here is invisible until a terminal is
/// left with mouse reporting on.
const MOUSE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
/// `crossterm::terminal::LeaveAlternateScreen`.
const ALT_OFF: &[u8] = b"\x1b[?1049l";

/// What a signal-time restore has to undo, in the order it has to undo it.
/// Pure, so the bytes can be checked against crossterm's own without a
/// terminal; empty slices are the steps that were never armed.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Plan {
    pub bytes: [&'static [u8]; 3],
    /// Whether raw mode is still on and the saved termios has to go back.
    pub termios: bool,
}

pub(super) fn plan(mouse: bool, alt: bool, raw: bool) -> Plan {
    if !(mouse || alt || raw) {
        return Plan {
            bytes: [b"", b"", b""],
            termios: false,
        };
    }
    Plan {
        bytes: [
            SHOW_CURSOR,
            if mouse { MOUSE_OFF } else { b"" },
            if alt { ALT_OFF } else { b"" },
        ],
        termios: raw,
    }
}

/// Read the terminal state and clear it in one step, so the plan built from it
/// covers everything still armed and a second pass through here writes nothing.
///
/// This is not an agreement with [`super::restore_with`] that a step is undone
/// only once — that guard clears each flag *after* its own undo precisely so a
/// signal in the middle still sees the rest as armed, and both may therefore
/// undo the same step. Every undo is idempotent, and the handler never returns
/// to let the guard resume, so the overlap costs nothing; see `restore_with`'s
/// own doc for why that ordering is the safe one.
pub(super) fn take_flags() -> (bool, bool, bool) {
    (
        MOUSE_ON.swap(false, Ordering::SeqCst),
        ALT_ON.swap(false, Ordering::SeqCst),
        RAW_ON.swap(false, Ordering::SeqCst),
    )
}

// ------------------------------------------------------------ saved termios

/// The termios the terminal had before raw mode, and the descriptor it belongs
/// to. Written once, on the main thread, before raw mode is enabled; read only
/// from the handler, and only once `SAVED` says the write finished.
struct Slot(UnsafeCell<MaybeUninit<libc::termios>>);

// Safety: the only writer is `save_termios` on the main thread before `SAVED`
// is set with `Release`; the only reader is the handler, after an `Acquire`
// load of `SAVED`.
unsafe impl Sync for Slot {}

static TERMIOS: Slot = Slot(UnsafeCell::new(MaybeUninit::uninit()));
static SAVED: AtomicBool = AtomicBool::new(false);
static TTY: AtomicI32 = AtomicI32::new(-1);

/// Remember the terminal's line discipline so a handler can put it back.
/// Called on the way into raw mode, from the main thread.
///
/// The descriptor is stdin when stdin is a terminal and `/dev/tty` otherwise —
/// the same choice crossterm makes, so this restores what crossterm changed.
pub(super) fn save_termios() {
    if SAVED.load(Ordering::Acquire) {
        return;
    }
    // Safety: plain libc calls on descriptors this process owns.
    let fd = unsafe {
        if libc::isatty(libc::STDIN_FILENO) == 1 {
            libc::STDIN_FILENO
        } else {
            libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR)
        }
    };
    if fd < 0 {
        return;
    }
    let mut termios = MaybeUninit::<libc::termios>::uninit();
    // Safety: `fd` is open and `termios` is writable for a whole struct.
    if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } != 0 {
        return;
    }
    TTY.store(fd, Ordering::Relaxed);
    // Safety: sole writer, and no reader can be looking yet — the handler
    // reads only after the `Release` store below.
    unsafe { TERMIOS.0.get().write(termios) };
    SAVED.store(true, Ordering::Release);
}

/// Async-signal-safe: `tcsetattr` is on POSIX's list, and the struct it reads
/// was fully written before `SAVED` was set.
fn restore_termios() {
    if !SAVED.load(Ordering::Acquire) {
        return;
    }
    let fd = TTY.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    // Safety: `TERMIOS` is initialised (that is what `SAVED` means) and is
    // only read here.
    unsafe {
        let saved = (*TERMIOS.0.get()).as_ptr();
        libc::tcsetattr(fd, libc::TCSANOW, saved);
    }
}

// ----------------------------------------------------------------- handling

/// Where the escapes go. Stdout, except while a test points it at a pipe:
/// `libc::write` bypasses libtest's stdout capture, so without this a plain
/// `cargo test` fires DisableMouseCapture and LeaveAlternateScreen at whatever
/// terminal the developer happens to be sitting in. An atomic load is
/// async-signal-safe, which a `#[cfg(test)]` lock would not be.
static OUT_FD: AtomicI32 = AtomicI32::new(libc::STDOUT_FILENO);

/// Write every byte, or give up. Async-signal-safe: `write` is on the list,
/// and `EINTR` is the one failure worth retrying.
fn write_all(bytes: &[u8]) {
    let fd = OUT_FD.load(Ordering::Relaxed);
    let mut at = 0;
    while at < bytes.len() {
        // Safety: writing `bytes` to a descriptor we do not own is still just
        // a write of a valid pointer and length.
        let n = unsafe { libc::write(fd, bytes[at..].as_ptr().cast(), bytes.len() - at) };
        if n > 0 {
            at += n as usize;
            continue;
        }
        // Reads errno and nothing else — no allocation, no locking.
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return;
    }
}

/// `errno`'s address for the calling thread, or null where libc's accessor is
/// not one we know. Both CI targets are covered; anywhere else the handler
/// simply skips the save.
fn errno_slot() -> *mut libc::c_int {
    // Safety: libc's own accessor, valid for the calling thread, and on
    // POSIX's async-signal-safe list.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::__errno_location()
    }
    #[cfg(target_os = "macos")]
    unsafe {
        libc::__error()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    std::ptr::null_mut()
}

/// Run `f` with `errno` saved and put back: a handler that returns to an
/// interrupted call owes it the value it had, and every syscall in here
/// (`write`, `tcsetattr`, `signal`, `raise`) may set it even on success.
///
/// Async-signal-safe: a statically dispatched call, one `libc` accessor and
/// two loads through a thread-local pointer. Nests harmlessly, which is what
/// lets both the whole handler and [`restore_from_signal`] on its own hold the
/// guarantee.
fn preserving_errno<T>(f: impl FnOnce() -> T) -> T {
    let slot = errno_slot();
    // Safety: `slot` is either null (checked) or libc's own thread-local.
    let saved = if slot.is_null() { 0 } else { unsafe { *slot } };
    let out = f();
    if !slot.is_null() {
        // Safety: as above.
        unsafe { *slot = saved };
    }
    out
}

/// Everything the handler does except dying: separated so it can be tested in
/// process, where re-raising would take the test runner with it.
///
/// Errno-neutral on its own, so the test can pin it without a signal.
pub(super) fn restore_from_signal() {
    preserving_errno(|| {
        let (mouse, alt, raw) = take_flags();
        let plan = plan(mouse, alt, raw);
        for bytes in plan.bytes {
            if !bytes.is_empty() {
                write_all(bytes);
            }
        }
        if plan.termios {
            restore_termios();
        }
    });
}

extern "C" fn handler(sig: libc::c_int) {
    // Around the *whole* body, not just the restore: `signal` and `raise` are
    // allowed to set `errno` too, so anything narrower would leave the promise
    // above only half kept. Free today — the re-raise never comes back — and
    // still true the moment anyone handles a signal that does return.
    preserving_errno(|| {
        restore_from_signal();
        // Put the default disposition back and re-raise, so the process dies
        // the way the sender asked it to rather than carrying on with a
        // half-torn-down terminal. The signal is blocked for the length of its
        // own handler, so the re-raised one lands the moment this returns.
        //
        // Explicitly rather than through `SA_RESETHAND`: macOS does not report
        // that flag back through `sigaction`, so nothing could test it. Both
        // `signal` and `raise` are async-signal-safe.
        // Safety: two libc calls with no state of ours involved.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    });
}

/// Run `f` with the handler's output pointed at a pipe, handing back what it
/// wrote. `OUT_FD` is process-global, so callers hold [`super::lifecycle_lock`]
/// — the same lock every flag-touching test already takes.
#[cfg(test)]
pub(super) fn capturing_output<T>(f: impl FnOnce() -> T) -> (T, Vec<u8>) {
    use std::io::Read;
    use std::os::fd::FromRawFd;

    /// Puts `OUT_FD` back and closes whatever is still ours, even if `f`
    /// unwinds. Without it a panicking test would leave the handler pointed at
    /// a closed pipe for every test after it — `lifecycle_lock` is
    /// poison-tolerant, so they keep running and would write nowhere.
    struct Restore {
        previous: libc::c_int,
        fds: [libc::c_int; 2],
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            OUT_FD.store(self.previous, Ordering::SeqCst);
            for fd in self.fds {
                if fd >= 0 {
                    // Safety: ours, and taken off the guard once handed over.
                    unsafe { libc::close(fd) };
                }
            }
        }
    }

    let mut fds = [0 as libc::c_int; 2];
    // Safety: `pipe` fills two descriptors into an array we own.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    let previous = OUT_FD.swap(fds[1], Ordering::SeqCst);
    let mut guard = Restore { previous, fds };

    let out = f();

    // Explicitly, not just via the guard: nothing should point at the pipe
    // while the read below blocks on it.
    OUT_FD.store(previous, Ordering::SeqCst);
    // Safety: both ends are ours; closing the write end is what ends the read.
    unsafe { libc::close(fds[1]) };
    guard.fds[1] = -1;
    // Safety: `fds[0]` is an open descriptor nothing else owns — and the guard
    // gives it up as the `File` takes it, so it is never closed twice.
    let mut read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    guard.fds[0] = -1;
    let mut buf = Vec::new();
    read.read_to_end(&mut buf)
        .expect("read the captured escapes");
    (out, buf)
}

static INSTALLED: Once = Once::new();

/// Arm the handlers. Idempotent, and cheap enough to call on every `enter`.
pub(super) fn install() {
    INSTALLED.call_once(|| {
        for sig in HANDLED {
            // No `SA_RESTART`: the handler never returns to the interrupted
            // call anyway. The mask carries every *other* handled signal:
            // only the delivered one is blocked automatically, so without it a
            // SIGINT could re-enter this handler in the middle of
            // `take_flags()` and leave half the terminal restored.
            // Safety: a zeroed `sigaction` with the handler filled in is
            // exactly what the call wants.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = handler as *const () as usize;
                action.sa_flags = 0;
                libc::sigemptyset(&mut action.sa_mask);
                for other in HANDLED {
                    libc::sigaddset(&mut action.sa_mask, other);
                }
                libc::sigaction(sig, &action, std::ptr::null_mut());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::cursor::Show;
    use crossterm::event::DisableMouseCapture;
    use crossterm::queue;
    use crossterm::terminal::LeaveAlternateScreen;

    fn emitted(f: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let mut out = Vec::new();
        f(&mut out);
        out
    }

    /// The handler cannot call crossterm — it allocates and locks — so it
    /// carries its own copy of the escapes. This is the only thing keeping the
    /// two in step across a crossterm upgrade.
    #[test]
    fn the_hand_written_escapes_are_the_ones_crossterm_writes() {
        assert_eq!(SHOW_CURSOR, emitted(|o| queue!(o, Show).unwrap()));
        assert_eq!(
            MOUSE_OFF,
            emitted(|o| queue!(o, DisableMouseCapture).unwrap())
        );
        assert_eq!(
            ALT_OFF,
            emitted(|o| queue!(o, LeaveAlternateScreen).unwrap())
        );
    }

    /// Same contract as `restore_with`: undo exactly what is on, and nothing
    /// at all when nothing is.
    #[test]
    fn the_plan_undoes_exactly_what_was_armed() {
        let all = plan(true, true, true);
        assert_eq!(all.bytes, [SHOW_CURSOR, MOUSE_OFF, ALT_OFF]);
        assert!(all.termios);
        // The cursor comes back first, whatever else was on.
        assert_eq!(all.bytes[0], SHOW_CURSOR);

        // `[ui] mouse = false`: nothing is written that was never armed.
        let no_mouse = plan(false, true, true);
        assert_eq!(no_mouse.bytes, [SHOW_CURSOR, b"", ALT_OFF]);
        assert!(no_mouse.termios);

        // Half-armed, the way `arm_steps` can leave it when a step fails.
        let raw_only = plan(false, false, true);
        assert_eq!(raw_only.bytes, [SHOW_CURSOR, b"", b""]);
        assert!(raw_only.termios);
        let alt_only = plan(false, true, false);
        assert_eq!(alt_only.bytes, [SHOW_CURSOR, b"", ALT_OFF]);
        assert!(!alt_only.termios);

        // Nothing armed: a signal outside the TUI must not spray escapes at a
        // shell that never asked for them.
        let none = plan(false, false, false);
        assert_eq!(none.bytes, [b"", b"", b""]);
        assert!(!none.termios);
    }

    /// The signal path and the guard race by construction, so the handler
    /// takes the flags rather than reading them.
    #[test]
    fn taking_the_flags_leaves_nothing_for_a_second_restore() {
        let _lock = super::super::lifecycle_lock();
        RAW_ON.store(true, Ordering::SeqCst);
        ALT_ON.store(true, Ordering::SeqCst);
        MOUSE_ON.store(true, Ordering::SeqCst);
        assert_eq!(take_flags(), (true, true, true));
        assert_eq!(take_flags(), (false, false, false));
        // And a plan built from the second take does nothing.
        assert_eq!(plan(false, false, false).bytes, [b"", b"", b""]);
    }

    /// The list itself is the contract: the test below walks `HANDLED` and so
    /// would pass just as happily with a signal missing from it.
    #[test]
    fn every_catchable_ending_signal_is_handled() {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
            assert!(
                HANDLED.contains(&sig),
                "signal {sig} ends the process with the terminal still armed"
            );
        }
        // Knowingly absent: SIGTSTP is resumable, so handling it without a
        // SIGCONT re-arm would leave the TUI running on a restored terminal.
        assert!(!HANDLED.contains(&libc::SIGTSTP));
    }

    /// Every signal in `HANDLED` must actually end up armed, and installing
    /// twice must not disturb it.
    #[test]
    fn install_arms_every_handled_signal_and_is_idempotent() {
        install();
        install();
        for sig in HANDLED {
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) },
                0,
                "signal {sig}"
            );
            assert_eq!(
                old.sa_sigaction, handler as *const () as usize,
                "signal {sig} is still on the disposition that leaks the terminal"
            );
            // N3: the other handled signals are blocked for the length of the
            // handler, so one cannot re-enter it mid-`take_flags`.
            for other in HANDLED {
                assert_eq!(
                    unsafe { libc::sigismember(&old.sa_mask, other) },
                    1,
                    "signal {sig} does not block {other}"
                );
            }
        }
    }

    /// N2: the escapes have to reach a descriptor a test can point somewhere
    /// harmless. Writing them to `STDOUT_FILENO` unconditionally bypasses
    /// libtest's capture and lands in the developer's own terminal.
    #[test]
    fn the_restore_writes_the_plan_to_a_descriptor_a_test_can_redirect() {
        let _lock = super::super::lifecycle_lock();
        RAW_ON.store(true, Ordering::SeqCst);
        ALT_ON.store(true, Ordering::SeqCst);
        MOUSE_ON.store(true, Ordering::SeqCst);

        let ((), written) = capturing_output(restore_from_signal);
        let mut want = Vec::new();
        want.extend_from_slice(SHOW_CURSOR);
        want.extend_from_slice(MOUSE_OFF);
        want.extend_from_slice(ALT_OFF);
        assert_eq!(written, want);
        assert_eq!(take_flags(), (false, false, false));

        // Nothing armed writes nothing at all — a signal outside the TUI must
        // not spray escapes at a shell that never asked for them.
        let ((), written) = capturing_output(restore_from_signal);
        assert!(written.is_empty(), "{written:?}");

        // And the descriptor is back where it belongs.
        assert_eq!(OUT_FD.load(Ordering::SeqCst), libc::STDOUT_FILENO);
    }

    /// N4: `write` and `tcsetattr` clobber `errno`, and a handler that returns
    /// owes the interrupted call the value it had.
    #[test]
    fn the_restore_puts_errno_back() {
        let _lock = super::super::lifecycle_lock();
        // A descriptor writes fail on, without the fd-reuse race a *closed*
        // one would have in a parallel test binary.
        // Safety: opening a path this process may read.
        let ro = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
        assert!(ro >= 0, "open /dev/null");
        let previous = OUT_FD.swap(ro, Ordering::SeqCst);

        RAW_ON.store(true, Ordering::SeqCst);
        ALT_ON.store(true, Ordering::SeqCst);
        MOUSE_ON.store(true, Ordering::SeqCst);

        let slot = errno_slot();
        assert!(!slot.is_null(), "no errno accessor for this target");
        // Safety: libc's thread-local slot for this thread.
        unsafe { *slot = libc::ERANGE };
        restore_from_signal();
        // Safety: as above.
        let after = unsafe { *slot };

        OUT_FD.store(previous, Ordering::SeqCst);
        // Safety: our descriptor.
        unsafe { libc::close(ro) };
        assert_eq!(
            after,
            libc::ERANGE,
            "the restore clobbered errno (EBADF from the failed write)"
        );
    }

    /// The promise is the *handler's*, not just the restore's: `signal` and
    /// `raise` run after `restore_from_signal` returns and may set `errno` too.
    /// `handler` cannot be called in process — it re-raises with `SIG_DFL` and
    /// would take the test runner with it — so what is pinned here is the
    /// wrapper the handler and the restore share, with a body that clobbers
    /// `errno` outside any restore.
    #[test]
    fn preserving_errno_covers_whatever_the_body_clobbers() {
        let slot = errno_slot();
        assert!(!slot.is_null(), "no errno accessor for this target");
        // Safety: libc's thread-local slot for this thread.
        unsafe { *slot = libc::ERANGE };
        let out = preserving_errno(|| {
            // Stands in for the two libc calls the handler makes after the
            // restore: fails, sets `errno`, touches nothing of ours.
            // Safety: closing a descriptor that cannot exist.
            assert_eq!(unsafe { libc::close(-1) }, -1);
            // Safety: as above.
            assert_eq!(unsafe { *slot }, libc::EBADF, "the body must clobber it");
            7u8
        });
        assert_eq!(out, 7);
        // Safety: as above.
        assert_eq!(unsafe { *slot }, libc::ERANGE);
    }

    /// A panicking body must not keep the swap: `lifecycle_lock` is
    /// poison-tolerant, so every later test would run with the handler pointed
    /// at a pipe that is already closed.
    #[test]
    fn the_capture_gives_the_descriptor_back_when_the_body_panics() {
        let _lock = super::super::lifecycle_lock();
        let before = OUT_FD.load(Ordering::SeqCst);
        let result = std::panic::catch_unwind(|| capturing_output(|| panic!("the body failed")));
        assert!(result.is_err(), "the panic must reach the caller");
        assert_eq!(OUT_FD.load(Ordering::SeqCst), before);
    }
}
