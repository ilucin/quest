//! `q new` — creates the Quest row, its tmux session with the `master` window,
//! and launches Claude in it (SPEC §5, §6).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Ctx;
use crate::beads;
use crate::commands::{AttachMode, attach_mode, sweep_quiet};
use crate::db::quest::QuestPatch;
use crate::db::{Db, ID_ATTEMPTS};
use crate::error::QError;
use crate::model::{NameSource, Quest, Session, SessionRole, SessionStatus, Template, new_id, now};
use crate::output;
use crate::tmux::{NewSession, config_override, db_override, quest_env, session_name};

pub const SLUG_MAX: usize = 40;
/// `foo`, `foo-2` … `foo-99` — how far an auto slug will step aside.
const SLUG_ATTEMPTS: u32 = 99;
const SLUG_RULE: &str = "must match ^[a-z0-9]+(-[a-z0-9]+)*$ and be at most 40 characters";
pub const MASTER: &str = "master";

/// Branch names that say nothing about the work, so they never become a slug.
pub const GENERIC_BRANCHES: [&str; 4] = ["main", "master", "develop", "HEAD"];

/// All a `git rev-parse` gets. Naming runs it from a hook.
const GIT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub struct Args<'a> {
    pub name: Option<&'a str>,
    pub goal: Option<&'a str>,
    pub dir: Option<&'a str>,
    pub workflow: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub no_beads: bool,
    pub prompt: Option<&'a str>,
    pub prompt_file: Option<&'a str>,
    pub no_auto_reset: bool,
    /// Create a brain session note for this Quest (SPEC §14, `--brain`). A
    /// template's `create_brain` maps onto this in a later milestone (7.9).
    pub brain: bool,
    pub detach: bool,
    /// The machine the Quest is recorded against. `None` is `ctx.machine()`,
    /// which is what the global `--machine` already decides for the CLI; the
    /// TUI's new-Quest form picks it per Quest instead (SPEC §17).
    pub machine: Option<&'a str>,
    /// The template this Quest is instantiated from (SPEC §11's `template_id`
    /// column) — `q tpl run`, `q new --template`, and the TUI's template
    /// select. The whole row rather than the id: it also names the Quest
    /// (`NameSource::Template`, below) and its run is counted here, so no
    /// caller can record one half and forget the other.
    pub template: Option<&'a Template>,
}

/// Everything `q new` creates, before anything is printed or attached to.
///
/// [`create`] is the whole of quest creation as a library call, so the TUI's
/// new-Quest form runs exactly the path `q new` runs — the slug claim, the
/// beads epic, the tmux session, the master, and the rollback when the master
/// will not start — rather than a second copy of it.
pub struct Created {
    pub quest: Quest,
    pub session: Session,
    pub tmux_session: String,
    /// The template this Quest came from, with the run just counted — SPEC
    /// §11's `run_count` / `last_run_at`. `None` when no template was used.
    pub template: Option<Template>,
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let created = create(ctx, args);
    // Before the payload and before `main` renders a failure: a warning about
    // the epic explains what the line after it says (or does not say), and on
    // the rollback path it is the only record of an epic that outlived its
    // Quest.
    crate::commands::flush_warnings(ctx);
    let Created {
        quest,
        session,
        tmux_session,
        template: _,
    } = created?;

    let attach = attach_mode(ctx, !args.detach);
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "session": session,
                "tmux_session": tmux_session,
                "attach": attach,
            }),
            || {
                format!(
                    "created quest {} ({}) · tmux {tmux_session} · run: q enter {}",
                    quest.id, quest.slug, quest.slug
                )
            },
        )?;
    }
    if attach != AttachMode::None {
        // An exec attach replaces this process, so nothing buffered survives it.
        std::io::stdout().flush()?;
        ctx.tmux().attach(&tmux_session, Some(&session.tmux_pane))?;
    }
    Ok(())
}

