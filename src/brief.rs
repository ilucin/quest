//! The Quest brief (SPEC §9): deterministic markdown built from the database,
//! shared by `q brief`, SessionStart hook injection, `q reset`, `q resume`
//! and handoff. Sections 1–10 in spec order; the size cap first caps links,
//! then halves the brain body, then the events. Beads and sessions are never
//! trimmed, which is why `MAX_CHARS` leaves headroom under the 32k budget.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::beads;
use crate::commands::fmt;
use crate::db::Db;
use crate::error::QError;
use crate::model::{Event, Link, Quest, Session, SessionRole, SessionStatus, display_state};
use crate::proc;

/// Default for section 9.
pub const DEFAULT_EVENTS: usize = 30;
/// ~6k tokens (SPEC §23 #4). Beads and sessions are never trimmed, so this
/// stays well under the 32k budget the hook injection has to fit.
pub const MAX_CHARS: usize = 24_000;
/// Cap on the brain body before the global cap even applies.
const BRAIN_MAX_CHARS: usize = 8_000;
const RECENT_ENDED: usize = 5;
const LINKS_CAP: usize = 10;
const NOTE_WIDTH: usize = 300;
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
    /// `brain show <slug>`; `None` when `brain` is missing or failed.
    fn brain_show(&self, slug: &str) -> Option<String>;

    /// The Quest's issues, always through `beads.rs` — the very call `q show`
    /// makes, so the brief cannot show a shorter or differently filtered list.
    /// `$Q_FIXTURE` is honoured in there, not here.
    fn bd_list(&self, quest_id: &str) -> Option<String> {
        beads::client().list_quest(quest_id)
    }
}

/// The real tools, or — under `$Q_FIXTURE` — canned output from the files
/// `$Q_FIXTURE_BD` / `$Q_FIXTURE_BRAIN` (absent file = tool unavailable).
/// `Q_FIXTURE` itself is the tmux stub's fixture (see `tmux.rs`); here it is
/// only read as a "we are in a test" switch, its value is not used.
pub fn external() -> Box<dyn External> {
    match std::env::var_os("Q_FIXTURE") {
        Some(p) if !p.is_empty() => Box::new(FixtureExternal),
        _ => Box::new(RealExternal),
    }
}

struct RealExternal;

impl External for RealExternal {
    fn brain_show(&self, slug: &str) -> Option<String> {
        proc::run_capped("brain", &["show", slug], EXTERNAL_TIMEOUT)
    }
}

struct FixtureExternal;

impl External for FixtureExternal {
    fn brain_show(&self, _slug: &str) -> Option<String> {
        fixture_file("Q_FIXTURE_BRAIN")
    }
}

fn fixture_file(var: &str) -> Option<String> {
    std::fs::read_to_string(std::env::var_os(var)?).ok()
}

/// No-op tools for unit tests.
#[cfg(test)]
pub struct NoExternal;

#[cfg(test)]
impl External for NoExternal {
    fn brain_show(&self, _slug: &str) -> Option<String> {
        None
    }
    /// Never the real `bd`: a unit test runs without `$Q_FIXTURE`.
    fn bd_list(&self, _quest_id: &str) -> Option<String> {
        None
    }
}

