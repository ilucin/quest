//! Running a short-lived child process with a deadline. Everything q shells
//! out to on a hot path (the statusline chain, `q doctor`'s probes) has to
//! come back promptly or not at all, so the wait is always bounded and stderr
//! is always dropped.

use std::io::{Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How often the wait loop polls the child.
const POLL: Duration = Duration::from_millis(20);

pub struct Outcome {
    /// `None` when the child was killed for outliving the timeout.
    pub status: Option<ExitStatus>,
    /// Whatever the child printed before it exited or was killed.
    pub stdout: Vec<u8>,
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
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

/// Runs `cmd` with `input` on stdin and at most `timeout` to finish. stdout is
/// captured, stderr discarded; a child that overruns is killed and its partial
/// output kept. Only a failure to spawn is an error.
pub fn run(cmd: &mut Command, input: &[u8], timeout: Duration) -> std::io::Result<Outcome> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    // Both pipes are drained off-thread: a child that fills stdout while we
    // write stdin (or vice versa) must not deadlock the wait loop.
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.to_vec();
        thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let reader = child.stdout.take().map(|mut stdout| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        })
    });

    let deadline = Instant::now() + timeout;
    let mut status = None;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            // Timed out, or the child became unwaitable: either way, stop it.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    let stdout = reader.and_then(|r| r.join().ok()).unwrap_or_default();
    Ok(Outcome { status, stdout })
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
    fn stdin_reaches_the_child_and_stderr_is_dropped() {
        let out = sh("cat; echo noise >&2", b"payload", 5000);
        assert_eq!(out.text(), "payload");
    }

    #[test]
    fn an_overrunning_child_is_killed_but_keeps_its_partial_output() {
        let out = sh("echo partial; sleep 30", b"", 200);
        assert!(out.timed_out());
        assert!(!out.success());
        assert_eq!(out.code(), None);
        assert_eq!(out.text(), "partial");
    }

    #[test]
    fn a_missing_binary_is_the_only_error() {
        let mut cmd = Command::new("q-definitely-not-a-binary");
        assert!(run(&mut cmd, b"", Duration::from_millis(100)).is_err());
    }
}