/// The Quest row, its beads epic, its tmux session and its master — SPEC §5
/// steps 1, 2, 4 and 5. Steps 3 (brain) and 6 (attach) are the caller's:
/// `q new` attaches unless `-d`, the TUI leaves that to `o`.
pub fn create(ctx: &Ctx, args: &Args) -> anyhow::Result<Created> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    // Both before anything is written: a bad label or a contradictory pair of
    // flags is the user's typo, not a half-created Quest.
    let repo = repo_flag(args)?;
    // SPEC §11: the workflow is a file, and a Quest whose workflow does not
    // resolve gives its master a brief with a hole where its instructions
    // should be. Checked here, before the row, the epic or the tmux session —
    // and this is also where `q tpl run`'s stored workflow is checked, the way
    // `run_cwd` checks its `cwd`.
    // Checked *and* normalized: the column stores the trimmed name that was
    // validated, so ` solo ` cannot become a Quest whose every brief reports a
    // workflow it "could not read", and `   ` means unset rather than a broken
    // name no later filter catches.
    let workflow = ctx.workflows().check_opt(args.workflow)?;
    let cwd = resolve_dir(args.dir)?;
    let prompt = resolve_prompt(args.prompt, args.prompt_file)?;
    let (base, name_source) = resolve_slug(args.name, args.template, &cwd)?;
    let (slug, tmux_session) = claim_slug(ctx, db, &base, name_source)?;

    let machine = match args.machine {
        Some(machine) => {
            crate::config::validate_machine_name(machine)?;
            machine
        }
        None => ctx.machine(),
    };

    let mut row = Quest::new(&slug, &cwd.to_string_lossy(), machine);
    row.name_source = name_source;
    row.goal = args.goal.map(str::to_string);
    // Already checked above, with the other flags, before anything was written.
    row.workflow = workflow;
    row.template_id = args.template.map(|t| t.id.clone());
    // Only the opt-out is stored; NULL keeps following `[context] auto_reset`.
    row.auto_reset = args.no_auto_reset.then_some(false);
    let quest = db.insert_quest(&row)?;
    db.append_event(
        &quest.id,
        None,
        "quest.created",
        &serde_json::json!({
            "slug": quest.slug,
            "goal": quest.goal,
            "cwd": quest.cwd,
            "machine": quest.machine,
            "workflow": quest.workflow,
            "template_id": quest.template_id,
            "name_source": quest.name_source,
            "auto_reset": quest.auto_reset,
        }),
    )?;

    // The epic goes in before the master starts, so the brief its SessionStart
    // hook injects already names it. A failing `bd` is a warning, never a
    // failed `q new` (SPEC §13).
    let quest = create_epic(ctx, quest, args, repo.as_deref());

    let master = match spawn_master(ctx, &quest, prompt) {
        Ok(master) => master,
        // Nothing was started, so the Quest row would only be an orphan — and
        // so would the epic, which lives in a tracker this row was the only
        // pointer to.
        Err(e) => {
            abandon_epic(ctx, &quest);
            let _ = db.delete_quest(&quest.id);
            return Err(e);
        }
    };
    // After the master is up, so a rollback never counts a run that produced
    // nothing; before the caller attaches, because an `exec` attach never
    // comes back here (`q tpl run`).
    let template = match args.template {
        Some(template) => Some(db.bump_template_run(&template.id, now())?),
        None => None,
    };
    // SPEC §5 step 3: the brain session note. Best effort — a missing brain or
    // an unwritable note is a buffered warning, never a failed `q new` — and it
    // runs only after the master is up, so a rollback never leaves a stray note.
    let quest = maybe_create_brain(ctx, quest, args);
    Ok(Created {
        quest,
        session: master.session,
        tmux_session,
        template,
    })
}

/// `q new … -d --json`, as the far end must receive it (SPEC §15's
/// `ssh <alias> q new … -d`).
///
/// **Built, not forwarded** — unlike every other proxied command
/// ([`crate::commands::proxy`]). It has to be: the TUI's new-Quest form reaches
/// the same path with no argv behind it, and one builder is the only way the
/// CLI and the form cannot drift apart.
///
/// * `-d` because there is no terminal at the far end; the attach is this
///   machine's, afterwards.
/// * `--json` because the answer has to be read — the slug and the tmux session
///   name are how that attach finds the Quest.
/// * `--dir` travels only when it was given. This machine's cwd is not a path
///   over there, so without it the far end's `q new` uses its own default (the
///   ssh login directory) rather than a directory that happens to share a name.
/// * `--prompt-file` never travels: it names a file on *this* machine, and
///   [`resolve_prompt`] has already turned it into text.
///
/// Nothing here is quoted: quoting is [`crate::remote`]'s, at the single ssh
/// boundary.
pub fn remote_argv(args: &Args) -> Vec<String> {
    let mut argv = vec!["q".to_string(), "new".to_string()];
    let mut flag = |name: &str, value: Option<&str>| {
        if let Some(value) = value {
            argv.push(name.to_string());
            argv.push(value.to_string());
        }
    };
    flag("--name", args.name);
    flag("--goal", args.goal);
    flag("--dir", args.dir);
    flag("--workflow", args.workflow);
    flag("--repo", args.repo);
    flag("--prompt", args.prompt);
    if args.no_beads {
        argv.push("--no-beads".to_string());
    }
    if args.no_auto_reset {
        argv.push("--no-auto-reset".to_string());
    }
    if args.brain {
        argv.push("--brain".to_string());
    }
    argv.push("-d".to_string());
    argv.push("--json".to_string());
    argv.push(crate::commands::proxy::NO_REMOTE.to_string());
    argv
}

