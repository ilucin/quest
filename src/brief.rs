//! The Quest brief (SPEC §9): deterministic markdown built from the database,
//! shared by `q brief`, SessionStart hook injection, `q reset`, `q resume`
//! and handoff. Sections 1–10 in spec order; the size cap first caps links,
//! then halves the brain body, then the events. Beads, sessions and the
//! workflow (section 3, SPEC §11) are never trimmed, which is why `MAX_CHARS`
//! leaves headroom under the 32k budget — and why `WORKFLOW_MAX_CHARS` bounds
//! what a workflow may cost before it eats that headroom.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::beads;
use crate::commands::fmt;
use crate::db::Db;
use crate::error::QError;
use crate::model::{Event, Link, Quest, Session, SessionRole, SessionStatus, display_state};
use crate::proc;
use crate::workflows::{Fences, Part, Registry};

/// Default for section 9.
pub const DEFAULT_EVENTS: usize = 30;
/// ~6k tokens (SPEC §23 #4). Beads and sessions are never trimmed, so this
/// stays well under the 32k budget the hook injection has to fit.
pub const MAX_CHARS: usize = 24_000;
/// Cap on the brain body before the global cap even applies.
const BRAIN_MAX_CHARS: usize = 8_000;
/// What one workflow may cost a brief. Section 3 is never trimmed — a master
/// half-told how to work is worse than one told nothing — so the budget is
/// enforced on the *workflow* instead: every built-in is asserted under it, and
/// a user file over it is rendered with its tail cut and said so.
pub const WORKFLOW_MAX_CHARS: usize = 8_000;
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
    /// Where section 3's markdown comes from (SPEC §11). Defaults to the
    /// built-ins alone, so a caller that forgets it renders a brief that is
    /// merely incomplete rather than one that reads the developer's own
    /// `~/.config/q/workflows`.
    pub workflows: Registry,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            role: SessionRole::Master,
            session: None,
            events: DEFAULT_EVENTS,
            max_chars: MAX_CHARS,
            workflows: Registry::builtin_only(),
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
    /// `brain show <slug>`; `None` when `brain` is missing or failed.
    fn brain_show(&self, slug: &str) -> Option<String>;

    /// The Quest's issues, always through `beads.rs` — the very call `q show`
    /// makes, so the brief cannot show a shorter or differently filtered list.
    /// `$Q_FIXTURE` is honoured in there, not here.
    ///
    /// Required rather than defaulted: the default was `beads::client()`, a
    /// client discovered off the process environment that writes its progress
    /// notices to stderr — which is a torn frame when the caller is the TUI,
    /// and the real `bd` when the caller is a unit test (N-4). Every impl now
    /// has to say where its `bd` comes from.
    fn bd_list(&self, quest_id: &str) -> Option<String>;
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
    fn bd_list(&self, quest_id: &str) -> Option<String> {
        beads::client().list_quest(quest_id)
    }
}

struct FixtureExternal;

impl External for FixtureExternal {
    fn brain_show(&self, _slug: &str) -> Option<String> {
        fixture_file("Q_FIXTURE_BRAIN")
    }
    fn bd_list(&self, quest_id: &str) -> Option<String> {
        beads::client().list_quest(quest_id)
    }
}

/// The tools a caller that already owns a `bd` uses: `brain` as usual (or its
/// fixture), and issues through the caller's client rather than one discovered
/// here. The TUI's `b` goes through this — `Ctx` holds a quiet client, and a
/// notice written from inside the call would land on the alternate screen.
pub struct WithBd<'a> {
    bd: &'a dyn beads::Bd,
    brain: Box<dyn External>,
}

