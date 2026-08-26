//! The Quest brief (SPEC §9): deterministic markdown built from the database,
//! shared by `q brief`, SessionStart hook injection, `q reset`, `q resume`
//! and handoff. Sections 1–10 in spec order; the size cap trims brain body,
//! then events, then links.

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::commands::fmt;
use crate::db::Db;
use crate::error::QError;
use crate::model::{Event, Link, Quest, Session, SessionRole, SessionStatus, display_state};

/// Default for section 9.
pub const DEFAULT_EVENTS: usize = 30;
/// ~6k tokens (SPEC §23 #4).
pub const MAX_CHARS: usize = 24_000;
/// Cap on the brain body before the global cap even applies.
const BRAIN_MAX_CHARS: usize = 8_000;
const RECENT_ENDED: usize = 5;
const PROMPT_WIDTH: usize = 60;
const EXTERNAL_TIMEOUT: Duration = Duration::from_secs(3);

/// Kinds that make it into section 9; prompt/stop chatter does not.
pub const EVENT_KINDS: &[&str] = &[
    "note",
    "phase",
    "link.added",
    "artifact.added",
    "session.reset",
];

#[derive(Debug, Clone)]
pub struct Opts {
    pub role: SessionRole,
    /// The session the brief is for: its id or label within the Quest.
    pub session: Option<String>,
    pub events: usize,
    pub max_chars: usize,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            role: SessionRole::Master,
            session: None,
            events: DEFAULT_EVENTS,
            max_chars: MAX_CHARS,
        }
    }
}

/// `--for` from the flag, else `$Q_ROLE`, else master.
pub fn default_role(flag: Option<SessionRole>) -> SessionRole {
    flag.or_else(|| {
        std::env::var("Q_ROLE")
            .ok()
            .and_then(|r| r.parse::<SessionRole>().ok())
    })
    .unwrap_or(SessionRole::Master)
}

/// `--session` from the flag, else `$Q_SESSION`.
pub fn default_session(flag: Option<&str>) -> Option<String> {
    flag.map(str::to_string)
        .or_else(|| std::env::var("Q_SESSION").ok())
        .filter(|s| !s.is_empty())
}

/// The Quest target from the argument, else `$Q_QUEST`.
pub fn default_target(arg: Option<&str>) -> anyhow::Result<String> {
    arg.map(str::to_string)
        .or_else(|| std::env::var("Q_QUEST").ok())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| QError::Invalid("no quest given and $Q_QUEST is not set".to_string()).into())
}

/// What sections 4 and 8 shell out to. Stubbed in tests and under `Q_FIXTURE`.
pub trait External {
    /// `bd list -l quest:<id> --json`; `None` when `bd` is missing or failed.
    fn bd_list(&self, quest_id: &str) -> Option<String>;
    /// `brain show <slug>`; `None` when `brain` is missing or failed.
    fn brain_show(&self, slug: &str) -> Option<String>;
}

/// The real tools, or — under `$Q_FIXTURE` — canned output from the files
/// `$Q_FIXTURE_BD` / `$Q_FIXTURE_BRAIN` (absent file = tool unavailable).
pub fn external() -> Box<dyn External> {
    match std::env::var_os("Q_FIXTURE") {
        Some(p) if !p.is_empty() => Box::new(FixtureExternal),
        _ => Box::new(RealExternal),
    }
}

struct RealExternal;

impl External for RealExternal {
    fn bd_list(&self, quest_id: &str) -> Option<String> {
        run_capped(
            "bd",
            &["list", "-l", &format!("quest:{quest_id}"), "--json"],
        )
    }
    fn brain_show(&self, slug: &str) -> Option<String> {
        run_capped("brain", &["show", slug])
    }
}

struct FixtureExternal;

impl External for FixtureExternal {
    fn bd_list(&self, _quest_id: &str) -> Option<String> {
        fixture_file("Q_FIXTURE_BD")
    }
    fn brain_show(&self, _slug: &str) -> Option<String> {
        fixture_file("Q_FIXTURE_BRAIN")
    }
}

fn fixture_file(var: &str) -> Option<String> {
    std::fs::read_to_string(std::env::var_os(var)?).ok()
}

/// No-op tools for unit tests and callers that want a DB-only brief.
#[allow(dead_code)]
pub struct NoExternal;

impl External for NoExternal {
    fn bd_list(&self, _quest_id: &str) -> Option<String> {
        None
    }
    fn brain_show(&self, _slug: &str) -> Option<String> {
        None
    }
}