/// The Quest row is about to be deleted, so its epic loses its only pointer:
/// close it rather than leave a stray open epic in a shared tracker. A `bd`
/// that will not cooperate is named, so the id is not simply lost.
fn abandon_epic(ctx: &Ctx, quest: &Quest) {
    let Some(epic) = beads::epic_of(quest) else {
        return;
    };
    if let Err(err) = ctx.bd().close(epic, "quest creation failed") {
        ctx.warn(format!(
            "warning: quest creation failed and beads epic {epic} could not be closed \
             ({err}); close it with `bd close {epic}`"
        ));
    }
}

/// `--repo` as a label, once: it is rejected outright when it cannot be one,
/// and refused as a contradiction alongside `--no-beads` (there is no epic for
/// it to label).
fn repo_flag(args: &Args) -> anyhow::Result<Option<String>> {
    let Some(repo) = args.repo else {
        return Ok(None);
    };
    if args.no_beads {
        return Err(QError::Invalid(
            "--repo labels the beads epic, which --no-beads skips; drop one of them".to_string(),
        )
        .into());
    }
    Ok(Some(beads::validate_repo_label(repo)?))
}

/// Creates the Quest's beads epic and stores it on the row. Returns the Quest
/// unchanged when `--no-beads` was given or `bd` could not be reached — the
/// warning is buffered on the `Ctx` (never written), so `--json` stdout stays a
/// single payload and a TUI caller can put it in the status bar instead.
fn create_epic(ctx: &Ctx, quest: Quest, args: &Args, repo: Option<&str>) -> Quest {
    if args.no_beads {
        return quest;
    }
    attach_epic(ctx, quest, repo)
}

/// Creates an epic for a Quest that has none and stores it on the row — at
/// `q new`, or later from `q set <quest> beads_epic new` for a Quest the TUI's
/// bare `n` made without one. `repo` is the label; `None` derives it from the
/// config and the Quest's directory.
pub fn attach_epic(ctx: &Ctx, quest: Quest, repo: Option<&str>) -> Quest {
    let repo = beads::repo_label(&ctx.config, repo, Path::new(&quest.cwd));
    let labels = format!("repo:{repo},quest:{}", quest.id);
    let title = beads::epic_title(&quest);
    match ctx.bd().create_epic(&title, &labels, &quest.id) {
        Ok(epic) => store_epic(ctx, quest, &epic, &repo),
        Err(e) => {
            ctx.warn(format!(
                "warning: no beads epic for {} ({e}); link one later with \
                 `q set {} beads_epic <id>`, or pass --no-beads to skip this",
                quest.slug, quest.slug
            ));
            quest
        }
    }
}

/// A stored epic the database then refuses is still a real epic, so the
/// database error is reported and the Quest carries on without the column.
fn store_epic(ctx: &Ctx, quest: Quest, epic: &str, repo: &str) -> Quest {
    let patch = QuestPatch {
        beads_epic: Some(Some(epic.to_string())),
        beads_repo: Some(Some(repo.to_string())),
        ..QuestPatch::default()
    };
    let stored = ctx.db().and_then(|db| {
        let stored = db.update_quest(&quest.id, &patch)?;
        db.append_event(
            &quest.id,
            None,
            "beads.epic",
            &serde_json::json!({ "epic": epic, "repo": repo }),
        )?;
        Ok(stored)
    });
    match stored {
        Ok(stored) => stored,
        Err(e) => {
            ctx.warn(format!(
                "warning: beads epic {epic} could not be stored: {e:#}"
            ));
            quest
        }
    }
}

