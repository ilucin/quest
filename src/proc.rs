//! Running a short-lived child process with a deadline. Everything q shells
//! out to on a hot path (the statusline chain, `q doctor`'s probes) has to
//! come back promptly or not at all, so both the wait *and* the reading of the
//! child's pipes are bounded, and the child runs in its own process group so
//! a forking chain dies whole.

use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// How often the wait loop polls the child.
const POLL: Duration = Duration::from_millis(20);
/// Once the child is gone the readers get this long to hand over what they
/// already have — no more. A grandchild may still hold the write end, so this
/// window is all the caller can ever be asked to wait for the pipes.
const DRAIN_FLOOR: Duration = Duration::from_millis(250);
/// Default cap on what one pipe contributes. q shows a line or two of most of
/// this output, and a chatty child must not be able to grow the buffer without
/// bound — but a caller that really does want a whole payload (a `bd list
/// --json` of a busy tracker) passes its own cap to [`run_bounded`] /
/// [`run_capped_bounded`], because a JSON document truncated mid-object parses
/// as nothing at all.
pub const MAX_CAPTURE: usize = 64 * 1024;

pub struct Outcome {
    /// `None` when the child was killed for outliving the timeout.
    pub status: Option<ExitStatus>,
    /// Whatever the child printed before it exited or was killed.
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Outcome {
    pub fn timed_out(&self) -> bool {
        self.status.is_none()
    }

    pub fn success(&self) -> bool {
        self.status.is_some_and(|s| s.success())
    }

    /// The exit code, absent for a timeout or a signal.
    pub fn code(&self) -> Option<i32> {
        self.status.and_then(|s| s.code())
    }

    /// Trimmed stdout, lossily decoded.
    pub fn text(&self) -> String {
        text(&self.stdout)
    }

    /// Trimmed stderr, lossily decoded.
    pub fn stderr_text(&self) -> String {
        text(&self.stderr)
    }

    /// The child's own complaint, first line only, for a one-line warning.
    /// Its stderr if it said anything there, else its stdout, else the status.
    pub fn message(&self) -> String {
        let stderr = self.stderr_text();
        let said = if stderr.is_empty() {
            self.text()
        } else {
            stderr
        };
        match said.lines().find(|l| !l.trim().is_empty()) {
            Some(line) => line.trim().to_string(),
            None => match self.status {
                Some(status) => format!("exited with {status}"),
                None => "timed out".to_string(),
            },
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

/// Runs `cmd` with `input` on stdin and at most `timeout` to finish. Both
/// pipes are captured; a child that overruns is killed — process group and
/// all — and its partial output kept. Only a failure to spawn is an error.
pub fn run(cmd: &mut Command, input: &[u8], timeout: Duration) -> std::io::Result<Outcome> {
    run_bounded(cmd, input, timeout, None, MAX_CAPTURE)
}

/// [`run`] that also prints `notice.1` to stderr, once, if the child is still
/// going after `notice.0` — so a slow write does not read as a hang.
pub fn run_noticed(
    cmd: &mut Command,
    input: &[u8],
    timeout: Duration,
    notice: Option<(Duration, &str)>,
) -> std::io::Result<Outcome> {
    run_bounded(cmd, input, timeout, notice, MAX_CAPTURE)
}

/// [`run_noticed`] with an explicit per-pipe capture cap, for the one caller
/// whose output is a document rather than a line: past the cap the bytes are
/// dropped, which would leave a JSON payload unparseable.
pub fn run_bounded(
    cmd: &mut Command,
    input: &[u8],
    timeout: Duration,
    notice: Option<(Duration, &str)>,
    max_capture: usize,
) -> std::io::Result<Outcome> {
    use std::os::unix::process::CommandExt;

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so a timeout can kill the whole tree: a
        // chain like `sh -c 'foo & wait'` leaves a grandchild holding the
        // stdout pipe, and killing only the shell would not free it.
        .process_group(0)
        .spawn()?;

    // Both pipes are drained off-thread: a child that fills stdout while we
    // write stdin (or vice versa) must not deadlock the wait loop.
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.to_vec();
        thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let out_rx = child.stdout.take().map(|p| drain(p, max_capture));
    let err_rx = child.stderr.take().map(|p| drain(p, max_capture));

    let started = Instant::now();
    let mut status = None;
    let mut announced = false;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) if started.elapsed() < timeout => {
                if let Some((after, text)) = notice
                    && !announced
                    && started.elapsed() >= after
                {
                    eprintln!("{text}");
                    announced = true;
                }
                thread::sleep(POLL);
            }
            // Timed out, or the child became unwaitable: either way, stop it.
            _ => {
                kill_group(&mut child);
                break;
            }
        }
    }
    // Every way out of that loop leaves the child reaped or killed, so the
    // only writer that can still hold a pipe is a survivor of the group kill.
    // Waiting out the rest of the budget for one would charge a child that
    // finished in 10ms for its background `sleep 30`: the drain window is
    // always just the floor. Joining the readers is never an option either —
    // a survivor could hold the pipe open forever.
    let until = Instant::now() + DRAIN_FLOOR;
    Ok(Outcome {
        status,
        stdout: collect(out_rx, until),
        stderr: collect(err_rx, until),
    })
}

/// `Some(stdout)` only when `program` ran to a successful exit inside the cap.
/// The argv shorthand for the many read-only probes that want nothing else.
pub fn run_capped(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    run_capped_bounded(program, args, timeout, MAX_CAPTURE)
}

/// [`run_capped`] with an explicit per-pipe capture cap.
pub fn run_capped_bounded(
    program: &str,
    args: &[&str],
    timeout: Duration,
    max_capture: usize,
) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    run_bounded(&mut cmd, b"", timeout, None, max_capture)
        .ok()
        .filter(Outcome::success)
        .map(|out| out.text())
}