/// Brief timestamps are UTC so a brief reads the same on every machine it is
/// handed to; `fmt::stamp` (local) stays for the interactive commands.
fn stamp(ts: i64) -> String {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// `--session` resolution: a session id, a bare `<label>`, or `<quest>/<label>`
/// where `<quest>` must be this Quest's id or slug (another Quest's session is
/// never "me").
pub fn resolve_session<'a>(
    quest: &Quest,
    sessions: &'a [Session],
    target: &str,
) -> Option<&'a Session> {
    let label = match target.split_once('/') {
        Some((q, label)) if q == quest.id || q == quest.slug => label,
        Some(_) => return None,
        None => target,
    };
    // A label is reused across a Quest's life, so a live row wins over the
    // ended one it replaced; an id is unique and matches outright.
    sessions
        .iter()
        .find(|s| s.id == target)
        .or_else(|| {
            sessions
                .iter()
                .find(|s| s.label == label && s.status != SessionStatus::Ended)
        })
        .or_else(|| sessions.iter().find(|s| s.label == label))
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
        .and_then(|s| resolve_session(quest, &sessions, s).cloned());

    let beads = beads::epic_of(quest).map(|_| ext.bd_list(&quest.id));
    let brain = quest
        .brain_session
        .as_deref()
        .and_then(|slug| ext.brain_show(slug));

    // Each trim step tightens one input, cheapest loss first; the loop stops
    // as soon as the rendering fits.
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
        if link_cap > LINKS_CAP {
            link_cap = LINKS_CAP;
        } else if brain.is_some() && brain_cap > 0 {
            brain_cap = if brain_cap > 1_000 { brain_cap / 2 } else { 0 };
        } else if event_cap > 0 {
            event_cap = if event_cap > 5 { event_cap / 2 } else { 0 };
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
    section_how(&mut out, quest, sessions, me, opts);
    section_workflow(&mut out, quest);
    section_beads(&mut out, quest, beads);
    section_sessions(&mut out, sessions);
    section_links(&mut out, links, caps.links);
    section_artifacts(&mut out, links);
    section_brain(&mut out, quest, brain, caps.brain);
    section_events(&mut out, events, caps.events);
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
        ("created", stamp(quest.created_at)),
        ("template", fmt::or_dash(quest.template_id.as_deref())),
    ];
    for (k, v) in rows {
        out.push_str(&format!("- **{k}**: {v}\n"));
    }
    if let Some(ts) = quest.finished_at {
        out.push_str(&format!("- **finished**: {}\n", stamp(ts)));
    }
    out.push('\n');
}

fn section_how(
    out: &mut String,
    quest: &Quest,
    sessions: &[Session],
    me: Option<&Session>,
    opts: &Opts,
) {
    let role = opts.role;
    out.push_str("## 2. How you work here\n\n");
    // A resolved session knows its role better than `--for` / `$Q_ROLE`.
    let role = match me {
        Some(s) => {
            let note = if s.role == role {
                String::new()
            } else {
                format!("; role `{}` was requested, the session's wins", role)
            };
            out.push_str(&format!(
                "You are session `{}` ({}, id `{}`{note}).\n\n",
                s.label, s.role, s.id
            ));
            s.role
        }
        None => {
            if let Some(target) = opts.session.as_deref() {
                out.push_str(&format!("_(session not found: {target})_\n\n"));
            }
            role
        }
    };
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
                 (`--blocker` when stuck). When you are done, leave a closing `q note`.\n\
                 - Lost the picture? `q brief` re-renders this document from the database.\n",
            );
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
    if let Some(epic) = beads::epic_of(quest) {
        let repo = quest.beads_repo.as_deref().unwrap_or("<repo>");
        out.push_str(&format!(
            "- Beads: every issue you open carries `-l repo:{repo},quest:{}` and the \
             epic `{epic}` as parent.\n",
            quest.id
        ));
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
    let Some(epic) = beads::epic_of(quest) else {
        out.push_str("No beads epic linked.\n\n");
        return;
    };
    out.push_str(&format!("- **epic**: `{epic}`"));
    if let Some(repo) = quest.beads_repo.as_deref() {
        out.push_str(&format!(" (repo `{repo}`)"));
    }
    out.push('\n');
    match beads.and_then(|b| b.as_deref()) {
        Some(raw) => render_issues(out, raw, quest, epic),
        None => out.push_str("- `bd list -l quest:<id>` unavailable (bd missing or failed).\n"),
    }
    out.push_str(&format!(
        "- Run `bd prime` when picking up work; list with `bd list -l quest:{}`.\n\n",
        quest.id
    ));
}