/// SPEC §14: writes `sessions/<slug>/<slug>.md` under the brain root and stores
/// the slug as `quest.brain_session`, so the brief and `q link add`'s
/// `sync_links` can find the note. A no-op unless `--brain` was given.
///
/// Best effort in both halves: no brain root (no `~/.brainrc`, no
/// `$Q_BRAIN_ROOT`) or an unwritable note leaves the Quest without a session,
/// and a stored slug the database then refuses is warned about — the Quest
/// still stands either way. The note write is idempotent (see [`crate::brain`]).
fn maybe_create_brain(ctx: &Ctx, quest: Quest, args: &Args) -> Quest {
    if !args.brain {
        return quest;
    }
    let Some(root) = crate::brain::root() else {
        ctx.warn(format!(
            "warning: --brain: no brain root (set [brain] in ~/.brainrc or $Q_BRAIN_ROOT); \
             quest {} created without a brain session",
            quest.slug
        ));
        return quest;
    };
    let note = crate::brain::SessionNote {
        quest_id: &quest.id,
        machine: &quest.machine,
        cwd: &quest.cwd,
        beads_epic: quest.beads_epic.as_deref(),
        created: &crate::commands::fmt::stamp_utc(quest.created_at),
    };
    if let Err(e) = crate::brain::write_session_note(&root, &quest.slug, &note) {
        ctx.warn(format!(
            "warning: --brain: could not write the session note for {} ({e})",
            quest.slug
        ));
        return quest;
    }
    let patch = QuestPatch {
        brain_session: Some(Some(quest.slug.clone())),
        ..QuestPatch::default()
    };
    match ctx.db().and_then(|db| {
        let stored = db.update_quest(&quest.id, &patch)?;
        db.append_event(
            &quest.id,
            None,
            "brain.session",
            &serde_json::json!({ "slug": quest.slug }),
        )?;
        Ok(stored)
    }) {
        Ok(stored) => stored,
        Err(e) => {
            ctx.warn(format!(
                "warning: brain session note written but not recorded on {}: {e:#}",
                quest.slug
            ));
            quest
        }
    }
}

/// Why a slug cannot be used. A Quest row and a live tmux session are both
/// blocking: `q rename` refuses to move onto either.
enum Taken {
    Quest(String),
    Tmux(String),
}

/// What holds `slug`, or `None` when nothing does.
fn taken(ctx: &Ctx, db: &Db, slug: &str) -> anyhow::Result<Option<Taken>> {
    if let Some(existing) = db.get_quest_by_slug(slug)? {
        return Ok(Some(Taken::Quest(existing.id)));
    }
    let tmux_session = session_name(&ctx.config, slug);
    if ctx.tmux().has_session(&tmux_session)? {
        return Ok(Some(Taken::Tmux(tmux_session)));
    }
    Ok(None)
}

/// `base` for `n == 1`, then `base-2`, `base-3`, …
fn candidate(base: &str, n: u32) -> String {
    if n == 1 {
        base.to_string()
    } else {
        numbered(base, n)
    }
}

/// What [`claim`] found for a proposed slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// Free — `base` itself, or the first numbered variant that was.
    Free(String),
    /// The caller already holds it; there is nothing to rename.
    Own,
    /// `base` and every variant of it is taken.
    Exhausted,
}

/// The first of `base`, `base-2`, … that neither a Quest nor a tmux session
/// holds. `own` is the slug the caller already carries.
///
/// Auto-naming steps aside by exactly the rule `q new` does (`claim_slug`
/// below shares the check and the attempt count), so a proposal that collides
/// lands on the same variant either way — and one that would collide with a
/// tmux session is skipped here rather than failing inside `rename::apply`.
pub fn claim(ctx: &Ctx, base: &str, own: &str) -> anyhow::Result<Claim> {
    let db = ctx.db()?;
    for n in 1..=SLUG_ATTEMPTS {
        let slug = candidate(base, n);
        if slug == own {
            return Ok(Claim::Own);
        }
        if taken(ctx, db, &slug)?.is_none() {
            return Ok(Claim::Free(slug));
        }
    }
    Ok(Claim::Exhausted)
}

/// The first free slug and the tmux session that goes with it. A slug nobody
/// typed steps aside (`-2`, `-3`, …) — an auto one and a template's name
/// alike, since the second run of a routine must not fail on the first run's
/// row. Only an explicit `--name` is a hard error instead.
fn claim_slug(
    ctx: &Ctx,
    db: &Db,
    base: &str,
    source: NameSource,
) -> anyhow::Result<(String, String)> {
    let auto = source != NameSource::Manual;
    for n in 1..=SLUG_ATTEMPTS {
        let slug = candidate(base, n);
        match taken(ctx, db, &slug)? {
            None => {
                let tmux_session = session_name(&ctx.config, &slug);
                return Ok((slug, tmux_session));
            }
            Some(_) if auto => continue,
            Some(Taken::Quest(id)) => {
                return Err(QError::Conflict(format!(
                    "slug `{slug}` is already taken by quest {id}; pick another with --name"
                ))
                .into());
            }
            Some(Taken::Tmux(tmux_session)) => {
                return Err(QError::Conflict(format!(
                    "tmux session `{tmux_session}` already exists; kill it or pick another slug with --name"
                ))
                .into());
            }
        }
    }
    Err(QError::Conflict(format!(
        "`{base}` and its first {SLUG_ATTEMPTS} variants are all taken; pick a slug with --name"
    ))
    .into())
}

/// `base-<n>`, kept within `SLUG_MAX` by trimming the base.
pub fn numbered(base: &str, n: u32) -> String {
    let suffix = format!("-{n}");
    let mut head = base.to_string();
    if head.len() + suffix.len() > SLUG_MAX {
        head.truncate(SLUG_MAX - suffix.len());
    }
    format!("{}{suffix}", head.trim_end_matches('-'))
}