/// Runs `program` with a wall-clock cap; `None` on any failure, non-zero exit
/// or timeout. Output is drained on a thread so a chatty child cannot block.
fn run_capped(program: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        stdout.read_to_string(&mut buf).ok().map(|_| buf)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < EXTERNAL_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = reader.join().ok().flatten();
    match status {
        Some(s) if s.success() => out,
        _ => None,
    }
}

// ------------------------------------------------------------------ rendering

/// The brief for `quest`, using the default external tools.
pub fn render(db: &Db, quest: &Quest, opts: &Opts) -> anyhow::Result<String> {
    render_with(db, quest, opts, external().as_ref())
}

pub fn render_with(
    db: &Db,
    quest: &Quest,
    opts: &Opts,
    ext: &dyn External,
) -> anyhow::Result<String> {
    let sessions = db.list_sessions_by_quest(&quest.id)?;
    let links = db.list_links_by_quest(&quest.id)?;
    let events = db.list_events_by_kinds(&quest.id, EVENT_KINDS, opts.events)?;
    let notes = db.list_events_by_kinds(&quest.id, &["note"], usize::MAX)?;
    let me = opts
        .session
        .as_deref()
        .and_then(|s| sessions.iter().find(|x| x.id == s || x.label == s).cloned());

    let beads = quest.beads_epic.as_deref().map(|_| ext.bd_list(&quest.id));
    let brain = quest
        .brain_session
        .as_deref()
        .and_then(|slug| ext.brain_show(slug));

    // Each trim step tightens one input; the loop stops as soon as the
    // rendering fits.
    let mut brain_cap = BRAIN_MAX_CHARS;
    let mut event_cap = opts.events;
    let mut link_cap = usize::MAX;
    loop {
        let out = assemble(
            quest,
            &sessions,
            &links,
            &events,
            &notes,
            me.as_ref(),
            opts,
            beads.as_ref(),
            brain.as_deref(),
            Caps {
                brain: brain_cap,
                events: event_cap,
                links: link_cap,
            },
        );
        if out.chars().count() <= opts.max_chars {
            return Ok(out);
        }
        if brain.is_some() && brain_cap > 0 {
            brain_cap = if brain_cap > 1_000 { brain_cap / 2 } else { 0 };
        } else if event_cap > 0 {
            event_cap = if event_cap > 5 { event_cap / 2 } else { 0 };
        } else if link_cap > 10 {
            link_cap = 10;
        } else {
            return Ok(out);
        }
    }
}

#[derive(Clone, Copy)]
struct Caps {
    brain: usize,
    events: usize,
    links: usize,
}

#[allow(clippy::too_many_arguments)]
fn assemble(
    quest: &Quest,
    sessions: &[Session],
    links: &[Link],
    events: &[Event],
    notes: &[Event],
    me: Option<&Session>,
    opts: &Opts,
    beads: Option<&Option<String>>,
    brain: Option<&str>,
    caps: Caps,
) -> String {
    let mut out = String::new();
    section_quest(&mut out, quest, sessions);
    section_how(&mut out, quest, sessions, me, opts.role);
    section_workflow(&mut out, quest);
    section_beads(&mut out, quest, beads);
    section_sessions(&mut out, sessions);
    section_links(&mut out, links, caps.links);
    section_artifacts(&mut out, links);
    section_brain(&mut out, quest, brain, caps.brain);
    section_events(&mut out, events, caps.events, opts.events);
    section_blockers(&mut out, notes, sessions);
    out
}

fn section_quest(out: &mut String, quest: &Quest, sessions: &[Session]) {
    out.push_str(&format!("# Quest {} `{}`\n\n", quest.id, quest.slug));
    out.push_str("## 1. Quest\n\n");
    let rows: Vec<(&str, String)> = vec![
        ("id", quest.id.clone()),
        ("slug", quest.slug.clone()),
        ("goal", fmt::or_dash(quest.goal.as_deref())),
        (
            "state",
            format!("{} ({})", quest.state, display_state(quest, sessions)),
        ),
        ("machine", quest.machine.clone()),
        ("cwd", quest.cwd.clone()),
        ("workflow", fmt::or_dash(quest.workflow.as_deref())),
        ("created", fmt::stamp(quest.created_at)),
        ("template", fmt::or_dash(quest.template_id.as_deref())),
    ];
    for (k, v) in rows {
        out.push_str(&format!("- **{k}**: {v}\n"));
    }
    if let Some(ts) = quest.finished_at {
        out.push_str(&format!("- **finished**: {}\n", fmt::stamp(ts)));
    }
    out.push('\n');
}