/// The counts and the rows both come out of `beads.rs`, off the one payload:
/// the brief used to tally the same JSON its own way and drifted from
/// `q show` — it counted the epic as open work and read `blocked` off a status
/// `bd` never stores. There is one definition of a Quest's progress, and this
/// renders it.
fn render_issues(out: &mut String, raw: &str, quest: &Quest, epic: &str) {
    let Some(rows) = beads::rows(raw, &quest.id, Some(epic)) else {
        out.push_str("- `bd list` output could not be parsed.\n");
        return;
    };
    let progress = beads::count(raw, &quest.id, Some(epic)).unwrap_or_default();
    out.push_str(&format!("- **progress**: {}\n", progress.summary()));
    // Closed work is already in the count; what an agent needs listed is what
    // is left. Grouped by status so the list reads in one order every time.
    let mut by_status: BTreeMap<&str, Vec<&beads::Row>> = BTreeMap::new();
    for row in rows.iter().filter(|r| r.status != "closed") {
        by_status.entry(row.status.as_str()).or_default().push(row);
    }
    for (status, list) in &by_status {
        for row in list {
            let blocked = if row.blocked { " (blocked)" } else { "" };
            out.push_str(&format!(
                "  - [{}] `{}` {}{blocked}\n",
                or_dash(status),
                or_dash(&row.id),
                or_dash(&row.title)
            ));
        }
    }
}