/// The `master` window of `q-<slug>`, and the session row recording it.
pub struct Master {
    pub session: Session,
    pub tmux_session: String,
}

/// Creates the Quest's tmux session with `master` in window 0, starts Claude
/// there and records the session row (SPEC §5, §6). Shared by `q new` and
/// `q resume`; the caller owns whatever else has to be undone on failure.
pub fn spawn_master(ctx: &Ctx, quest: &Quest, prompt: Option<String>) -> anyhow::Result<Master> {
    let db = ctx.db()?;
    let tmux_session = session_name(&ctx.config, &quest.slug);
    // The session id goes into the window's environment, so it has to exist
    // before the pane it will be stored against.
    let session_id = fresh_session_id(db)?;
    let spec = NewSession {
        name: tmux_session.clone(),
        window_name: MASTER.to_string(),
        cwd: quest.cwd.clone(),
        env: quest_env(
            &quest.id,
            &session_id,
            SessionRole::Master,
            &quest.machine,
            db_override().as_deref(),
            config_override().as_deref(),
        ),
        // The pane command is the login shell (SPEC §6 v2): Claude is a child
        // launched into it below, so `/exit` lands back in a shell rather than
        // killing the session.
        command: None,
    };
    let pane = ctx.tmux().new_session(&spec)?;
    // The Quest now has a tmux session, so the spawn-a-worker key can be live.
    // Server-wide and best-effort (see `bind_spawn_key`), set on every master.
    bind_spawn_key(ctx);

    let mut row = Session::new(
        &quest.id,
        SessionRole::Master,
        MASTER,
        &tmux_session,
        &pane.pane_id,
    );
    row.id = session_id.clone();
    row.status = SessionStatus::Starting;
    // The master's workflow *is* the Quest's, by definition (SPEC §11: "master
    // može promijeniti"). Leaving its session column unset — rather than
    // snapshotting the Quest's value at `q new` time — is what lets a later
    // `q workflow set` reach the master's own brief (D5): `effective_workflow`
    // falls through to the Quest's. A worker still gets its own via
    // `q spawn --workflow`, which is why the session column exists at all.
    row.first_prompt = prompt;
    // The name `claude -n` was just given, so the registry's identity check has
    // something true to compare against before any rename (SPEC §6).
    row.claude_name = Some(crate::naming::claude_name(&quest.slug, MASTER));
    // `session.start` is the hook's to append once Claude comes up (M1).
    let session = match db.insert_session(&row) {
        // A regenerated id would no longer match `Q_SESSION` in the window.
        Ok(session) if session.id != session_id => {
            let _ = ctx.tmux().kill_session(&tmux_session);
            return Err(QError::Db(format!(
                "session id `{session_id}` was taken between allocating and inserting it"
            ))
            .into());
        }
        Ok(session) => session,
        Err(e) => {
            let _ = ctx.tmux().kill_session(&tmux_session);
            return Err(e);
        }
    };
    // The pane is a shell; launch Claude into it (SPEC §6 v2). The prompt is
    // already on the row (`first_prompt`), so `launch` reuses it. A launch that
    // will not go takes the tmux session down with it, exactly as an insert
    // failure does — a shell with no Claude and no row is nobody's.
    let started = match crate::commands::start::launch(ctx, quest, &session, None, false, false) {
        Ok(started) => started,
        Err(e) => {
            let _ = ctx.tmux().kill_session(&tmux_session);
            let _ = db.delete_session(&session.id);
            return Err(e);
        }
    };
    Ok(Master {
        session: started.session,
        tmux_session,
    })
}

/// An existing directory, canonicalized. `None` is the current one.
pub fn resolve_dir(dir: Option<&str>) -> anyhow::Result<PathBuf> {
    let raw = match dir {
        Some(d) => PathBuf::from(d),
        None => std::env::current_dir()
            .map_err(|e| QError::Other(format!("cannot read the current directory: {e}")))?,
    };
    if !raw.exists() {
        return Err(QError::NotFound(format!("no such directory: {}", raw.display())).into());
    }
    if !raw.is_dir() {
        return Err(QError::Invalid(format!("not a directory: {}", raw.display())).into());
    }
    raw.canonicalize()
        .map_err(|e| QError::Other(format!("cannot resolve {}: {e}", raw.display())).into())
}

