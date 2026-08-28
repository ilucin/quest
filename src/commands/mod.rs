//! One module per CLI command; the dispatcher in `main.rs` calls into them.

pub mod brief;
pub mod close;
pub mod enter;
pub mod events;
pub mod fmt;
pub mod hook;
pub mod hook_capture;
pub mod kill;
pub mod list;
pub mod locate;
pub mod name;
pub mod new;
pub mod peek;
pub mod proxy;
pub mod rename;
pub mod reset;
pub mod resume;
pub mod rm;
pub mod send;
pub mod sessions;
pub mod set;
pub mod show;
pub mod spawn;
pub mod target;
pub mod tpl;
pub mod workflow;

// Agent self-report (bd-8lz.2.5).
pub mod link;
pub mod note;
pub mod phase;
pub mod report;

use std::io::{BufRead, IsTerminal, Write};

use serde::{Deserialize, Serialize};

use crate::Ctx;
use crate::beads::{self, Progress};
use crate::error::QError;
use crate::model::{
    DisplayState, Quest, Session, SessionStatus, display_state, master_ctx_pct, needs_you,
};
use crate::tmux;

/// A Quest with the three fields SPEC §4 derives rather than stores. `q list`
/// flattens it; `q show` nests the Quest itself.
///
/// `Deserialize` because this *is* the wire format between machines: a remote's
/// `q list --json` is read straight back into these (SPEC §15), so local and
/// remote rows need no translation. The optional fields default so a remote on
/// an older `q` that never learned to report one still parses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestView {
    #[serde(flatten)]
    pub quest: Quest,
    pub display_state: DisplayState,
    pub needs_you: bool,
    pub live_sessions: usize,
    /// The live master's context reading (SPEC §8); `null` when there is no
    /// live master, or the statusline hook has never reported one.
    #[serde(default)]
    pub master_ctx_pct: Option<u8>,
    /// Beads counts (SPEC §13); `null` without an epic, or when `bd` has never
    /// answered for this Quest.
    #[serde(default)]
    pub progress: Option<Progress>,
}

impl QuestView {
    pub fn new(quest: Quest, sessions: &[Session]) -> QuestView {
        QuestView {
            display_state: display_state(&quest, sessions),
            needs_you: needs_you(sessions),
            live_sessions: live(sessions).count(),
            master_ctx_pct: master_ctx_pct(sessions),
            progress: None,
            quest,
        }
    }

    pub fn with_progress(mut self, progress: Option<Progress>) -> QuestView {
        self.progress = progress;
        self
    }

    /// `3/7`, or `-` when there is nothing to report.
    pub fn progress_cell(&self) -> String {
        self.progress
            .map(|p| p.cell())
            .unwrap_or_else(|| "-".to_string())
    }

    /// `active` / `idle · needs you` — the state column of a listing.
    pub fn state_cell(&self) -> String {
        if self.needs_you {
            format!("{} · needs you", self.display_state)
        } else {
            self.display_state.to_string()
        }
    }
}

pub fn live(sessions: &[Session]) -> impl Iterator<Item = &Session> {
    sessions.iter().filter(|s| s.status != SessionStatus::Ended)
}

/// Where a listing row came from (SPEC §15), and the `source` object of
/// `q list --json`.
///
/// Not derivable from the row itself: a remote's rows carry `remotes[].name`
/// in their `machine` column, but nothing in a row says whether it was read
/// out of this machine's database, fetched over ssh a moment ago, or replayed
/// from the cache of a machine that is down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// This machine's database.
    Local,
    /// A remote's `q list --json`. `stale` is the cache standing in for a
    /// machine that did not answer this round.
    Remote { stale: bool },
}

impl Origin {
    pub fn is_remote(&self) -> bool {
        matches!(self, Origin::Remote { .. })
    }

    pub fn is_stale(&self) -> bool {
        matches!(self, Origin::Remote { stale: true })
    }
}

/// A Quest's derived view together with the sessions it was derived from.
/// `q list` only needs the view; the TUI's Quests tab also renders the
/// sessions, so the one loader hands back both rather than querying twice.
#[derive(Debug, Clone)]
pub struct QuestRow {
    pub view: QuestView,
    pub sessions: Vec<Session>,
    pub origin: Origin,
    /// The row exactly as a remote sent it, for `--json` to re-emit; `None`
    /// for a local row, which is serialized from `view`. See
    /// [`crate::remote::RemoteQuest`].
    pub raw: Option<serde_json::Value>,
}

impl QuestRow {
    pub fn local(view: QuestView, sessions: Vec<Session>) -> QuestRow {
        QuestRow {
            view,
            sessions,
            origin: Origin::Local,
            raw: None,
        }
    }

    /// A row a remote sent. It has no sessions here: sessions live in the
    /// database of the machine that runs them (SPEC §15, "no sync"), and
    /// reaching for them is bd-8lz.5.3's proxying.
    pub fn remote(quest: crate::remote::RemoteQuest, stale: bool) -> QuestRow {
        QuestRow {
            view: quest.view,
            sessions: Vec::new(),
            origin: Origin::Remote { stale },
            raw: Some(quest.raw),
        }
    }