/// SIGKILL to the child's whole process group, then reap it. `process_group(0)`
/// made the child its own group leader, so its pid is the group id.
fn kill_group(child: &mut Child) {
    let pid = child.id() as libc::pid_t;
    // SAFETY: killpg on the group q just created for this child; the worst a
    // race can do is fail with ESRCH, which is ignored.
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// A pipe being read off-thread: what has arrived so far, plus the signal that
/// nothing more will. The buffer is shared rather than sent at the end so a
/// reader that never reaches EOF — a grandchild still holding the write end —
/// still yields its partial output.
struct Draining {
    buf: Arc<Mutex<Vec<u8>>>,
    eof: mpsc::Receiver<()>,
}

fn drain(mut pipe: impl Read + Send + 'static, max_capture: usize) -> Draining {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::default();
    let (tx, eof) = mpsc::channel();
    let sink = Arc::clone(&buf);
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let mut buf = lock(&sink);
                    let room = max_capture.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                    // Past the cap the bytes are dropped but the pipe is still
                    // read, so the child never blocks on a full pipe.
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = tx.send(());
    });
    Draining { buf, eof }
}

/// Everything read by EOF, or by `until` — whichever comes first. An abandoned
/// reader thread costs its buffer, not the caller's deadline.
fn collect(pipe: Option<Draining>, until: Instant) -> Vec<u8> {
    let Some(pipe) = pipe else {
        return Vec::new();
    };
    let _ = pipe
        .eof
        .recv_timeout(until.saturating_duration_since(Instant::now()));
    lock(&pipe.buf).clone()
}