/// `--prompt`, or `--prompt-file <path>`; `-` reads stdin. Blank is no prompt.
pub fn resolve_prompt(prompt: Option<&str>, file: Option<&str>) -> anyhow::Result<Option<String>> {
    let text = match (prompt, file) {
        (Some(text), _) => text.to_string(),
        (None, Some("-")) => std::io::read_to_string(std::io::stdin())
            .map_err(|e| QError::Other(format!("cannot read the prompt from stdin: {e}")))?,
        (None, Some(path)) => std::fs::read_to_string(path)
            .map_err(|e| QError::Invalid(format!("cannot read {path}: {e}")))?,
        (None, None) => return Ok(None),
    };
    let text = text.trim();
    Ok((!text.is_empty()).then(|| text.to_string()))
}

/// `--name` is taken as given (validated), then the template's name, then the
/// M0 heuristic (SPEC §4's three `name_source` values).
fn resolve_slug(
    name: Option<&str>,
    template: Option<&Template>,
    cwd: &Path,
) -> anyhow::Result<(String, NameSource)> {
    if let Some(name) = name {
        validate_slug(name)?;
        return Ok((name.to_string(), NameSource::Manual));
    }
    // A routine run three times should be three recognisable rows in `q list`,
    // not three model-invented names: a templated Quest is named after its
    // template (`weekly-hygiene`, `weekly-hygiene-2`, …). `template` is also
    // what stops `naming::schedule` renaming it — that gate is `auto` only.
    if let Some(template) = template {
        return Ok((template.name.clone(), NameSource::Template));
    }
    // A new Quest gets the heuristic slug right away — `q new` must not
    // wait on a model. `name_source = auto` and a NULL `name_input_hash`
    // then make the master's first `Stop` hook schedule the real
    // auto-name (SPEC §10, `naming.rs`).
    Ok((
        heuristic_slug(git_branch(cwd).as_deref(), cwd),
        NameSource::Auto,
    ))
}

pub fn validate_slug(slug: &str) -> anyhow::Result<()> {
    validate_kebab("slug", slug)
}

/// A session label follows the slug grammar — it becomes part of a tmux window
/// name and of `claude -n <slug>/<label>` (SPEC §6).
pub fn validate_label(label: &str) -> anyhow::Result<()> {
    validate_kebab("label", label)
}

/// A template name follows the same grammar (SPEC §11). It is not a slug — no
/// Quest is named after it — but it is typed as a target, matched by prefix,
/// and written into TOML by hand, and one grammar for all three is one rule to
/// remember.
pub fn validate_template_name(name: &str) -> anyhow::Result<()> {
    validate_kebab("template name", name)
}

/// A workflow name follows it too (SPEC §11), and here the grammar is load
/// bearing rather than only tidy: the name becomes `<name>.md` under the config
/// directory, so a spelling with a `/` or a `..` in it would address a file
/// somewhere else entirely.
pub fn validate_workflow_name(name: &str) -> anyhow::Result<()> {
    validate_kebab("workflow name", name)
}

fn validate_kebab(what: &str, value: &str) -> anyhow::Result<()> {
    if value.len() > SLUG_MAX || !is_slug(value) {
        return Err(QError::Invalid(format!("invalid {what} `{value}`: it {SLUG_RULE}")).into());
    }
    Ok(())
}

pub fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() && !c.is_ascii_uppercase())
        })
}

/// Branch first — it usually names the work; then the directory; then the id.
fn heuristic_slug(branch: Option<&str>, cwd: &Path) -> String {
    let from_branch = branch
        .filter(|b| !GENERIC_BRANCHES.contains(b))
        .map(slugify)
        .filter(|s| !s.is_empty());
    if let Some(slug) = from_branch {
        return slug;
    }
    let from_dir = cwd
        .file_name()
        .map(|n| slugify(&n.to_string_lossy()))
        .filter(|s| !s.is_empty());
    from_dir.unwrap_or_else(|| new_id("quest"))
}

/// Lowercased, every other run of characters collapsed to one `-`.
pub fn slugify(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(SLUG_MAX);
    out.trim_matches('-').to_string()
}

/// The branch checked out in `cwd`, or `None`. Called from the master's `Stop`
/// hook on every turn (`naming::Input::collect`), so it is bounded: a `git` on
/// a stale network mount or waiting on `index.lock` must cost a hook the
/// budget below and nothing more.
pub fn git_branch(cwd: &Path) -> Option<String> {
    let branch = crate::proc::run_capped(
        "git",
        &[
            "-C",
            &cwd.to_string_lossy(),
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
        ],
        GIT_TIMEOUT,
    )?;
    (!branch.is_empty()).then_some(branch)
}