impl<'a> WithBd<'a> {
    pub fn new(bd: &'a dyn beads::Bd) -> WithBd<'a> {
        WithBd {
            bd,
            brain: external(),
        }
    }
}

impl External for WithBd<'_> {
    fn brain_show(&self, slug: &str) -> Option<String> {
        self.brain.brain_show(slug)
    }
    fn bd_list(&self, quest_id: &str) -> Option<String> {
        self.bd.list_quest(quest_id)
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

    // The **name**, because the master reading this brief has no way to look
    // `t-f5b1` up mid-turn. The id stays alongside it for `q tpl show`.
    let template = match quest.template_id.as_deref() {
        None => "-".to_string(),
        Some(id) => match db.get_template(id)? {
            Some(t) => format!("{} ({})", t.name, t.id),
            None => id.to_string(),
        },
    };
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
            &template,
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
    template: &str,
    caps: Caps,
) -> String {
    let mut out = String::new();
    // The role the brief is actually written for: a resolved session's own
    // role wins over `--for`/`$Q_ROLE`. Computed once and handed to both
    // sections that obey it, so they cannot disagree about who is reading.
    let role = effective_role(me, opts.role);
    section_quest(&mut out, quest, sessions, template);
    section_how(&mut out, quest, sessions, me, opts, role);
    section_workflow(
        &mut out,
        effective_workflow(me, quest),
        role,
        &opts.workflows,
    );
    section_beads(&mut out, quest, beads);
    section_sessions(&mut out, sessions);
    section_links(&mut out, links, caps.links);
    section_artifacts(&mut out, links);
    section_brain(&mut out, quest, brain, caps.brain);
    section_events(&mut out, events, caps.events);
    section_blockers(&mut out, notes, sessions);
    out
}

fn section_quest(out: &mut String, quest: &Quest, sessions: &[Session], template: &str) {
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
        (
            "workflow",
            // Whitespace-only is unset here too, as it is for section 3.
            fmt::or_dash(
                quest
                    .workflow
                    .as_deref()
                    .map(str::trim)
                    .filter(|w| !w.is_empty()),
            ),
        ),
        ("created", stamp(quest.created_at)),
        ("template", template.to_string()),
    ];
    for (k, v) in rows {
        out.push_str(&format!("- **{k}**: {v}\n"));
    }
    if let Some(ts) = quest.finished_at {
        out.push_str(&format!("- **finished**: {}\n", stamp(ts)));
    }
    out.push('\n');
}

/// Whose instructions the brief carries: the resolved session's role when
/// there is one, else what was asked for. Section 2 says this out loud and
/// section 3 obeys the same answer — both are handed this one result.
fn effective_role(me: Option<&Session>, requested: SessionRole) -> SessionRole {
    me.map_or(requested, |s| s.role)
}

/// Which workflow section 3 renders: the session's own when the brief is for
/// one, else the Quest's. `q spawn --workflow`'s help promises "default: the
/// Quest's", the session row carries it, and this is where that promise is
/// kept.
///
/// A blank or whitespace-only column is no workflow at all — the same thing
/// `q set <quest> workflow ""` means, and the shape a row written before the
/// `--workflow` flag trimmed what it stored can still be in.
fn effective_workflow<'a>(me: Option<&'a Session>, quest: &'a Quest) -> Option<&'a str> {
    fn set(column: &Option<String>) -> Option<&str> {
        column.as_deref().map(str::trim).filter(|w| !w.is_empty())
    }
    me.and_then(|s| set(&s.workflow))
        .or_else(|| set(&quest.workflow))
}