    /// The machine column of `q list`: the machine's name, marked when the row
    /// is the cache standing in for a machine that did not answer (SPEC §15).
    ///
    /// The TUI marks the same thing with the glyph alone
    /// ([`crate::tui::quests`]): its rows are a fixed width and it carries a
    /// standing chip naming the machine, while a table column has room to say
    /// it outright.
    pub fn machine_cell(&self) -> String {
        if self.origin.is_stale() {
            format!("{} \u{26a0} stale", self.view.quest.machine)
        } else {
            self.view.quest.machine.clone()
        }
    }
}

/// Fold every remote's rows into the local listing and rank the whole thing as
/// one list (SPEC §15), draining `results` of the rows it moves.
///
/// One ranking, not one section per machine: the grouping SPEC §17 asks for is
/// needs-you / active / idle / finished, and a Quest that needs you needs you
/// wherever it runs. Ties keep the order the rows arrived in — local first,
/// then the remotes in config order — because [`sort_quests`] sorts stably.
pub fn merge_remote(rows: &mut Vec<QuestRow>, results: &mut [crate::remote::RemoteResult]) {
    rows.extend(remote_rows(results));
    sort_quests(rows);
}

/// One round's remote rows, drained out of `results`.
///
/// Split from [`merge_remote`] for the TUI, which builds these once when a
/// round lands and then re-merges the same rows into every 2 s local reload —
/// its remote tick is 10 s (SPEC §17), so most reloads have no new answer to
/// fold in.
pub fn remote_rows(results: &mut [crate::remote::RemoteResult]) -> Vec<QuestRow> {
    let mut out = Vec::new();
    for result in results.iter_mut() {
        let stale = result.stale;
        let quests = std::mem::take(&mut result.quests);
        out.extend(quests.into_iter().map(|q| QuestRow::remote(q, stale)));
    }
    out
}

/// SPEC §16's listing filters, as one predicate.
///
/// The single definition of "is this row in this listing": `q list` runs local
/// rows through it and [`crate::remote::retain_listed`] runs every remote row —
/// fresh and cached alike — through the same one. bd-8lz.5.1's constraint is
/// that the merge must not filter remote rows differently from local ones, and
/// one shared function is the only way to keep that true.
pub fn listed(view: &QuestView, all: bool, state: Option<crate::cli::QuestState>) -> bool {
    match state {
        // `--state` is exact, `--all` or not: asking for finished Quests is how
        // you see them, and asking for active ones must never return others.
        Some(want) => view.display_state == display_state_of(want),
        None => all || view.display_state != DisplayState::Finished,
    }
}

/// The `DisplayState` a `--state` value names.
pub fn display_state_of(state: crate::cli::QuestState) -> DisplayState {
    match state {
        crate::cli::QuestState::Active => DisplayState::Active,
        crate::cli::QuestState::Idle => DisplayState::Idle,
        crate::cli::QuestState::Finished => DisplayState::Finished,
    }
}

/// Every **local** Quest, swept, machine-filtered and ranked.
///
/// The single definition of "the Quest listing": `q list` and the TUI's Quests
/// tab both come through here, so they can never disagree about which Quests
/// exist, what state they are in, or what order they belong in. Remote rows
/// join them through [`merge_remote`], on each caller's own schedule — the CLI
/// once per invocation, the TUI on `[ui] tick_remote` rather than on its 2 s
/// local tick.
pub fn load_quests(ctx: &Ctx, include_finished: bool) -> anyhow::Result<Vec<QuestRow>> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let mut rows: Vec<QuestRow> = Vec::new();
    for quest in db.list_quests(include_finished)? {
        // Only this machine's Quests are in here; a remote's come over ssh and
        // are folded in by [`merge_remote`].
        if ctx.machine_filter().is_some_and(|m| m != quest.machine) {
            continue;
        }
        let sessions = db.list_sessions_by_quest(&quest.id)?;
        rows.push(QuestRow::local(QuestView::new(quest, &sessions), sessions));
    }
    sort_quests(&mut rows);
    Ok(rows)
}

/// One `bd` call for the whole listing, capped and cache-backed, so a slow or
/// missing `bd` can never hold up a listing or a TUI tick (SPEC §13).
pub fn fill_progress(ctx: &Ctx, rows: &mut [QuestRow]) {
    let quests: Vec<&Quest> = rows.iter().map(|r| &r.view.quest).collect();
    let progress = beads::progress_all_with(ctx.bd(), &quests);
    for row in rows.iter_mut() {
        row.view.progress = progress.get(&row.view.quest.id).copied();
    }
}