/// `claude -n <slug>/<label> [-- <prompt>]`, run by tmux through a shell.
/// Bind `[tmux] spawn_key` (default prefix+`N`) server-wide to `q spawn-here`,
/// so any pane in any Quest can open a fresh worker in its own Quest. The bind
/// is a tmux server setting, not a session one, so setting it whenever a master
/// comes up is idempotent; an empty `spawn_key` turns it off.
///
/// The bind is server-wide, so it would clobber a user's own prefix+key: only
/// take the key when it is unbound, or already bound to *our* `q spawn-here`.
/// Anything else the user set stays put and we skip — prefix+N then just isn't
/// wired to spawning here.
///
/// Best-effort throughout: a tmux that refuses the bind, or a `q` whose own path
/// cannot be resolved, must not sink the master that has just come up.
fn bind_spawn_key(ctx: &Ctx) {
    let key = ctx.config.tmux.spawn_key.trim();
    if key.is_empty() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // `#{pane_id}` is tmux's, expanded to the pressed pane before the shell
    // runs; single-quoted so the shell passes it through untouched.
    let command = format!(
        "{} spawn-here '#{{pane_id}}'",
        shell_quote(&exe.to_string_lossy())
    );
    // Don't overwrite a binding the user set for themselves; only claim the key
    // when nothing holds it, or our own spawn-here already does.
    if let Ok(Some(existing)) = ctx.tmux().prefix_binding(key)
        && !existing.contains("spawn-here")
    {
        return;
    }
    let _ = ctx.tmux().bind_key(key, &command);
}