fn or_dash(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
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
        out.push_str("## 8. Brain session\n\n_(no brain session linked)_\n\n");
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

fn section_events(out: &mut String, events: &[Event], cap: usize) {
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
            "\n_(truncated: showing {shown} of the last {} events; run `q events`)_\n",
            events.len()
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
    let text = fmt::oneline(&event_text(e), NOTE_WIDTH);
    format!("{} `{}`{who} {text}", stamp(e.ts), e.kind)
}

/// The human part of a payload: `text` when present, else `k=v` pairs. Kept
/// to one line so a multi-line note cannot break the bullet list.
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

/// The blocker contract for `note` events (what `q note --blocker` writes):
/// the payload is `{"text": "<text>", "blocker": true}`. Nothing else marks a
/// blocker — no `tag`/`tags` fields, no kind of its own. Blockers are not
/// resolvable yet; that is a follow-up.
fn is_blocker(e: &Event) -> bool {
    e.payload
        .as_ref()
        .and_then(|p| p.get("blocker"))
        .and_then(Value::as_bool)
        == Some(true)
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

    #[test]
    fn a_reused_label_resolves_to_the_live_session() {
        let quest = Quest::new("alpha", "/tmp/repo", "laptop");
        let mut ended = Session::new(&quest.id, SessionRole::Worker, "tests", "q-alpha", "%1");
        ended.status = SessionStatus::Ended;
        let mut live = Session::new(&quest.id, SessionRole::Worker, "tests", "q-alpha", "%2");
        live.status = SessionStatus::Busy;
        // Oldest first, as `list_sessions_by_quest` returns them.
        let rows = vec![ended.clone(), live.clone()];

        let id = |target: &str| resolve_session(&quest, &rows, target).map(|s| s.id.clone());
        for target in ["tests", "alpha/tests", &format!("{}/tests", quest.id)] {
            assert_eq!(id(target).as_deref(), Some(live.id.as_str()), "{target}");
        }
        // An id is unique, so it still reaches the ended row.
        assert_eq!(id(&ended.id).as_deref(), Some(ended.id.as_str()));
        // With no live row the history is the best answer there is.
        assert_eq!(
            resolve_session(&quest, std::slice::from_ref(&ended), "tests").map(|s| s.id.as_str()),
            Some(ended.id.as_str())
        );
        assert!(resolve_session(&quest, &rows, "other/tests").is_none());
        assert!(resolve_session(&quest, &rows, "nope").is_none());
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
            &serde_json::json!({ "text": "DB is locked", "blocker": true }),
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
        let label = format!("quest:{}", quest.id);
        let bd = serde_json::json!([
            // The epic itself, and another Quest's issue: neither is this
            // Quest's work, so neither may reach the count or the list.
            { "id": "bd-42", "title": "the epic", "status": "open",
              "issue_type": "epic", "labels": [&label] },
            { "id": "bd-9", "title": "not ours", "status": "open",
              "labels": ["quest:q-somebody-else"] },
            { "id": "bd-1", "title": "done", "status": "closed", "labels": [&label] },
            { "id": "bd-2", "title": "doing", "status": "in_progress", "labels": [&label] },
            { "id": "bd-3", "title": "stuck", "status": "blocked", "labels": [&label] },
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
        // Exactly `Progress::summary()` — the string `q show` prints.
        assert!(
            md.contains("**progress**: 1/3 closed · 1 in progress · 1 blocked"),
            "{md}"
        );
        assert!(md.contains("[blocked] `bd-3` stuck"));
        // Closed work is counted, not listed; the epic and the stranger are
        // neither.
        assert!(!md.contains("[closed] `bd-1`"));
        assert!(!md.contains("`bd-42` the epic"), "{md}");
        assert!(!md.contains("bd-9"), "{md}");
        assert!(md.contains("repo:repo-x,quest:"));
        assert!(md.contains("_(brain note unavailable)_"));
        let worker = Opts {
            role: SessionRole::Worker,
            ..Opts::default()
        };
        let md = render_with(&db, &quest, &worker, &NoExternal).unwrap();
        assert!(
            md.contains("repo:repo-x,quest:"),
            "workers get the rule too"
        );
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

    fn bulk(db: &Db, quest: &Quest, notes: usize, links: usize) {
        for i in 0..notes {
            db.append_event(
                &quest.id,
                None,
                "note",
                &serde_json::json!({ "text": format!("note {i} {}", "x".repeat(200)) }),
            )
            .unwrap();
        }
        for i in 0..links {
            db.insert_link(&Link::new(&quest.id, "url", &format!("https://x/{i}")))
                .unwrap();
        }
    }

    #[test]
    fn size_cap_trims_links_then_brain_then_events() {
        let (db, quest) = seeded();
        bulk(&db, &quest, 40, 30);
        let brain = Canned {
            bd: None,
            brain: Some("b".repeat(5_000)),
        };
        let full = render_with(&db, &quest, &Opts::default(), &brain).unwrap();
        assert!(!full.contains("truncated"), "{full}");

        // Links go first.
        let opts = Opts {
            max_chars: full.chars().count() - 200,
            ..Opts::default()
        };
        let md = render_with(&db, &quest, &opts, &brain).unwrap();
        assert!(md.contains("more links"), "{md}");
        assert!(!md.contains("brain body cut"));
        assert!(!md.contains("truncated: showing"));

        // Then the brain body.
        let opts = Opts {
            max_chars: full.chars().count() - 3_000,
            ..Opts::default()
        };
        let md = render_with(&db, &quest, &opts, &brain).unwrap();
        assert!(
            md.contains("_(truncated: brain body cut for size)_"),
            "{md}"
        );
        assert!(!md.contains("truncated: showing"));

        // Finally events.
        let opts = Opts {
            max_chars: 4_000,
            ..Opts::default()
        };
        let md = render_with(&db, &quest, &opts, &brain).unwrap();
        assert!(md.contains("brain body dropped"));
        assert!(md.contains("truncated: showing"), "{md}");
        assert!(md.contains("of the last 30 events"), "{md}");
        assert!(md.contains("## 10. Open questions / blockers"));
    }

    #[test]
    fn huge_links_do_not_starve_brain_and_events() {
        let (db, quest) = seeded();
        bulk(&db, &quest, 40, 450);
        let brain = Canned {
            bd: None,
            brain: Some("b".repeat(BRAIN_MAX_CHARS)),
        };
        let md = render_with(&db, &quest, &Opts::default(), &brain).unwrap();
        assert!(md.chars().count() <= MAX_CHARS, "{}", md.chars().count());
        assert!(md.contains("more links"), "{md}");
        assert!(md.contains(&"b".repeat(BRAIN_MAX_CHARS)));
        assert!(!md.contains("truncated: brain"));
        assert!(!md.contains("truncated: showing"));
        let events = md
            .split("## 9.")
            .nth(1)
            .unwrap()
            .split("## 10.")
            .next()
            .unwrap();
        assert_eq!(events.lines().filter(|l| l.starts_with("- ")).count(), 30);
    }

    #[test]
    fn events_truncation_counts_what_exists() {
        let (db, quest) = seeded();
        // Two events qualify; ask to show fewer than that.
        let mut out = String::new();
        let events = db.list_events_by_kinds(&quest.id, EVENT_KINDS, 30).unwrap();
        section_events(&mut out, &events, 1);
        assert!(out.contains("showing 1 of the last 3 events"), "{out}");
    }

    #[test]
    fn note_text_is_one_line_and_bounded() {
        let (db, quest) = seeded();
        db.append_event(
            &quest.id,
            None,
            "note",
            &serde_json::json!({ "text": format!("first\nsecond {}", "y".repeat(3_000)) }),
        )
        .unwrap();
        let md = render_with(&db, &quest, &Opts::default(), &NoExternal).unwrap();
        let line = md.lines().find(|l| l.contains("first second")).unwrap();
        assert!(line.starts_with("- "), "{line}");
        assert!(line.chars().count() < NOTE_WIDTH + 60, "{line}");
        assert!(line.ends_with('…'));
    }

    #[test]
    fn brain_header_is_present_without_a_brain_session() {
        let db = Db::open_in_memory().unwrap();
        let quest = db
            .insert_quest(&Quest::new("bare", "/tmp/repo", "laptop"))
            .unwrap();
        let md = render_with(&db, &quest, &Opts::default(), &NoExternal).unwrap();
        assert!(headers(&md).contains(&"## 8. Brain session"), "{md}");
        assert!(md.contains("_(no brain session linked)_"));
    }

    #[test]
    fn session_forms_and_role_precedence() {
        let (db, quest) = seeded();
        let worker = Opts {
            role: SessionRole::Master,
            session: Some(format!("{}/w1-tests", quest.slug)),
            ..Opts::default()
        };
        let md = render_with(&db, &quest, &worker, &NoExternal).unwrap();
        assert!(md.contains("You are session `w1-tests` (worker"), "{md}");
        assert!(md.contains("role `master` was requested, the session's wins"));
        assert!(md.contains("You are a **worker**"));
        assert!(!md.contains("You are the **master**"));

        let by_id = Opts {
            session: Some(format!("{}/w1-tests", quest.id)),
            ..Opts::default()
        };
        let md = render_with(&db, &quest, &by_id, &NoExternal).unwrap();
        assert!(md.contains("You are session `w1-tests`"), "{md}");

        for bogus in ["nope", "other-quest/w1-tests"] {
            let opts = Opts {
                session: Some(bogus.to_string()),
                ..Opts::default()
            };
            let md = render_with(&db, &quest, &opts, &NoExternal).unwrap();
            assert!(
                md.contains(&format!("_(session not found: {bogus})_")),
                "{md}"
            );
            assert!(md.contains("You are the **master**"));
        }
    }

    #[test]
    fn timestamps_are_utc() {
        assert_eq!(stamp(0), "1970-01-01 00:00 UTC");
        let (db, quest) = seeded();
        let md = render_with(&db, &quest, &Opts::default(), &NoExternal).unwrap();
        assert!(md.contains(" UTC `note`"), "{md}");
    }

    #[test]
    fn unparseable_bd_output_is_said_to_be_unparseable() {
        let (db, quest) = seeded();
        let md = render_with(
            &db,
            &quest,
            &Opts::default(),
            &Canned {
                bd: Some("nope".to_string()),
                brain: None,
            },
        )
        .unwrap();
        assert!(md.contains("could not be parsed"), "{md}");
    }
}