fn section_how(
    out: &mut String,
    quest: &Quest,
    sessions: &[Session],
    me: Option<&Session>,
    role: SessionRole,
) {
    out.push_str("## 2. How you work here\n\n");
    if let Some(s) = me {
        out.push_str(&format!(
            "You are session `{}` ({}, id `{}`).\n\n",
            s.label, s.role, s.id
        ));
    }
    match role {
        SessionRole::Master => {
            out.push_str(
                "You are the **master** of this Quest: you own the goal, split the work and \
                 keep the picture. Workers are Claude sessions you spawn in this tmux session; \
                 they report back to you.\n\n\
                 - See who is running: `q sessions`; look into one: `q peek <session>`.\n\
                 - Spawn a worker: `q spawn <quest> --label <l> \"<prompt>\"`; talk to it: \
                 `q send <session> \"<text>\"` or `SendMessage`.\n\
                 - Report where you are: `q phase \"<text>\"`.\n\
                 - Link everything you produce: `q link add <ref>` / `q artifact add <path>`.\n\
                 - Record decisions and open questions: `q note \"<text>\"` \
                 (`--blocker` when stuck). When you are done, leave a closing `q note`.\n",
            );
            if quest.beads_epic.is_some() {
                let repo = quest.beads_repo.as_deref().unwrap_or("<repo>");
                out.push_str(&format!(
                    "- Beads: every issue you open carries `-l repo:{repo},quest:{}` and the \
                     epic `{}` as parent.\n",
                    quest.id,
                    quest.beads_epic.as_deref().unwrap_or("-")
                ));
            }
        }
        SessionRole::Worker => {
            let master = sessions
                .iter()
                .filter(|s| s.role == SessionRole::Master && s.status != SessionStatus::Ended)
                .map(|s| format!("`{}` (id `{}`)", s.label, s.id))
                .next()
                .unwrap_or_else(|| "not running right now".to_string());
            out.push_str(&format!(
                "You are a **worker** in this Quest. Your master is {master}.\n\n\
                 - Stay inside the scope your master gave you; ask before widening it.\n\
                 - Report to the master with `SendMessage` or `q send master \"<text>\"`.\n\
                 - Report where you are: `q phase \"<text>\"`.\n\
                 - Link what you produce: `q link add <ref>` / `q artifact add <path>`.\n\
                 - Stuck? `q note --blocker \"<text>\"` and tell the master.\n",
            ));
        }
    }
    out.push('\n');
}

fn section_workflow(out: &mut String, quest: &Quest) {
    out.push_str("## 3. Workflow\n\n");
    match quest.workflow.as_deref() {
        // TODO(M5): render the workflow markdown (SPEC §11) here.
        Some(name) => out.push_str(&format!(
            "Workflow `{name}` is set; its content is not available yet.\n"
        )),
        None => out.push_str("No workflow set.\n"),
    }
    out.push('\n');
}

fn section_beads(out: &mut String, quest: &Quest, beads: Option<&Option<String>>) {
    out.push_str("## 4. Beads\n\n");
    let Some(epic) = quest.beads_epic.as_deref() else {
        out.push_str("No beads epic linked.\n\n");
        return;
    };
    out.push_str(&format!("- **epic**: `{epic}`"));
    if let Some(repo) = quest.beads_repo.as_deref() {
        out.push_str(&format!(" (repo `{repo}`)"));
    }
    out.push('\n');
    match beads.and_then(|b| b.as_deref()) {
        Some(raw) => match parse_bd(raw) {
            Some(issues) => render_issues(out, &issues),
            None => out.push_str("- `bd list` output could not be parsed.\n"),
        },
        None => out.push_str("- `bd list -l quest:<id>` unavailable (bd missing or failed).\n"),
    }
    out.push_str(&format!(
        "- Run `bd prime` when picking up work; list with `bd list -l quest:{}`.\n\n",
        quest.id
    ));
}

struct Issue {
    id: String,
    title: String,
    status: String,
}