/// Single-quoted unless the word is plainly safe; `'` is closed, escaped and
/// reopened, which is the only way out of single quotes in sh.
pub fn shell_quote(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

pub fn fresh_session_id(db: &Db) -> anyhow::Result<String> {
    for _ in 0..ID_ATTEMPTS {
        let id = new_id("s");
        if db.get_session(&id)?.is_none() {
            return Ok(id);
        }
    }
    Err(QError::Db("cannot allocate a session id".to_string()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_validation_follows_the_spec_grammar() {
        for good in [
            "a",
            "q1",
            "cdc-backfill-retry",
            "a-1-b",
            &"x".repeat(SLUG_MAX),
        ] {
            assert!(validate_slug(good).is_ok(), "rejected `{good}`");
        }
        for bad in [
            "",
            "-lead",
            "trail-",
            "double--dash",
            "Upper",
            "with space",
            "under_score",
            "sla/sh",
            &"x".repeat(SLUG_MAX + 1),
        ] {
            let e = validate_slug(bad).unwrap_err();
            assert!(format!("{e}").contains("invalid slug"), "accepted `{bad}`");
        }
    }

    #[test]
    fn heuristic_prefers_a_meaningful_branch() {
        let dir = Path::new("/tmp/some-repo");
        assert_eq!(
            heuristic_slug(Some("feat/CDC-backfill"), dir),
            "feat-cdc-backfill"
        );
        assert_eq!(heuristic_slug(Some("main"), dir), "some-repo");
        assert_eq!(heuristic_slug(Some("HEAD"), dir), "some-repo");
        assert_eq!(heuristic_slug(None, dir), "some-repo");
        assert_eq!(heuristic_slug(Some("///"), dir), "some-repo");
    }

    #[test]
    fn heuristic_falls_back_to_a_generated_slug() {
        let slug = heuristic_slug(None, Path::new("/"));
        assert!(slug.starts_with("quest-"), "{slug}");
        assert!(validate_slug(&slug).is_ok(), "{slug}");
    }

    #[test]
    fn heuristic_output_is_always_a_valid_slug() {
        for branch in ["Feature/ABC 123", "a--b", "-lead-", "x".repeat(60).as_str()] {
            let slug = heuristic_slug(Some(branch), Path::new("/tmp/repo"));
            assert!(validate_slug(&slug).is_ok(), "`{branch}` produced `{slug}`");
        }
    }

    #[test]
    fn numbered_slugs_stay_valid_and_within_the_limit() {
        assert_eq!(numbered("foo", 2), "foo-2");
        assert_eq!(numbered("foo", 99), "foo-99");
        let long = "x".repeat(SLUG_MAX);
        let slug = numbered(&long, 12);
        assert_eq!(slug.len(), SLUG_MAX);
        assert!(validate_slug(&slug).is_ok(), "{slug}");
        // Trimming must not leave a dangling separator behind.
        let dashed = format!("{}-a", "y".repeat(SLUG_MAX - 2));
        assert!(validate_slug(&numbered(&dashed, 7)).is_ok());
    }

    #[test]
    fn shell_quote_leaves_safe_words_alone() {
        assert_eq!(shell_quote("foo/master"), "foo/master");
        assert_eq!(shell_quote("a-b_c.d:e,f=g+h%i@j"), "a-b_c.d:e,f=g+h%i@j");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("`whoami`"), "'`whoami`'");
    }

    #[test]
    fn prompt_sources_are_trimmed_and_optional() {
        assert_eq!(resolve_prompt(None, None).unwrap(), None);
        assert_eq!(
            resolve_prompt(Some("  hi  "), None).unwrap(),
            Some("hi".to_string())
        );
        assert_eq!(resolve_prompt(Some("   "), None).unwrap(), None);
    }

    fn quest_with_epic(epic: Option<&str>) -> Quest {
        let mut quest = Quest::new("slug", "/tmp", "machine");
        quest.beads_epic = epic.map(str::to_string);
        quest
    }

    /// A `Ctx` whose `bd` is the given stub, so the rollback runs against the
    /// same seam the TUI and the CLI use.
    fn ctx_with(bd: &std::sync::Arc<beads::stub::StubBd>) -> Ctx {
        Ctx::for_tests(
            crate::config::Config::default(),
            crate::db::Db::open_in_memory().unwrap(),
            Box::new(crate::tmux::FixtureTmux::new(std::path::PathBuf::from(
                "/nonexistent/tmux.json",
            ))),
        )
        .with_bd(Box::new(bd.clone()))
    }

    #[test]
    fn a_rolled_back_quest_closes_the_epic_it_had_already_minted() {
        let bd = std::sync::Arc::new(beads::stub::StubBd::working("bd-7fx"));
        let ctx = ctx_with(&bd);
        abandon_epic(&ctx, &quest_with_epic(Some("bd-7fx")));
        assert_eq!(
            bd.closed.lock().unwrap().as_slice(),
            [("bd-7fx".to_string(), "quest creation failed".to_string())]
        );
        assert!(ctx.take_warnings().is_empty());
    }

    #[test]
    fn a_rollback_with_no_epic_asks_bd_for_nothing() {
        let bd = std::sync::Arc::new(beads::stub::StubBd::working("bd-7fx"));
        let ctx = ctx_with(&bd);
        abandon_epic(&ctx, &quest_with_epic(None));
        assert!(bd.closed.lock().unwrap().is_empty());
    }

    /// The rollback's own warning is buffered on the `Ctx`, never written: the
    /// TUI reaches this path with the alternate screen up, and a stray stderr
    /// write there tears the frame and is never repainted (B1).
    #[test]
    fn a_bd_that_refuses_the_rollback_is_survivable_and_says_so_in_data() {
        let bd = std::sync::Arc::new(beads::stub::StubBd::failing("bd is wedged"));
        let ctx = ctx_with(&bd);
        abandon_epic(&ctx, &quest_with_epic(Some("bd-7fx")));
        assert_eq!(bd.closed.lock().unwrap().len(), 1);
        let warnings = ctx.take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("bd close bd-7fx"), "{warnings:?}");
        // Drained: nothing is carried into the next command.
        assert!(ctx.take_warnings().is_empty());
    }

    /// D5: `q new` must not snapshot the Quest's workflow onto the *master*
    /// session row. If it did, a later `q workflow set` would change the Quest's
    /// column but never the master's own brief — which reads the session's over
    /// the Quest's. Left unset, the master falls through to the Quest's, so the
    /// value the master reads is always the current one.
    #[test]
    fn the_master_session_does_not_snapshot_the_quests_workflow() {
        let fixture = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_tests(
            crate::config::Config::default(),
            crate::db::Db::open_in_memory().unwrap(),
            Box::new(crate::tmux::FixtureTmux::new(
                fixture.path().join("tmux.json"),
            )),
        );
        let mut quest = Quest::new("wq", "/tmp", "laptop");
        quest.workflow = Some("orchestrator".to_string());
        let quest = ctx.db().unwrap().insert_quest(&quest).unwrap();

        let master = spawn_master(&ctx, &quest, None).unwrap();
        assert_eq!(
            master.session.workflow, None,
            "the master inherits the Quest's workflow at read time, never a snapshot"
        );
        // The Quest still carries it, so the master's brief resolves it.
        assert_eq!(quest.workflow.as_deref(), Some("orchestrator"));
    }

    #[test]
    fn the_repo_flag_is_validated_and_refused_alongside_no_beads() {
        let args = Args {
            repo: Some("  quest "),
            ..Args::default()
        };
        assert_eq!(repo_flag(&args).unwrap(), Some("quest".to_string()));
        assert!(repo_flag(&Args::default()).unwrap().is_none());

        let bad = Args {
            repo: Some("evil,repo:other"),
            ..Args::default()
        };
        assert!(repo_flag(&bad).is_err());

        let contradictory = Args {
            repo: Some("quest"),
            no_beads: true,
            ..Args::default()
        };
        let err = repo_flag(&contradictory).unwrap_err().to_string();
        assert!(err.contains("--no-beads"), "{err}");
    }
}