/// What needs the human first, then what is running, then the rest; ties go to
/// the most recently touched Quest.
pub fn sort_quests(rows: &mut [QuestRow]) {
    rows.sort_by(|a, b| {
        rank(&a.view)
            .cmp(&rank(&b.view))
            .then(b.view.quest.updated_at.cmp(&a.view.quest.updated_at))
    });
}

/// The group a Quest belongs to, and the order the groups are shown in
/// (SPEC §17: needs-you on top, then active, then idle, finished last).
pub fn rank(view: &QuestView) -> u8 {
    if view.needs_you {
        return 0;
    }
    match view.display_state {
        DisplayState::Active => 1,
        DisplayState::Idle => 2,
        DisplayState::Finished => 3,
    }
}

/// The pane's own process id, when tmux still has that pane. `None` is an
/// ordinary answer: the pane may be gone, or tmux may not be running.
pub fn pane_pid(ctx: &Ctx, pane_id: &str) -> Option<i64> {
    ctx.tmux()
        .list_panes()
        .ok()?
        .into_iter()
        .find(|p| p.pane_id == pane_id)
        .map(|p| i64::from(p.pane_pid))
}

/// The liveness sweep every command that reads Quests runs first (SPEC §6).
/// Silent: what it changed shows up in the listing it precedes.
pub fn sweep_quiet(ctx: &Ctx) -> anyhow::Result<()> {
    tmux::sweep(ctx.db()?, ctx.tmux())?;
    Ok(())
}

/// Write out and clear whatever the call buffered on the `Ctx` (see
/// [`Ctx::warn`]). Every command that can warn calls this before its own
/// output, so the order on the terminal is exactly what it was when these were
/// `eprintln!`s inside the library — and the TUI, which drains the same buffer
/// into its status bar, never sees a byte reach the screen behind its back.
pub fn flush_warnings(ctx: &Ctx) {
    for warning in ctx.take_warnings() {
        eprintln!("{warning}");
    }
}

/// `[y/N]` on a terminal, asked on stderr so it never pollutes the payload.
/// Anything else — a plain "no", a pipe, a closed stdin — refuses, and `-f` is
/// the only way past it.
///
/// `--json` and `$Q_QUEST` (an agent running inside a Quest pane) refuse
/// without asking at all: nobody is there to answer, and a blocked agent is
/// worse than a failed command.
///
/// `--confirmed` answers it: a human already said yes, on the machine that had
/// the terminal (SPEC §15's proxy). It skips this question and no other check —
/// see [`crate::cli::Cli::confirmed`].
pub fn confirm(ctx: &Ctx, question: &str) -> anyhow::Result<()> {
    if ctx.confirmed() {
        return Ok(());
    }
    if ctx.json || in_quest_pane() {
        return Err(aborted());
    }
    // The fixture stands in for stdin, and for nothing else: the question is
    // still asked out loud, so a test reads exactly what the human would.
    let scripted = fixture_answer();
    if scripted.is_none() && !std::io::stdin().is_terminal() {
        return Err(aborted());
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    if let Some(answer) = scripted {
        return answer;
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err(aborted()),
    }
}

fn in_quest_pane() -> bool {
    std::env::var_os("Q_QUEST").is_some_and(|v| !v.is_empty())
}

/// The scripted answer to a `[y/N]`, or `None` when nothing scripted one.
///
/// A test has no terminal, so without this the only half of a confirmation the
/// suite can execute is the abort — and the *yes* is where a destructive
/// command actually destroys something. Gated on `$Q_FIXTURE`, exactly like the
/// tmux, ssh, `bd` and `claude` stubs, so a stray variable in a real shell
/// cannot answer a real question.
fn fixture_answer() -> Option<anyhow::Result<()>> {
    std::env::var_os("Q_FIXTURE").filter(|v| !v.is_empty())?;
    let said = std::env::var("Q_FIXTURE_CONFIRM").ok()?;
    Some(match said.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err(aborted()),
    })
}

/// What a command did with the terminal, for the payload and the one-liner.
/// `Switch` and `Exec` move a client between sessions; `Select` only changes
/// which window of the caller's own session is active, which is all `q spawn`
/// ever does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachMode {
    /// `-d` / `--no-attach`: the terminal is left alone.
    None,
    Select,
    Switch,
    Exec,
}

impl std::fmt::Display for AttachMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AttachMode::None => "none",
            AttachMode::Select => "select",
            AttachMode::Switch => "switch",
            AttachMode::Exec => "exec",
        })
    }
}

/// What an attach will do: inside tmux the client switches, outside it the
/// process is replaced by `tmux attach`. The one helper every attaching
/// command reports through.
pub fn attach_mode(ctx: &Ctx, attaching: bool) -> AttachMode {
    match (attaching, ctx.tmux().in_tmux()) {
        (false, _) => AttachMode::None,
        (true, true) => AttachMode::Switch,
        (true, false) => AttachMode::Exec,
    }
}

fn aborted() -> anyhow::Error {
    QError::Other("aborted (use -f)".to_string()).into()
}
