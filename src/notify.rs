//! Desktop / push notifications on session state transitions (SPEC §20–21).
//!
//! Two halves, kept apart so the decision is unit-testable and the side effect
//! never fires in a test:
//!
//! * [`plan`] is pure — given the `[notify]` config and an event [`Kind`] it
//!   says which channels should fire (macOS, ntfy, both, or neither). It owns
//!   the `on` filter, the `macos` toggle and the empty-`ntfy_topic` skip.
//! * a [`Runner`] does the impure work. The real one ([`SysRunner`]) shells out
//!   to `osascript` / `curl`, fire-and-forget on a detached thread with a short
//!   timeout so it never blocks — or fails — the caller. Tests pass a
//!   [`CaptureRunner`] and assert the invocation without a process ever
//!   spawning; [`runner`] additionally swaps in a file-recording [`FixtureRunner`]
//!   whenever `$Q_NOTIFY_FIXTURE` is set, the same `Q_FIXTURE` shape `tmux` uses,
//!   so no code path reachable from a hook can notify for real under test.
//!
//! Everything here is best effort: a missing binary, a non-macOS host, an empty
//! topic or a curl failure is silently nothing, never an error.

use std::process::Command;
use std::time::Duration;

use crate::config::Notify;

/// A transition worth telling the human about. Its [`Kind::tag`] is exactly the
/// token the `[notify] on` list carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Waiting,
    Reset,
    Ended,
    /// A session went `off` — Claude left an otherwise-live pane (SPEC §6, §20).
    /// Not in the default `[notify] on`, so it is silent unless opted into.
    Off,
}

impl Kind {
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Waiting => "waiting",
            Kind::Reset => "reset",
            Kind::Ended => "ended",
            Kind::Off => "off",
        }
    }
}

/// The pure verdict: which channels [`emit`] should drive for one event. Built
/// only by [`plan`]; `fires` is false when the `on` filter excluded the kind or
/// nothing is configured, and then [`emit`] does nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channels {
    pub macos: bool,
    /// `Some(topic)` only when a non-empty `ntfy_topic` is configured.
    pub ntfy_topic: Option<String>,
}

impl Channels {
    fn none() -> Channels {
        Channels {
            macos: false,
            ntfy_topic: None,
        }
    }

    /// Whether any channel would actually fire.
    pub fn fires(&self) -> bool {
        self.macos || self.ntfy_topic.is_some()
    }
}

/// Pure decision (SPEC §21): the `on` filter first, then the per-channel
/// toggles. A kind not listed in `on` fires nothing, whatever the toggles say.
pub fn plan(cfg: &Notify, kind: Kind) -> Channels {
    if !cfg.on.iter().any(|k| k == kind.tag()) {
        return Channels::none();
    }
    Channels {
        macos: cfg.macos,
        ntfy_topic: (!cfg.ntfy_topic.is_empty()).then(|| cfg.ntfy_topic.clone()),
    }
}

/// The impure half, behind a trait so a test drives it without a process — the
/// same shape `tmux` and `bd` are gated with.
pub trait Runner: Send + Sync {
    fn macos(&self, title: &str, body: &str);
    fn ntfy(&self, topic: &str, title: &str, body: &str);
}

/// Decide, then dispatch through `runner`. The one entry point every call site
/// and every test goes through; it never blocks and never fails.
pub fn emit(cfg: &Notify, runner: &dyn Runner, kind: Kind, title: &str, body: &str) {
    let channels = plan(cfg, kind);
    if !channels.fires() {
        return;
    }
    if channels.macos {
        runner.macos(title, body);
    }
    if let Some(topic) = channels.ntfy_topic {
        runner.ntfy(&topic, title, body);
    }
}