/// A panicking reader thread must not poison the output: the bytes it already
/// collected are still good.
fn lock(buf: &Mutex<Vec<u8>>) -> std::sync::MutexGuard<'_, Vec<u8>> {
    buf.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, input: &[u8], ms: u64) -> Outcome {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        run(&mut cmd, input, Duration::from_millis(ms)).unwrap()
    }

    #[test]
    fn a_finished_child_reports_its_code_and_output() {
        let out = sh("echo hi", b"", 5000);
        assert!(out.success());
        assert_eq!(out.code(), Some(0));
        assert!(!out.timed_out());
        assert_eq!(out.text(), "hi");

        let out = sh("exit 3", b"", 5000);
        assert!(!out.success());
        assert_eq!(out.code(), Some(3));
    }

    #[test]
    fn stdin_reaches_the_child_and_stderr_is_captured_separately() {
        let out = sh("cat; echo noise >&2", b"payload", 5000);
        assert_eq!(out.text(), "payload");
        assert_eq!(out.stderr_text(), "noise");
    }

    #[test]
    fn an_overrunning_child_is_killed_but_keeps_its_partial_output() {
        // `sleep` in the background makes `sh` fork: the grandchild inherits
        // the stdout pipe, so only killing the group ends the read.
        let started = Instant::now();
        let out = sh("echo partial; sleep 30 & wait", b"", 200);
        assert!(out.timed_out());
        assert!(!out.success());
        assert_eq!(out.code(), None);
        assert_eq!(out.text(), "partial");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_forking_child_leaves_no_survivor() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        // The grandchild's write is a whole second out — an order of magnitude
        // past the budget — and we then wait half again as long, so no amount
        // of scheduler jitter can make a killed grandchild look alive or a
        // surviving one look dead.
        let out = sh(
            &format!("(sleep 1; : > {}) & echo partial; wait", marker.display()),
            b"",
            100,
        );
        assert_eq!(out.text(), "partial");
        thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists(), "the grandchild survived the group kill");
    }

    #[test]
    fn a_background_grandchild_does_not_hold_up_a_child_that_finished() {
        // `sh` exits at once, but its background `sleep` inherited the stdout
        // pipe, so EOF never comes. The drain window, not the remaining
        // budget, is what the caller waits for.
        let started = Instant::now();
        let out = sh("echo hi; sleep 30 &", b"", 30_000);
        let elapsed = started.elapsed();
        assert_eq!(out.text(), "hi");
        assert_eq!(out.code(), Some(0));
        assert!(
            elapsed < Duration::from_secs(1),
            "the surviving `sleep` held up the return: {elapsed:?}"
        );
    }

    #[test]
    fn a_flood_of_output_is_capped() {
        let out = sh("yes qqqqqqqq | head -c 200000", b"", 5000);
        assert!(out.success());
        assert_eq!(out.stdout.len(), MAX_CAPTURE);
    }

    #[test]
    fn a_caller_that_wants_a_whole_document_can_raise_the_cap() {
        // 200 KB is past the default cap, so the default truncates and a
        // caller that needs the payload intact has to ask for more room.
        let script = "yes qqqqqqqq | head -c 200000";
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let big = run_bounded(&mut cmd, b"", Duration::from_secs(5), None, 1 << 20).unwrap();
        assert_eq!(big.stdout.len(), 200_000);
        assert_eq!(
            run_capped_bounded("sh", &["-c", script], Duration::from_secs(5), 1 << 20)
                .map(|s| s.len()),
            Some(200_000)
        );
        // The default is still the default.
        assert_eq!(sh(script, b"", 5000).stdout.len(), MAX_CAPTURE);
    }

    #[test]
    fn a_missing_binary_is_the_only_error() {
        let mut cmd = Command::new("q-definitely-not-a-binary");
        assert!(run(&mut cmd, b"", Duration::from_millis(100)).is_err());
    }

    #[test]
    fn only_a_successful_run_yields_capped_output() {
        let cap = Duration::from_secs(5);
        assert_eq!(
            run_capped("sh", &["-c", "echo hi"], cap).as_deref(),
            Some("hi")
        );
        assert!(run_capped("sh", &["-c", "echo x; exit 3"], cap).is_none());
        assert!(run_capped("q-definitely-not-a-binary", &[], cap).is_none());
        assert!(run_capped("sh", &["-c", "sleep 30"], Duration::from_millis(200)).is_none());
    }

    #[test]
    fn the_message_prefers_stderr_then_stdout_then_the_status() {
        assert_eq!(
            sh("echo out; echo err >&2; exit 1", b"", 5000).message(),
            "err"
        );
        assert_eq!(
            sh("echo only-stdout; exit 1", b"", 5000).message(),
            "only-stdout"
        );
        let quiet = sh("exit 1", b"", 5000).message();
        assert!(quiet.contains("exit"), "{quiet}");
        assert_eq!(sh("sleep 30", b"", 200).message(), "timed out");
    }

    #[test]
    fn the_notice_fires_only_for_a_child_that_outstays_its_warm_up() {
        fn noticed(script: &str, after: Duration) -> Outcome {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(script);
            run_noticed(
                &mut cmd,
                b"",
                Duration::from_secs(5),
                Some((after, "waiting")),
            )
            .expect("ran")
        }
        // Fires: still running well past the warm-up. Does not fire: gone
        // before it. Both must succeed; the test process's own stderr is not
        // assertable here, so the output is what is checked.
        assert_eq!(
            noticed("sleep 0.3; echo done", Duration::from_millis(50)).text(),
            "done"
        );
        assert_eq!(
            noticed("echo quick", Duration::from_secs(30)).text(),
            "quick"
        );
    }
}