/// `bd list --json` is an array of issues; tolerate an object wrapping one.
fn parse_bd(raw: &str) -> Option<Vec<Issue>> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let items = match &value {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o.get("issues")?.as_array()?.clone(),
        _ => return None,
    };
    Some(
        items
            .iter()
            .map(|i| Issue {
                id: str_of(i, "id"),
                title: str_of(i, "title"),
                status: str_of(i, "status"),
            })
            .collect(),
    )
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_string()
}

fn render_issues(out: &mut String, issues: &[Issue]) {
    let mut by_status: BTreeMap<&str, Vec<&Issue>> = BTreeMap::new();
    for i in issues {
        by_status.entry(i.status.as_str()).or_default().push(i);
    }
    let count = |s: &str| by_status.get(s).map_or(0, Vec::len);
    out.push_str(&format!(
        "- **progress**: {}/{} closed · {} open · {} in progress · {} blocked\n",
        count("closed"),
        issues.len(),
        count("open"),
        count("in_progress"),
        count("blocked")
    ));
    for (status, list) in &by_status {
        if *status == "closed" {
            continue;
        }
        for i in list {
            out.push_str(&format!("  - [{status}] `{}` {}\n", i.id, i.title));
        }
    }
}

fn section_sessions(out: &mut String, sessions: &[Session]) {
    out.push_str("## 5. Sessions\n\n");
    let mut live: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.status != SessionStatus::Ended)
        .collect();
    live.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));
    let mut ended: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Ended)
        .collect();
    ended.sort_by(|a, b| b.ended_at.cmp(&a.ended_at).then(b.id.cmp(&a.id)));
    ended.truncate(RECENT_ENDED);
    if live.is_empty() && ended.is_empty() {
        out.push_str("No sessions yet.\n\n");
        return;
    }
    out.push_str("| label | role | status | phase | ctx | last prompt |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for s in live.iter().chain(ended.iter()) {
        let status = match s.waiting_for.as_deref() {
            Some(w) if s.status == SessionStatus::Waiting => format!("waiting ({w})"),
            _ => s.status.to_string(),
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            cell(&s.label),
            s.role,
            status,
            cell(&fmt::or_dash(s.phase.as_deref())),
            s.ctx_pct.map_or("-".to_string(), |p| format!("{p}%")),
            cell(&fmt::oneline(
                s.last_prompt.as_deref().unwrap_or("-"),
                PROMPT_WIDTH
            )),
        ));
    }
    out.push('\n');
}

fn cell(text: &str) -> String {
    text.replace('|', "\\|")
}

fn section_links(out: &mut String, links: &[Link], cap: usize) {
    out.push_str("## 6. Links\n\n");
    let non_artifacts: Vec<&Link> = links.iter().filter(|l| l.kind != "artifact").collect();
    if non_artifacts.is_empty() {
        out.push_str("No links yet.\n\n");
        return;
    }
    let shown = non_artifacts.len().min(cap);
    let mut by_kind: BTreeMap<&str, Vec<&Link>> = BTreeMap::new();
    for l in &non_artifacts[..shown] {
        by_kind.entry(l.kind.as_str()).or_default().push(l);
    }
    for (kind, list) in &by_kind {
        out.push_str(&format!("**{kind}**\n"));
        for l in list {
            out.push_str(&format!("- {}\n", link_line(l)));
        }
    }
    if shown < non_artifacts.len() {
        out.push_str(&format!(
            "\n_(truncated: {} more links; run `q links`)_\n",
            non_artifacts.len() - shown
        ));
    }
    out.push('\n');
}