/// The runner for a real invocation:
/// * the recording [`FixtureRunner`] when `$Q_NOTIFY_FIXTURE` names a file — a
///   test that wants to assert the invocation,
/// * a [`NoopRunner`] when `$Q_FIXTURE` is set — the tmux fixture world every
///   integration test runs in, where a real `osascript`/`curl` must never fire
///   even for a path that did not opt into recording (the same `Q_FIXTURE`
///   convention `tmux` is stubbed with),
/// * else the real [`SysRunner`].
pub fn runner() -> Box<dyn Runner> {
    if let Some(path) = std::env::var_os("Q_NOTIFY_FIXTURE").filter(|p| !p.is_empty()) {
        return Box::new(FixtureRunner { path: path.into() });
    }
    if std::env::var_os("Q_FIXTURE").is_some_and(|p| !p.is_empty()) {
        return Box::new(NoopRunner);
    }
    Box::new(SysRunner)
}

/// Fires nothing: the default under the tmux fixture so no test notifies for
/// real without asking to record.
struct NoopRunner;

impl Runner for NoopRunner {
    fn macos(&self, _title: &str, _body: &str) {}
    fn ntfy(&self, _topic: &str, _title: &str, _body: &str) {}
}

/// How long a notification child gets before it is killed. Generous for a local
/// `osascript`, a firm cap for a `curl` to the network.
const TIMEOUT: Duration = Duration::from_secs(3);

/// The real thing. Each channel spawns a detached thread that runs the child
/// under [`crate::proc`]'s bounded waiter — so the caller returns at once, the
/// child is reaped even inside a long-lived `q watch`, and a hang cannot outlast
/// [`TIMEOUT`].
pub struct SysRunner;

impl Runner for SysRunner {
    fn macos(&self, title: &str, body: &str) {
        // AppleScript string literals take backslash and double-quote escapes;
        // everything else is literal. The argv itself never touches a shell.
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            applescript_escape(body),
            applescript_escape(title),
        );
        spawn_detached(move || {
            let mut cmd = Command::new("osascript");
            cmd.arg("-e").arg(&script);
            let _ = crate::proc::run(&mut cmd, b"", TIMEOUT);
        });
    }

    fn ntfy(&self, topic: &str, title: &str, body: &str) {
        let url = format!("https://ntfy.sh/{topic}");
        let title = title.to_string();
        let body = body.to_string();
        spawn_detached(move || {
            let mut cmd = Command::new("curl");
            cmd.arg("-fsS")
                .arg("-m")
                .arg("3")
                .arg("-H")
                .arg(format!("Title: {title}"))
                .arg("-d")
                .arg(&body)
                .arg(&url);
            let _ = crate::proc::run(&mut cmd, b"", TIMEOUT);
        });
    }
}

fn spawn_detached(f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name("q-notify".into()).spawn(f);
}

fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// One recorded call, for a test to assert against.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Call {
    pub channel: String,
    pub topic: Option<String>,
    pub title: String,
    pub body: String,
}

/// An in-memory [`Runner`] for the in-crate tests: records every call instead
/// of making it.
#[cfg(test)]
#[derive(Default)]
pub struct CaptureRunner {
    calls: std::sync::Mutex<Vec<Call>>,
}

#[cfg(test)]
impl CaptureRunner {
    pub fn new() -> CaptureRunner {
        CaptureRunner::default()
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[cfg(test)]
impl Runner for CaptureRunner {
    fn macos(&self, title: &str, body: &str) {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Call {
                channel: "macos".into(),
                topic: None,
                title: title.into(),
                body: body.into(),
            });
    }

    fn ntfy(&self, topic: &str, title: &str, body: &str) {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Call {
                channel: "ntfy".into(),
                topic: Some(topic.into()),
                title: title.into(),
                body: body.into(),
            });
    }
}

/// A [`Runner`] that appends each call as a JSON line to `$Q_NOTIFY_FIXTURE`,
/// so a caller reached across a process boundary (a hook) still records rather
/// than notifies. The `Q_FIXTURE` analog for notifications.
struct FixtureRunner {
    path: std::path::PathBuf,
}