/// `role` is [`effective_role`]'s answer, passed in rather than recomputed:
/// section 2 and section 3 must never disagree about who is reading, and the
/// only way to guarantee that is for there to be one call.
fn section_how(
    out: &mut String,
    quest: &Quest,
    sessions: &[Session],
    me: Option<&Session>,
    opts: &Opts,
    role: SessionRole,
) {
    out.push_str("## 2. How you work here\n\n");
    // A resolved session knows its role better than `--for` / `$Q_ROLE`.
    match me {
        Some(s) => {
            let note = if s.role == opts.role {
                String::new()
            } else {
                format!("; role `{}` was requested, the session's wins", opts.role)
            };
            out.push_str(&format!(
                "You are session `{}` ({}, id `{}`{note}).\n\n",
                s.label, s.role, s.id
            ));
        }
        None => {
            if let Some(target) = opts.session.as_deref() {
                out.push_str(&format!("_(session not found: {target})_\n\n"));
            }
        }
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
                 (`--blocker` when stuck). When you are done, leave a closing `q note`.\n\
                 - Lost the picture? `q brief` re-renders this document from the database.\n",
            );
            // A Quest made with the TUI's bare `n` starts with neither; the
            // master fills them in once it knows what the work is.
            if quest.goal.as_deref().is_none_or(|g| g.trim().is_empty()) {
                out.push_str(&format!(
                    "- This Quest has no goal yet. Once you know what it is, record it in one \
                     line: `q set {} goal \"<text>\"`.\n",
                    quest.id
                ));
            }
            if beads::epic_of(quest).is_none() {
                out.push_str(&format!(
                    "- No beads epic yet. When the work is worth tracking, \
                     `q set {} beads_epic new` creates one.\n",
                    quest.id
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

/// SPEC §9 section 3, SPEC §11: the workflow markdown itself, not its name.
///
/// The master gets the whole file. A worker gets the file's `## worker`
/// section when it defines one, and the whole file — said out loud — when it
/// does not; see [`crate::workflows`] for why that is the fallback rather than
/// nothing.
///
/// Headings inside the file are demoted by two levels before they go in. A
/// workflow opens with `# <name>` and uses `## …` for its own sections, and
/// pasted verbatim those would sit alongside the brief's own `## 1.`…`## 10.`
/// and break the one structure every reader (and every test) navigates by.
///
/// `name` is [`effective_workflow`]'s answer: a worker spawned with its own
/// `--workflow` reads that one, not its master's.
///
/// A workflow that cannot be read is a **stated** failure, never a silent one:
/// a master handed a brief with an empty section 3 has no way to tell "no
/// workflow" from "your workflow is missing", and would improvise.
fn section_workflow(out: &mut String, name: Option<&str>, role: SessionRole, registry: &Registry) {
    out.push_str("## 3. Workflow\n\n");
    let Some(name) = name else {
        out.push_str(
            "No workflow set. Ask how the human wants this run, or pick one: \
             `q workflow list`, then `q workflow set <quest> <name>`.\n\n",
        );
        return;
    };
    let workflow = match registry.get(name) {
        Ok(workflow) => workflow,
        Err(e) => {
            out.push_str(&format!(
                "Workflow `{name}` is set but could not be read: {e:#}\n\n\
                 Say so rather than guessing — `q workflow list` shows what exists.\n\n"
            ));
            return;
        }
    };
    let part = workflow.for_role(role);
    out.push_str(&format!(
        "Workflow **`{name}`** ({}). This is how you work here; it outranks your habits.\n\n",
        workflow.source
    ));
    if let Part::WholeForWorker(_) = part {
        out.push_str(
            "_(this workflow defines no `## worker` section, so you are reading the \
             master's copy: take from it what applies to your scope, and leave the \
             orchestration to your master)_\n\n",
        );
    }
    let body = part.text().trim();
    if body.is_empty() {
        out.push_str(match part {
            Part::Worker(_) => "_(the workflow's worker section is empty)_\n\n",
            _ => "_(the workflow file is empty)_\n\n",
        });
        return;
    }
    let demoted = demote(body);
    if demoted.chars().count() > WORKFLOW_MAX_CHARS {
        out.push_str(&truncate_markdown(&demoted, WORKFLOW_MAX_CHARS));
        out.push_str(&format!(
            "\n\n_(truncated: this workflow is longer than the brief's \
             {WORKFLOW_MAX_CHARS}-character budget; read the rest with \
             `q workflow show {name}`)_\n\n"
        ));
        return;
    }
    // A workflow *file* whose own fence is never closed at EOF would otherwise
    // push an open fence into the brief and render the truncation notice and
    // sections 4–10 as code — the sole cause of every outline break the test
    // fuzz found. The truncation path already closes it; the ordinary path
    // must too, through the same [`Fences`] answer (D1).
    out.push_str(&demoted);
    if let Some(closer) = open_fence_closer(&demoted) {
        out.push('\n');
        out.push_str(&closer);
    }
    out.push_str("\n\n");
}

/// The delimiter that closes any code fence `text` leaves open at its end, else
/// `None` — the one place the brief asks "is a fence still open here", so the
/// truncated and the whole-file render paths answer it the same way.
fn open_fence_closer(text: &str) -> Option<String> {
    let mut fences = Fences::default();
    for line in text.lines() {
        fences.feed(line);
    }
    fences.closer()
}

/// `text` cut to at most `max` characters, at a line boundary, with any code
/// fence the cut left open closed again.
///
/// Both halves matter: a cut mid-line can land inside a fence *delimiter*, and
/// a fence left open swallows the truncation notice and every brief section
/// after it into a code block. The "is a fence open" question is [`Fences`]'s,
/// the same one `demote` and `workflows::worker_section` ask.
fn truncate_markdown(text: &str, max: usize) -> String {
    let cut: String = text.chars().take(max).collect();
    let kept = match cut.rfind('\n') {
        // Cut back to the last line boundary so no half-written line — a split
        // fence delimiter included — is left. But markdown is commonly one
        // paragraph per line: when the last newline is near the very start, a
        // one-line body would truncate to nothing (D3), so keep the
        // char-boundary cut and let the fence-closer below tidy it.
        Some(i) if i > max / 2 => &cut[..i],
        _ => cut.as_str(),
    };
    let mut out = kept.trim_end().to_string();
    if let Some(closer) = open_fence_closer(&out) {
        out.push('\n');
        out.push_str(&closer);
    }
    out
}

/// Every ATX heading pushed down two levels, so a workflow's `#`/`##` become
/// `###`/`####` and cannot be mistaken for one of the brief's own sections.
/// Deeper headings are left alone once they reach `######`, which is as far as
/// markdown goes. Fenced code blocks are skipped: `#` inside one is a comment,
/// not a heading — and which lines those are is [`Fences`]'s answer, the same
/// one `workflows::worker_section` reads the file by.
fn demote(body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 32);
    let mut fences = Fences::default();
    for line in body.lines() {
        let trimmed = line.trim_start();
        // A fenced-code line, or one indented four or more (an ATX heading
        // carries at most three — CommonMark), is never a heading: counting its
        // hashes and stripping its indent would lift a `## worker` out of the
        // code block it sits in (D2).
        let hashes = if fences.feed(line) || crate::workflows::line_indent(line) > 3 {
            0
        } else {
            trimmed.bytes().take_while(|b| *b == b'#').count()
        };
        // `#foo` is not a heading; a heading's hashes are followed by a space
        // (or are the whole line).
        let is_heading = (1..=6).contains(&hashes)
            && trimmed[hashes..]
                .chars()
                .next()
                .is_none_or(|c| c == ' ' || c == '\t');
        if is_heading {
            out.push_str(&"#".repeat((hashes + 2).min(6)));
            out.push_str(&trimmed[hashes..]);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out.trim_end().to_string()
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
/// blocker — no `tag`/`tags` fields, no kind of its own.
fn is_blocker(e: &Event) -> bool {
    e.payload
        .as_ref()
        .and_then(|p| p.get("blocker"))
        .and_then(Value::as_bool)
        == Some(true)
}

/// The event id a resolution note points at (`q note --resolve <id>` writes a
/// `note` event with payload `{"resolves": <id>}`), or `None` for any other
/// note.
fn resolves_id(e: &Event) -> Option<i64> {
    e.payload
        .as_ref()
        .and_then(|p| p.get("resolves"))
        .and_then(Value::as_i64)
}

fn section_blockers(out: &mut String, notes: &[Event], sessions: &[Session]) {
    out.push_str("## 10. Open questions / blockers\n\n");
    let resolved: std::collections::HashSet<i64> = notes.iter().filter_map(resolves_id).collect();
    let blockers: Vec<&Event> = notes
        .iter()
        .filter(|e| is_blocker(e) && !resolved.contains(&e.id))
        .collect();
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
    fn a_resolved_blocker_drops_out_of_section_ten() {
        let (db, quest) = seeded();
        // The seed's blocker.
        let blocker = db
            .list_events_by_kinds(&quest.id, &["note"], usize::MAX)
            .unwrap()
            .into_iter()
            .find(is_blocker)
            .unwrap();
        // A second, unresolved blocker that must survive.
        db.append_event(
            &quest.id,
            None,
            "note",
            &serde_json::json!({ "text": "waiting on review", "blocker": true }),
        )
        .unwrap();

        let before = render_with(&db, &quest, &Opts::default(), &NoExternal).unwrap();
        let section = |md: &str| md.split("## 10.").nth(1).unwrap().to_string();
        assert!(section(&before).contains("DB is locked"));
        assert!(section(&before).contains("waiting on review"));

        db.append_event(
            &quest.id,
            None,
            "note",
            &serde_json::json!({ "resolves": blocker.id }),
        )
        .unwrap();

        let after = section(&render_with(&db, &quest, &Opts::default(), &NoExternal).unwrap());
        assert!(!after.contains("DB is locked"), "{after}");
        assert!(after.contains("waiting on review"), "{after}");
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

    // ------------------------------------------------- section 3 (SPEC §11)

    /// A Quest with `workflow` set, and a registry over `dir`.
    fn with_workflow(name: &str, dir: &std::path::Path) -> (Db, Quest, Opts) {
        let (db, quest) = seeded();
        let quest = db
            .update_quest(
                &quest.id,
                &crate::db::quest::QuestPatch {
                    workflow: Some(Some(name.to_string())),
                    ..Default::default()
                },
            )
            .unwrap();
        let opts = Opts {
            workflows: Registry::new(dir),
            ..Opts::default()
        };
        (db, quest, opts)
    }

    fn section3(md: &str) -> &str {
        md.split("## 3. Workflow")
            .nth(1)
            .expect("no section 3")
            .split("\n## 4.")
            .next()
            .unwrap()
    }

    #[test]
    fn no_workflow_says_so_and_says_what_to_do_about_it() {
        let (db, quest) = seeded();
        let md = render_with(&db, &quest, &Opts::default(), &NoExternal).unwrap();
        let body = section3(&md);
        assert!(body.contains("No workflow set"), "{body}");
        assert!(body.contains("q workflow list"), "{body}");
    }

    #[test]
    fn a_masters_section_three_is_the_whole_workflow_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let (db, quest, opts) = with_workflow("orchestrator", dir.path());
        let md = render_with(&db, &quest, &opts, &NoExternal).unwrap();
        let body = section3(&md);
        assert!(
            body.contains("Workflow **`orchestrator`** (builtin)"),
            "{body}"
        );
        // The content, not the name — this is the bead's whole point.
        assert!(body.contains("q spawn"), "{body}");
        assert!(body.contains("plan-review"), "{body}");
        // Including the worker half, which the master needs to brief workers.
        assert!(
            body.contains("Do **only** the stage you were given"),
            "{body}"
        );
    }

    #[test]
    fn a_worker_gets_only_the_worker_section() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path());
        registry
            .write(
                "split",
                "# split\n\nMASTER-ONLY-TEXT\n\n## worker\n\nWORKER-ONLY-TEXT\n",
            )
            .unwrap();
        let (db, quest, mut opts) = with_workflow("split", dir.path());

        let master = render_with(&db, &quest, &opts, &NoExternal).unwrap();
        assert!(section3(&master).contains("MASTER-ONLY-TEXT"));
        assert!(section3(&master).contains("WORKER-ONLY-TEXT"));

        opts.role = SessionRole::Worker;
        let worker = render_with(&db, &quest, &opts, &NoExternal).unwrap();
        let body = section3(&worker);
        assert!(body.contains("WORKER-ONLY-TEXT"), "{body}");
        assert!(!body.contains("MASTER-ONLY-TEXT"), "{body}");
        assert!(!body.contains("no `## worker` section"), "{body}");

        // The session's own role wins over `--for`, exactly as section 2 says.
        let by_session = Opts {
            role: SessionRole::Master,
            session: Some("w1-tests".to_string()),
            ..opts.clone()
        };
        let md = render_with(&db, &quest, &by_session, &NoExternal).unwrap();
        assert!(section3(&md).contains("WORKER-ONLY-TEXT"), "{md}");
        assert!(!section3(&md).contains("MASTER-ONLY-TEXT"), "{md}");
    }

    #[test]
    fn a_worker_with_no_worker_section_gets_the_whole_file_and_is_told_so() {
        let dir = tempfile::tempdir().unwrap();
        Registry::new(dir.path())
            .write("flat", "# flat\n\nEVERYTHING\n")
            .unwrap();
        let (db, quest, opts) = with_workflow("flat", dir.path());
        let worker = Opts {
            role: SessionRole::Worker,
            ..opts
        };
        let body = section3(&render_with(&db, &quest, &worker, &NoExternal).unwrap()).to_string();
        assert!(body.contains("EVERYTHING"), "{body}");
        assert!(body.contains("defines no `## worker` section"), "{body}");
        assert!(
            body.contains("leave the orchestration to your master"),
            "{body}"
        );
    }

    #[test]
    fn a_workflow_that_is_gone_is_said_out_loud_rather_than_left_blank() {
        let dir = tempfile::tempdir().unwrap();
        let (db, quest, opts) = with_workflow("vanished", dir.path());
        let md = render_with(&db, &quest, &opts, &NoExternal).unwrap();
        let body = section3(&md);
        assert!(body.contains("could not be read"), "{body}");
        assert!(body.contains("unknown workflow `vanished`"), "{body}");
        assert!(body.contains("orchestrator"), "the list is offered: {body}");
        // Section 1 still reports the name, and the brief is still a brief.
        assert!(md.contains("**workflow**: vanished"), "{md}");
        assert!(md.contains("## 10. Open questions / blockers"), "{md}");
    }

    #[test]
    fn a_user_file_shadows_the_builtin_in_the_brief_too() {
        let dir = tempfile::tempdir().unwrap();
        Registry::new(dir.path())
            .write("solo", "# solo\n\nMY OWN SOLO\n")
            .unwrap();
        let (db, quest, opts) = with_workflow("solo", dir.path());
        let body = section3(&render_with(&db, &quest, &opts, &NoExternal).unwrap()).to_string();
        assert!(body.contains("MY OWN SOLO"), "{body}");
        assert!(
            body.contains("**`solo`** (user (shadows builtin))"),
            "{body}"
        );
        assert!(!body.contains("One master, no workers"), "{body}");
    }

    /// Section 3 must not be able to forge a section header. A workflow's own
    /// `## Gates` pasted verbatim would sit among `## 1.`…`## 10.`.
    #[test]
    fn a_workflows_headings_are_demoted_so_they_cannot_pose_as_brief_sections() {
        let dir = tempfile::tempdir().unwrap();
        Registry::new(dir.path())
            .write(
                "deep",
                "# deep\n\n## Gates\n\ntext\n\n### Detail\n\n###### Six\n\n\
                 ```\n# not a heading\n## nor this\n```\n\n#hash\n",
            )
            .unwrap();
        let (db, quest, opts) = with_workflow("deep", dir.path());
        let md = render_with(&db, &quest, &opts, &NoExternal).unwrap();
        // Fence-aware, unlike `headers`: the fixture deliberately puts a `##`
        // *inside* a code fence, which is content and must survive untouched.
        let outline: Vec<&str> = {
            let mut fenced = false;
            md.lines()
                .filter(|line| {
                    if line.trim_start().starts_with("```") {
                        fenced = !fenced;
                        return false;
                    }
                    !fenced && line.starts_with("## ")
                })
                .collect()
        };
        assert_eq!(
            outline,
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
            ],
            "a workflow heading reached the brief's own outline:\n{md}"
        );
        let body = section3(&md);
        assert!(body.contains("### deep"), "{body}");
        assert!(body.contains("#### Gates"), "{body}");
        assert!(body.contains("##### Detail"), "{body}");
        assert!(
            body.contains("###### Six"),
            "six is as deep as it goes: {body}"
        );
        // Inside a fence, `#` is content.
        assert!(body.contains("\n# not a heading"), "{body}");
        assert!(body.contains("\n## nor this"), "{body}");
        // `#hash` was never a heading.
        assert!(body.contains("\n#hash"), "{body}");
    }

    #[test]
    fn every_builtin_fits_the_briefs_workflow_budget() {
        for (name, body) in crate::workflows::BUILTIN {
            // The *demoted* string is what section 3 truncates — two characters
            // longer per heading than the raw file — so that is what has to
            // fit, for the master's whole file and the worker's section alike.
            for (part, text) in [
                ("whole", demote(body.trim())),
                (
                    "worker",
                    demote(crate::workflows::worker_section(body).unwrap_or("").trim()),
                ),
            ] {
                assert!(
                    text.chars().count() <= WORKFLOW_MAX_CHARS,
                    "{name} ({part}) is {} chars, over the {WORKFLOW_MAX_CHARS} budget",
                    text.chars().count()
                );
            }
        }
    }

    /// The brief's outline, read the way a reader does: a `##` inside a code
    /// fence is content.
    fn outline(md: &str) -> Vec<&str> {
        let mut fences = Fences::default();
        md.lines()
            .filter(|line| !fences.feed(line) && line.starts_with("## "))
            .collect()
    }

    const BRIEF_OUTLINE: [&str; 10] = [
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
    ];

    /// `demote` used to toggle a bool on any ``` or `~~~`, so a nested or
    /// mismatched fence inverted it and every heading after it went in
    /// verbatim — two `## 4. Beads` in one brief.
    #[test]
    fn a_nested_or_mismatched_fence_does_not_let_a_workflows_headings_out() {
        for (name, body) in [
            // A ``` nested inside a longer ```` fence.
            (
                "nested",
                "# nested\n\nExample:\n\n````\nq note \"x\"\n```\n````\n\n## 4. Beads\n\nnot a brief section.\n",
            ),
            // A ``` inside a ~~~ block.
            (
                "tilde",
                "# tilde\n\n~~~\ncode\n```\nmore code\n~~~\n\n## 4. Beads\n\nnot a brief section.\n",
            ),
            // A closing fence carries no info string, so this one stays open.
            (
                "info",
                "# info\n\n```\ncode\n``` rust\ncode\n```\n\n## 4. Beads\n\nnot a brief section.\n",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            Registry::new(dir.path()).write(name, body).unwrap();
            let (db, quest, opts) = with_workflow(name, dir.path());
            let md = render_with(&db, &quest, &opts, &NoExternal).unwrap();
            assert_eq!(outline(&md), BRIEF_OUTLINE, "{name}:\n{md}");
            assert_eq!(
                md.lines().filter(|l| *l == "## 4. Beads").count(),
                1,
                "{name}: the workflow's heading posed as a brief section:\n{md}"
            );
            assert!(
                section3(&md).contains("#### 4. Beads"),
                "{name}: demoted, not dropped:\n{md}"
            );
        }
    }

    /// A cut inside an open fence turned the truncation notice and sections
    /// 4–10 into code.
    #[test]
    fn truncation_never_leaves_a_code_fence_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::from("# big\n\nintro\n\n```bash\n");
        for i in 0..1_200 {
            body.push_str(&format!("echo line {i}\n"));
        }
        body.push_str("```\n\nTAIL\n");
        assert!(body.chars().count() > WORKFLOW_MAX_CHARS * 2);
        Registry::new(dir.path()).write("big", &body).unwrap();
        let (db, quest, opts) = with_workflow("big", dir.path());
        let md = render_with(&db, &quest, &opts, &NoExternal).unwrap();

        assert!(!md.contains("TAIL"), "the tail survived the cut:\n{md}");
        let count = md
            .lines()
            .filter(|l| l.trim_start().starts_with("```"))
            .count();
        assert_eq!(
            count % 2,
            0,
            "an odd number of fence lines ({count}):\n{md}"
        );
        let mut fences = Fences::default();
        for line in md.lines() {
            fences.feed(line);
        }
        assert_eq!(fences.closer(), None, "a fence was left open:\n{md}");
        // The notice and every section after 3 are prose, not code.
        assert_eq!(outline(&md), BRIEF_OUTLINE, "{md}");
        assert!(section3(&md).contains("_(truncated:"), "{md}");
        // The cut lands on a line boundary, so no half-written line is left.
        assert!(!md.contains("echo line 1\n```\n\necho"), "{md}");
    }

    /// D1: a workflow *file* whose fence is never closed at EOF — under budget,
    /// so the truncation path is not what saves it — must still not push an open
    /// fence into the brief and render sections 4–10 as code.
    #[test]
    fn an_unclosed_fence_in_the_file_does_not_swallow_the_later_sections() {
        let dir = tempfile::tempdir().unwrap();
        let body = "# f11\n\nMASTER\n\n## worker\n\nREAL\n\n```\nunterminated\n";
        Registry::new(dir.path()).write("f11", body).unwrap();
        let (db, quest, opts) = with_workflow("f11", dir.path());
        let md = render_with(&db, &quest, &opts, &NoExternal).unwrap();

        assert_eq!(
            outline(&md),
            BRIEF_OUTLINE,
            "sections rendered as code:\n{md}"
        );
        let fences = md
            .lines()
            .filter(|l| l.trim_start().starts_with("```"))
            .count();
        assert_eq!(
            fences % 2,
            0,
            "an odd number of fence lines ({fences}):\n{md}"
        );
        assert_eq!(open_fence_closer(&md), None, "a fence was left open:\n{md}");
    }

    /// D3: an overlong body written as a single line — a heading then one huge
    /// paragraph — used to truncate to nothing, because the only newline is at
    /// the very top and cutting back to it dropped every character of content.
    #[test]
    fn a_single_line_body_over_budget_keeps_its_content() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("# t\n\n{}\n", "x ".repeat(WORKFLOW_MAX_CHARS));
        Registry::new(dir.path()).write("t", &body).unwrap();
        let (db, quest, opts) = with_workflow("t", dir.path());
        let md = render_with(&db, &quest, &opts, &NoExternal).unwrap();
        let body3 = section3(&md);
        assert!(
            body3.contains("q workflow show t"),
            "no truncation note:\n{body3}"
        );
        assert!(
            body3.matches('x').count() > 1_000,
            "the single line was dropped, leaving no content:\n{body3}"
        );
        assert_eq!(outline(&md), BRIEF_OUTLINE, "{md}");
    }

    /// D2 end to end: a `## worker` inside an indented code block must not leak
    /// the master's prose into a worker's brief, nor cost the worker its real
    /// section — the same double failure the fenced case was fixed for.
    #[test]
    fn an_indented_worker_heading_does_not_leak_into_a_workers_brief() {
        let dir = tempfile::tempdir().unwrap();
        let body = concat!(
            "# f07b\n\n",
            "MASTER-SECRET\n\n",
            "    ```\n",
            "    ## worker\n",
            "    FAKE-4SP\n",
            "    ```\n\n",
            "MASTER-SECOND\n\n",
            "## worker\n\n",
            "REAL-WORKER-TEXT\n",
        );
        Registry::new(dir.path()).write("f07b", body).unwrap();
        let (db, quest, opts) = with_workflow("f07b", dir.path());
        let worker = Opts {
            role: SessionRole::Worker,
            ..opts
        };
        let md = render_with(&db, &quest, &worker, &NoExternal).unwrap();
        let body3 = section3(&md);
        assert!(
            body3.contains("REAL-WORKER-TEXT"),
            "the worker lost its section:\n{body3}"
        );
        assert!(
            !body3.contains("MASTER-SECOND") && !body3.contains("MASTER-SECRET"),
            "master-only prose leaked to the worker:\n{body3}"
        );
    }

    /// D5: with the master's session workflow left unset (it is not snapshotted
    /// at `q new` — see `commands::new`), the master reads the Quest's, so a
    /// `q workflow set` that changes the Quest changes the master's own brief.
    #[test]
    fn a_master_with_no_session_workflow_follows_the_quest() {
        let dir = tempfile::tempdir().unwrap();
        Registry::new(dir.path())
            .write("orchestrator", "# orchestrator\n\nORCH-BODY\n")
            .unwrap();
        Registry::new(dir.path())
            .write("solo", "# solo\n\nSOLO-BODY\n")
            .unwrap();
        let (db, quest, opts) = with_workflow("orchestrator", dir.path());
        // The master's session row carries no workflow of its own.
        let by_master = Opts {
            session: Some("master".to_string()),
            ..opts.clone()
        };
        let md = render_with(&db, &quest, &by_master, &NoExternal).unwrap();
        assert!(section3(&md).contains("ORCH-BODY"), "{md}");

        // `q workflow set` changes the Quest; the master's brief follows.
        let quest = db
            .update_quest(
                &quest.id,
                &crate::db::quest::QuestPatch {
                    workflow: Some(Some("solo".to_string())),
                    ..Default::default()
                },
            )
            .unwrap();
        let md = render_with(&db, &quest, &by_master, &NoExternal).unwrap();
        assert!(
            section3(&md).contains("SOLO-BODY") && !section3(&md).contains("ORCH-BODY"),
            "the master read a stale workflow:\n{md}"
        );
    }

    /// SPEC §11 and `q spawn --workflow`'s own help ("default: the Quest's"):
    /// a worker with its own workflow reads that one, not its master's.
    #[test]
    fn a_worker_with_its_own_workflow_reads_that_one_not_the_quests() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path());
        registry
            .write("mine", "# mine\n\nMASTERS\n\n## worker\n\nMASTERS-WORKER\n")
            .unwrap();
        registry
            .write("hers", "# hers\n\nHERS\n\n## worker\n\nHERS-WORKER\n")
            .unwrap();
        let (db, quest, opts) = with_workflow("mine", dir.path());

        let mut own = Session::new(&quest.id, SessionRole::Worker, "w2-own", "q-alpha", "%4");
        own.workflow = Some("hers".to_string());
        db.insert_session(&own).unwrap();

        let by_session = Opts {
            session: Some("w2-own".to_string()),
            ..opts.clone()
        };
        let body =
            section3(&render_with(&db, &quest, &by_session, &NoExternal).unwrap()).to_string();
        assert!(body.contains("`hers`"), "{body}");
        assert!(body.contains("HERS-WORKER"), "{body}");
        assert!(!body.contains("MASTERS"), "{body}");

        // A worker with no workflow of its own still reads the Quest's.
        let inherited = Opts {
            session: Some("w1-tests".to_string()),
            ..opts.clone()
        };
        let body =
            section3(&render_with(&db, &quest, &inherited, &NoExternal).unwrap()).to_string();
        assert!(body.contains("MASTERS-WORKER"), "{body}");

        // And the Quest's own brief is unaffected.
        let body = section3(&render_with(&db, &quest, &opts, &NoExternal).unwrap()).to_string();
        assert!(
            body.contains("`mine`") && body.contains("MASTERS"),
            "{body}"
        );
    }

    /// A whitespace-only column is unset, not a workflow whose name is spaces —
    /// the shape a row written before `--workflow` trimmed can still be in.
    #[test]
    fn a_whitespace_only_workflow_column_is_no_workflow_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let (db, quest, opts) = with_workflow("   ", dir.path());
        let md = render_with(&db, &quest, &opts, &NoExternal).unwrap();
        assert!(section3(&md).contains("No workflow set"), "{md}");
        assert!(md.contains("- **workflow**: -"), "{md}");
    }

    #[test]
    fn an_empty_workflow_says_which_half_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        // `write` refuses an empty body, so this is a file put there by hand.
        std::fs::write(dir.path().join("blank.md"), "\n\n").unwrap();
        let (db, quest, opts) = with_workflow("blank", dir.path());
        let md = render_with(&db, &quest, &opts, &NoExternal).unwrap();
        assert!(section3(&md).contains("the workflow file is empty"), "{md}");

        std::fs::write(
            dir.path().join("half.md"),
            "# half\n\nmaster text\n\n## worker\n",
        )
        .unwrap();
        let (db, quest, opts) = with_workflow("half", dir.path());
        let worker = Opts {
            role: SessionRole::Worker,
            ..opts
        };
        let md = render_with(&db, &quest, &worker, &NoExternal).unwrap();
        assert!(
            section3(&md).contains("the workflow's worker section is empty"),
            "{md}"
        );
    }

    #[test]
    fn a_workflow_over_the_budget_is_cut_and_says_where_the_rest_is() {
        let dir = tempfile::tempdir().unwrap();
        Registry::new(dir.path())
            .write(
                "huge",
                &format!("# huge\n\n{}\nTAIL\n", "x".repeat(WORKFLOW_MAX_CHARS)),
            )
            .unwrap();
        let (db, quest, opts) = with_workflow("huge", dir.path());
        let md = render_with(&db, &quest, &opts, &NoExternal).unwrap();
        let body = section3(&md);
        assert!(!body.contains("TAIL"), "the tail survived the cut");
        assert!(body.contains("q workflow show huge"), "{body}");
        // Everything after section 3 is still there.
        assert!(md.contains("## 10. Open questions / blockers"), "{md}");
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