/// `title — ref (state, CI)`; enrichment is best effort so every piece is
/// optional.
fn link_line(l: &Link) -> String {
    let mut line = match l.title.as_deref() {
        Some(t) if !t.is_empty() => format!("{t} — {}", l.r#ref),
        _ => l.r#ref.clone(),
    };
    let mut extras = Vec::new();
    if let Some(meta) = l.meta.as_ref().and_then(Value::as_object) {
        for key in ["state", "status", "ci"] {
            if let Some(v) = meta.get(key).and_then(Value::as_str) {
                extras.push(format!("{key}: {v}"));
            }
        }
    }
    if !extras.is_empty() {
        line.push_str(&format!(" ({})", extras.join(", ")));
    }
    line
}

fn section_artifacts(out: &mut String, links: &[Link]) {
    out.push_str("## 7. Artifacts\n\n");
    let artifacts: Vec<&Link> = links.iter().filter(|l| l.kind == "artifact").collect();
    if artifacts.is_empty() {
        out.push_str("No artifacts yet.\n\n");
        return;
    }
    for a in artifacts {
        let note = a
            .meta
            .as_ref()
            .and_then(|m| m.get("note"))
            .and_then(Value::as_str)
            .or(a.title.as_deref());
        match note {
            Some(n) if !n.is_empty() => out.push_str(&format!("- `{}` — {n}\n", a.r#ref)),
            _ => out.push_str(&format!("- `{}`\n", a.r#ref)),
        }
    }
    out.push('\n');
}

fn section_brain(out: &mut String, quest: &Quest, brain: Option<&str>, cap: usize) {
    let Some(slug) = quest.brain_session.as_deref() else {
        return;
    };
    out.push_str(&format!("## 8. Brain session `{slug}`\n\n"));
    match brain {
        None => out.push_str("_(brain note unavailable)_\n"),
        Some(body) => {
            let body = body.trim();
            if cap == 0 {
                out.push_str("_(truncated: brain body dropped for size)_\n");
            } else if body.chars().count() > cap {
                out.push_str(&body.chars().take(cap).collect::<String>());
                out.push_str("\n\n_(truncated: brain body cut for size)_\n");
            } else {
                out.push_str(body);
                out.push('\n');
            }
        }
    }
    out.push('\n');
}

fn section_events(out: &mut String, events: &[Event], cap: usize, requested: usize) {
    out.push_str("## 9. Recent events\n\n");
    if events.is_empty() {
        out.push_str("No events yet.\n\n");
        return;
    }
    let shown = events.len().min(cap);
    // Oldest first reads as a log.
    for e in events[..shown].iter().rev() {
        out.push_str(&format!("- {}\n", event_line(e)));
    }
    if shown < events.len() {
        out.push_str(&format!(
            "\n_(truncated: showing {shown} of the last {requested} events; run `q events`)_\n"
        ));
    }
    out.push('\n');
}

fn event_line(e: &Event) -> String {
    let who = e
        .session_id
        .as_deref()
        .map(|s| format!(" [{s}]"))
        .unwrap_or_default();
    let text = event_text(e);
    format!("{} `{}`{who} {text}", fmt::stamp(e.ts), e.kind)
}

/// The human part of a payload: `text` when present, else `k=v` pairs.
fn event_text(e: &Event) -> String {
    match e.payload.as_ref() {
        Some(Value::Object(map)) => {
            let text = ["text", "phase", "ref", "path"]
                .iter()
                .find_map(|k| map.get(*k).and_then(Value::as_str))
                .map(str::to_string);
            let rest: Vec<String> = map
                .iter()
                .filter(|(k, _)| !["text", "phase", "ref", "path"].contains(&k.as_str()))
                .map(|(k, v)| format!("{k}={}", fmt::payload(Some(v), 80)))
                .collect();
            match (text, rest.is_empty()) {
                (Some(t), true) => t,
                (Some(t), false) => format!("{t} ({})", rest.join(" ")),
                (None, _) => rest.join(" "),
            }
        }
        other => fmt::payload(other, 200),
    }
}

fn is_blocker(e: &Event) -> bool {
    let Some(p) = e.payload.as_ref() else {
        return false;
    };
    p.get("blocker").and_then(Value::as_bool) == Some(true)
        || p.get("tag").and_then(Value::as_str) == Some("blocker")
        || p.get("tags")
            .and_then(Value::as_array)
            .is_some_and(|t| t.iter().any(|v| v.as_str() == Some("blocker")))
}

fn section_blockers(out: &mut String, notes: &[Event], sessions: &[Session]) {
    out.push_str("## 10. Open questions / blockers\n\n");
    let blockers: Vec<&Event> = notes.iter().filter(|e| is_blocker(e)).collect();
    let mut waiting: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Waiting)
        .collect();
    waiting.sort_by(|a, b| a.label.cmp(&b.label));
    if blockers.is_empty() && waiting.is_empty() {
        out.push_str("None.\n");
        return;
    }
    for e in blockers.iter().rev() {
        out.push_str(&format!("- BLOCKER {}\n", event_line(e)));
    }
    for s in waiting {
        out.push_str(&format!(
            "- session `{}` is waiting for {}\n",
            s.label,
            s.waiting_for.as_deref().unwrap_or("input")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Session;

    struct Canned {
        bd: Option<String>,
        brain: Option<String>,
    }

    impl External for Canned {
        fn bd_list(&self, _: &str) -> Option<String> {
            self.bd.clone()
        }
        fn brain_show(&self, _: &str) -> Option<String> {
            self.brain.clone()
        }
    }

    fn seeded() -> (Db, Quest) {
        let db = Db::open_in_memory().unwrap();
        let mut quest = Quest::new("alpha", "/tmp/repo", "laptop");
        quest.goal = Some("Ship the thing".to_string());
        quest.beads_epic = Some("bd-42".to_string());
        quest.beads_repo = Some("repo-x".to_string());
        quest.brain_session = Some("alpha-session".to_string());
        let quest = db.insert_quest(&quest).unwrap();

        let master = db
            .insert_session(&Session::new(
                &quest.id,
                SessionRole::Master,
                "master",
                "q-alpha",
                "%1",
            ))
            .unwrap();
        let mut w1 = Session::new(&quest.id, SessionRole::Worker, "w1-tests", "q-alpha", "%2");
        w1.status = SessionStatus::Waiting;
        w1.waiting_for = Some("permission".to_string());
        w1.phase = Some("testing".to_string());
        w1.ctx_pct = Some(42);
        w1.last_prompt = Some("run the suite\nand report".to_string());
        db.insert_session(&w1).unwrap();
        let old = db
            .insert_session(&Session::new(
                &quest.id,
                SessionRole::Worker,
                "w0-old",
                "q-alpha",
                "%3",
            ))
            .unwrap();
        db.mark_session_ended(&old.id, 1_000).unwrap();

        let mut pr = Link::new(&quest.id, "pr", "https://github.com/x/y/pull/1");
        pr.title = Some("Fix backfill".to_string());
        pr.meta = Some(serde_json::json!({ "state": "open", "ci": "passing" }));
        db.insert_link(&pr).unwrap();
        db.insert_link(&Link::new(&quest.id, "branch", "feat/x"))
            .unwrap();
        let mut art = Link::new(&quest.id, "artifact", "/tmp/out/report.html");
        art.meta = Some(serde_json::json!({ "note": "the report" }));
        db.insert_link(&art).unwrap();

        db.append_event(
            &quest.id,
            Some(&master.id),
            "session.prompt",
            &serde_json::json!({ "text": "noise" }),
        )
        .unwrap();
        db.append_event(
            &quest.id,
            Some(&master.id),
            "phase",
            &serde_json::json!({ "phase": "planning" }),
        )
        .unwrap();
        db.append_event(
            &quest.id,
            Some(&master.id),
            "note",
            &serde_json::json!({ "text": "DB is locked", "tag": "blocker" }),
        )
        .unwrap();
        db.append_event(
            &quest.id,
            None,
            "note",
            &serde_json::json!({ "text": "plain note" }),
        )
        .unwrap();
        (db, quest)
    }

    fn headers(md: &str) -> Vec<&str> {
        md.lines().filter(|l| l.starts_with("## ")).collect()
    }

    #[test]
    fn all_sections_in_spec_order() {
        let (db, quest) = seeded();
        let md = render_with(
            &db,
            &quest,
            &Opts::default(),
            &Canned {
                bd: None,
                brain: Some("brain body".to_string()),
            },
        )
        .unwrap();
        assert_eq!(
            headers(&md),
            [
                "## 1. Quest",
                "## 2. How you work here",
                "## 3. Workflow",
                "## 4. Beads",
                "## 5. Sessions",
                "## 6. Links",
                "## 7. Artifacts",
                "## 8. Brain session `alpha-session`",
                "## 9. Recent events",
                "## 10. Open questions / blockers",
            ]
        );
        assert!(md.contains("**goal**: Ship the thing"));
        assert!(md.contains("bd list -l quest:<id>` unavailable"));
        assert!(
            md.contains("Fix backfill — https://github.com/x/y/pull/1 (state: open, ci: passing)")
        );
        assert!(md.contains("`/tmp/out/report.html` — the report"));
        assert!(md.contains("brain body"));
        assert!(md.contains("| w1-tests | worker | waiting (permission) | testing | 42% | run the suite and report |"));
        assert!(md.contains("| w0-old | worker | ended |"));
        assert!(!md.contains("noise"), "prompt events are noise:\n{md}");
        assert!(md.contains("- BLOCKER"));
        assert!(md.contains("DB is locked"));
        assert!(md.contains("session `w1-tests` is waiting for permission"));
    }

    #[test]
    fn events_are_oldest_first_and_blockers_are_notes_only() {
        let (db, quest) = seeded();
        let md = render_with(&db, &quest, &Opts::default(), &NoExternal).unwrap();
        let events = md
            .split("## 9. Recent events")
            .nth(1)
            .unwrap()
            .split("## 10.")
            .next()
            .unwrap();
        let phase = events.find("`phase`").unwrap();
        let note = events.find("plain note").unwrap();
        assert!(phase < note, "{events}");
        let blockers = md.split("## 10.").nth(1).unwrap();
        assert!(!blockers.contains("plain note"));
    }

    #[test]
    fn beads_output_is_summarised() {
        let (db, quest) = seeded();
        let bd = serde_json::json!([
            { "id": "bd-1", "title": "done", "status": "closed" },
            { "id": "bd-2", "title": "doing", "status": "in_progress" },
            { "id": "bd-3", "title": "stuck", "status": "blocked" },
        ])
        .to_string();
        let md = render_with(
            &db,
            &quest,
            &Opts::default(),
            &Canned {
                bd: Some(bd),
                brain: None,
            },
        )
        .unwrap();
        assert!(md.contains("**progress**: 1/3 closed · 0 open · 1 in progress · 1 blocked"));
        assert!(md.contains("[blocked] `bd-3` stuck"));
        assert!(!md.contains("`bd-1`"));
        assert!(md.contains("repo:repo-x,quest:"));
        assert!(md.contains("_(brain note unavailable)_"));
    }

    #[test]
    fn worker_and_master_get_different_instructions() {
        let (db, quest) = seeded();
        let master = render_with(&db, &quest, &Opts::default(), &NoExternal).unwrap();
        assert!(master.contains("You are the **master**"));
        let opts = Opts {
            role: SessionRole::Worker,
            session: Some("w1-tests".to_string()),
            ..Opts::default()
        };
        let worker = render_with(&db, &quest, &opts, &NoExternal).unwrap();
        assert!(worker.contains("You are a **worker**"));
        assert!(worker.contains("Your master is `master`"));
        assert!(worker.contains("You are session `w1-tests` (worker"));
    }

    #[test]
    fn rendering_is_deterministic() {
        let (db, quest) = seeded();
        let a = render_with(&db, &quest, &Opts::default(), &NoExternal).unwrap();
        let b = render_with(&db, &quest, &Opts::default(), &NoExternal).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn size_cap_trims_brain_then_events_then_links() {
        let (db, quest) = seeded();
        for i in 0..40 {
            db.append_event(
                &quest.id,
                None,
                "note",
                &serde_json::json!({ "text": format!("note {i} {}", "x".repeat(200)) }),
            )
            .unwrap();
        }
        for i in 0..30 {
            db.insert_link(&Link::new(&quest.id, "url", &format!("https://x/{i}")))
                .unwrap();
        }
        let brain = Canned {
            bd: None,
            brain: Some("b".repeat(5_000)),
        };
        let full = render_with(&db, &quest, &Opts::default(), &brain).unwrap();
        assert!(!full.contains("truncated"), "{full}");

        // Brain goes first.
        let opts = Opts {
            max_chars: full.chars().count() - 3_000,
            ..Opts::default()
        };
        let md = render_with(&db, &quest, &opts, &brain).unwrap();
        assert!(md.contains("_(truncated: brain body cut for size)_"));
        assert!(!md.contains("truncated: showing"));

        // Then events.
        let opts = Opts {
            max_chars: 8_000,
            ..Opts::default()
        };
        let md = render_with(&db, &quest, &opts, &brain).unwrap();
        assert!(md.contains("brain body dropped"));
        assert!(md.contains("truncated: showing"), "{md}");
        assert!(!md.contains("more links"));

        // Finally links.
        let opts = Opts {
            max_chars: 2_000,
            ..Opts::default()
        };
        let md = render_with(&db, &quest, &opts, &brain).unwrap();
        assert!(md.contains("more links"), "{md}");
        assert!(md.contains("## 10. Open questions / blockers"));
    }

    #[test]
    fn bd_json_wrapped_in_an_object_is_accepted() {
        let issues = parse_bd(r#"{"issues":[{"id":"bd-1","title":"t","status":"open"}]}"#).unwrap();
        assert_eq!(issues.len(), 1);
        assert!(parse_bd("nope").is_none());
    }
}