impl FixtureRunner {
    fn record(&self, call: &Call) {
        use std::io::Write;
        if let Ok(line) = serde_json::to_string(call)
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

impl Runner for FixtureRunner {
    fn macos(&self, title: &str, body: &str) {
        self.record(&Call {
            channel: "macos".into(),
            topic: None,
            title: title.into(),
            body: body.into(),
        });
    }

    fn ntfy(&self, topic: &str, title: &str, body: &str) {
        self.record(&Call {
            channel: "ntfy".into(),
            topic: Some(topic.into()),
            title: title.into(),
            body: body.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(macos: bool, topic: &str, on: &[&str]) -> Notify {
        Notify {
            macos,
            ntfy_topic: topic.to_string(),
            on: on.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn the_on_filter_gates_every_channel() {
        // `reset` is not in `on`, so nothing fires however the toggles read.
        let c = cfg(true, "mytopic", &["waiting", "ended"]);
        let ch = plan(&c, Kind::Reset);
        assert!(!ch.fires());
        assert!(!ch.macos);
        assert_eq!(ch.ntfy_topic, None);

        // A listed kind fires the configured channels.
        let ch = plan(&c, Kind::Waiting);
        assert!(ch.macos);
        assert_eq!(ch.ntfy_topic.as_deref(), Some("mytopic"));
    }

    #[test]
    fn macos_false_suppresses_the_desktop_channel() {
        let c = cfg(false, "mytopic", &["waiting"]);
        let ch = plan(&c, Kind::Waiting);
        assert!(!ch.macos);
        assert_eq!(ch.ntfy_topic.as_deref(), Some("mytopic"));
    }

    #[test]
    fn an_empty_topic_skips_ntfy() {
        let c = cfg(true, "", &["waiting"]);
        let ch = plan(&c, Kind::Waiting);
        assert!(ch.macos);
        assert_eq!(ch.ntfy_topic, None);
    }

    #[test]
    fn both_off_fires_nothing() {
        let c = cfg(false, "", &["waiting", "reset", "ended"]);
        for kind in [Kind::Waiting, Kind::Reset, Kind::Ended] {
            assert!(!plan(&c, kind).fires());
        }
    }

    #[test]
    fn emit_drives_exactly_the_planned_channels() {
        let c = cfg(true, "mytopic", &["ended"]);
        let runner = CaptureRunner::new();
        emit(&c, &runner, Kind::Ended, "quest · ended", "worker ended");
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].channel, "macos");
        assert_eq!(calls[0].title, "quest · ended");
        assert_eq!(calls[1].channel, "ntfy");
        assert_eq!(calls[1].topic.as_deref(), Some("mytopic"));
        assert_eq!(calls[1].body, "worker ended");

        // A kind outside `on` records nothing.
        let runner = CaptureRunner::new();
        emit(&c, &runner, Kind::Waiting, "t", "b");
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn a_transition_guard_de_dupes_a_repeated_waiting() {
        use crate::model::SessionStatus;
        let c = cfg(true, "", &["waiting"]);
        let runner = CaptureRunner::new();
        // Mirrors the hooks.rs site: notify only on the edge INTO waiting.
        let notify_if_entering = |prev: SessionStatus| {
            if prev != SessionStatus::Waiting {
                emit(&c, &runner, Kind::Waiting, "t", "b");
            }
        };
        notify_if_entering(SessionStatus::Busy); // enters -> fires
        notify_if_entering(SessionStatus::Waiting); // already there -> skipped
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn off_is_a_channel_that_defaults_silent() {
        // The tag matches the config token.
        assert_eq!(Kind::Off.tag(), "off");
        // Not in the shipped default `on`, so a default config stays quiet.
        let default = Notify::default();
        assert!(!default.on.iter().any(|k| k == "off"));
        assert!(!plan(&default, Kind::Off).fires());
        // Opting in fires the configured channels.
        let c = cfg(true, "mytopic", &["off"]);
        let ch = plan(&c, Kind::Off);
        assert!(ch.macos);
        assert_eq!(ch.ntfy_topic.as_deref(), Some("mytopic"));
    }

    #[test]
    fn applescript_metacharacters_are_escaped() {
        assert_eq!(applescript_escape(r#"a "b" \c"#), r#"a \"b\" \\c"#);
    }
}
