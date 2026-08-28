//! `q tpl` — the template CRUD of SPEC §11, where **the database is the source
//! of truth** and TOML is only how a definition travels.
//!
//! ```text
//! q tpl list | show | add | edit | rm | run | export | import | from
//! ```
//!
//! The rules this module had to pick, and why:
//!
//! * **A run that cannot be filled in does not happen.** `{{arg.k}}` with no
//!   `--arg k=…`, or any `{{…}}` that is neither `date` nor `arg.<key>`, fails
//!   the command and names every offending key **of every field** at once
//!   ([`expand_fields`]). The alternative — leaving the braces in — hands an
//!   agent a prompt with a hole in it, which is worse than not starting. Text
//!   that means the braces literally doubles them
//!   ([`crate::templates::escape`]).
//! * **A directory is checked where it has to exist.** `--cwd` is
//!   canonicalized when it is *set*, exactly as `q new --dir` is, so `--cwd .`
//!   pins the directory it was typed in. It is checked again at `q tpl run`,
//!   and nowhere else: re-checking it on an unrelated `q tpl edit
//!   --description` refuses an edit over a directory the edit never touched,
//!   and refusing it at import time would break
//!   `q tpl export | ssh <alias> q tpl import -` for every template whose
//!   directory the far machine has not checked out yet.
//! * **Run stats are history, not definition.** `run_count` and `last_run_at`
//!   never leave through `q tpl export` and are never written by
//!   `q tpl import --replace`: overwriting a definition says nothing about how
//!   often it has been used, and a file that could rewrite that would make the
//!   Templates tab's "last run" a claim rather than a record.
//! * **An import is all or nothing.** A file with a name that already exists
//!   (without `--replace`), a duplicate inside the file, or a bad field in the
//!   third of five templates leaves the database exactly as it was.
//! * **Deleting a template does not delete its Quests** — see
//!   [`crate::db::Db::delete_template`].
//! * **`q tpl` stays on this machine, except `q tpl run --machine`.** A
//!   template is a row in *this* machine's database, like the config and the
//!   hooks, so `q tpl list`/`add`/`edit`/… are refused a `--machine <other>`
//!   rather than silently creating a local Quest stamped `ws` — the very row
//!   `src/tui/quests.rs`'s machine select stopped producing ([`refuse_remote`]).
//!   `q tpl run <name> --machine ws` is the one that travels: it is intercepted
//!   by [`crate::commands::proxy`] *before* it reaches here, sent over ssh, and
//!   the far end instantiates **its own** template of that name — the definition
//!   `--machine ws` explicitly asked for. See [`crate::commands::proxy::route`].

use std::collections::BTreeMap;
use std::io::Write;

use crate::Ctx;
use crate::cli::{TplAction, TplFields};
use crate::commands::{AttachMode, attach_mode, confirm, flush_warnings, new};
use crate::db::Db;
use crate::error::QError;
use crate::model::{Quest, SessionRole, Template};
use crate::output;
use crate::templates::{self, Definition};

pub fn run(ctx: &Ctx, action: &TplAction) -> anyhow::Result<()> {
    refuse_remote(ctx)?;
    match action {
        TplAction::List => list(ctx),
        TplAction::Show { name } => show(ctx, name),
        TplAction::Add { name, fields } => add(ctx, name, fields),
        TplAction::Edit { name, fields } => edit(ctx, name, fields),
        TplAction::Rm { name, force } => rm(ctx, name, *force),
        TplAction::Run { name, args, detach } => instantiate(ctx, name, args, *detach),
        TplAction::Export { name } => export(ctx, name.as_deref()),
        TplAction::Import { path, replace } => import(ctx, path, *replace),
        TplAction::From { quest, name } => from_quest(ctx, quest, name),
    }
}

/// `--machine <other>` is an error for every `q tpl` subcommand *that reaches
/// here* — and for the TUI's Templates tab, which runs the same definitions
/// through the same [`instantiate_with`] (`src/tui/templates.rs`).
///
/// `q tpl run <name> --machine ws` never reaches here: [`crate::commands::proxy`]
/// sends it over ssh, and the far end runs it with no `--machine` at all. So the
/// only `run` this sees is a local one (`--machine` absent, or naming this
/// machine), which is honoured. Every other subcommand names a definition that
/// is this machine's row: `q --machine ws tpl edit x` used to create nothing
/// remote, and `q --machine ws tpl run x` used to create the Quest here and
/// record it as living on `ws` — a local row indistinguishable from a real
/// remote one. Refusing says what the user asked for cannot be done and names
/// the thing that can.
pub fn refuse_remote(ctx: &Ctx) -> anyhow::Result<()> {
    let Some(machine) = ctx.machine_filter() else {
        return Ok(());
    };
    if machine == ctx.config.machine.name {
        return Ok(());
    }
    Err(QError::Invalid(format!(
        "--machine {machine}: templates are rows in this machine's database and `q tpl` \
         never reaches another one; copy one over with \
         `q tpl export <name> | ssh {machine} q tpl import -`"
    ))
    .into())
}

fn list(ctx: &Ctx) -> anyhow::Result<()> {
    let rows = ctx.db()?.list_templates()?;
    if ctx.json || !ctx.quiet {
        output::emit(ctx.json, &rows, || human_list(&rows))?;
    }
    Ok(())
}

fn show(ctx: &Ctx, target: &str) -> anyhow::Result<()> {
    let template = ctx.db()?.resolve_template(target)?;
    if ctx.json || !ctx.quiet {
        output::emit(ctx.json, &template, || human_show(&template))?;
    }
    Ok(())
}

fn add(ctx: &Ctx, name: &str, fields: &TplFields) -> anyhow::Result<()> {
    let definition = patched(&Definition::default(), name, fields)?;
    let stored = create(ctx, &definition)?;
    report(ctx, &stored, "created")
}

/// `q tpl add`'s store step as a library call — the same `cwd` pinning, the
/// same name validation, the same field checks.
///
/// The TUI's add form goes through here rather than through a second copy of
/// them (SPEC §17), so a name the CLI refuses is a name the form refuses, with
/// the same message.
pub fn create(ctx: &Ctx, definition: &Definition) -> anyhow::Result<Template> {
    let mut definition = definition.clone();
    pin_cwd(&mut definition, None)?;
    check_workflow(ctx, &definition, None)?;
    insert(ctx.db()?, &definition)
}

/// `q tpl edit`: a patch when any field flag is given, the editor otherwise.
fn edit(ctx: &Ctx, target: &str, fields: &TplFields) -> anyhow::Result<()> {
    let db = ctx.db()?;
    let current = db.resolve_template(target)?;
    let definition = if fields.any() {
        patched(&Definition::of(&current), &current.name, fields)?
    } else {
        from_editor(&current)?
    };
    let stored = save(ctx, &current, &definition)?;
    report(ctx, &stored, "updated")
}

/// `q tpl edit`'s store step as a library call — the TUI's edit form
/// (SPEC §17), which fills every field rather than patching a few.
///
/// `current` is the row being written over; its id and its run stats survive,
/// which is what makes an edit an edit rather than a delete and an add.
pub fn save(ctx: &Ctx, current: &Template, definition: &Definition) -> anyhow::Result<Template> {
    let db = ctx.db()?;
    let mut definition = definition.clone();
    // Trimmed here as `insert` and `import` trim: a hand-edited TOML is
    // exactly where a stray space around a rename happens.
    definition.name = definition.name.trim().to_string();
    if definition.name != current.name {
        new::validate_template_name(&definition.name)?;
        taken(db, &definition.name)?;
    }
    pin_cwd(&mut definition, current.cwd.as_deref())?;
    check_workflow(ctx, &definition, current.workflow.as_deref())?;
    let mut row = current.clone();
    definition.apply(&mut row);
    check(&row)?;
    db.update_template(&current.id, &row)
}

/// The template's TOML, through `$EDITOR` and back (SPEC §11). Never launched
/// by a test — see [`crate::editor`].
fn from_editor(current: &Template) -> anyhow::Result<Definition> {
    let before = templates::render(std::slice::from_ref(current))?;
    let after = crate::editor::edit(&before, ".toml")?;
    let mut doc = templates::parse(&after)?;
    match doc.templates.len() {
        1 => Ok(doc.templates.remove(0)),
        0 => Err(QError::Invalid(
            "the edited file has no [[template]]; nothing was changed".to_string(),
        )
        .into()),
        n => Err(QError::Invalid(format!(
            "the edited file has {n} templates; `q tpl edit` changes exactly one \
             (use `q tpl import` for a file of many)"
        ))
        .into()),
    }
}

fn rm(ctx: &Ctx, target: &str, force: bool) -> anyhow::Result<()> {
    let db = ctx.db()?;
    let template = db.resolve_template(target)?;
    if !force {
        confirm(ctx, &format!("remove template {}?", template.name))?;
    }
    let unlinked = remove(ctx, &template)?;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({ "template": template, "unlinked_quests": unlinked }),
            || {
                let note = match unlinked {
                    0 => String::new(),
                    1 => " · 1 quest unlinked".to_string(),
                    n => format!(" · {n} quests unlinked"),
                };
                format!("removed template {} ({}){note}", template.name, template.id)
            },
        )?;
    }
    Ok(())
}

/// `q tpl rm`'s delete as a library call, returning how many Quests were
/// unlinked — see [`crate::db::Db::delete_template`] for why they survive it.
pub fn remove(ctx: &Ctx, template: &Template) -> anyhow::Result<usize> {
    ctx.db()?.delete_template(&template.id)
}

/// How many Quests a delete *would* unlink, per template and index-aligned
/// with `templates`.
///
/// The CLI learns the number from the delete itself and prints it afterwards;
/// a confirm box has to say it *beforehand*, which is the whole difference
/// (SPEC §17 `d`). One pass over the Quests rather than a query per template:
/// the Templates tab reloads this on every tick.
pub fn linked_counts(ctx: &Ctx, templates: &[Template]) -> anyhow::Result<Vec<usize>> {
    let quests = ctx.db()?.list_quests(true)?;
    Ok(templates
        .iter()
        .map(|t| {
            quests
                .iter()
                .filter(|q| q.template_id.as_deref() == Some(t.id.as_str()))
                .count()
        })
        .collect())
}

/// `q tpl run` — SPEC §11's instantiation, through the one `q new` code path so
/// a Quest from a template is a Quest in every other respect.
///
/// The run is counted **before** the attach, not after: an attach outside tmux
/// `exec`s and this process is gone, so anything left until afterwards would
/// only ever be recorded for `-d`.
fn instantiate(ctx: &Ctx, target: &str, raw_args: &[String], detach: bool) -> anyhow::Result<()> {
    let template = ctx.db()?.resolve_template(target)?;
    let args = templates::parse_args(raw_args)?;
    let created = instantiate_with(ctx, &template, &args, detach);
    flush_warnings(ctx);
    let created = created?;

    // `new::create` counts the run, so no caller can create the Quest and
    // forget the bookkeeping.
    let template = created.template.clone().unwrap_or(template);
    let attach = attach_mode(ctx, !detach);
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "template": template,
                "quest": created.quest,
                "session": created.session,
                "tmux_session": created.tmux_session,
                "attach": attach,
            }),
            || {
                format!(
                    "created quest {} ({}) from template {} · tmux {} · run: q enter {}",
                    created.quest.id,
                    created.quest.slug,
                    template.name,
                    created.tmux_session,
                    created.quest.slug
                )
            },
        )?;
    }
    if attach != AttachMode::None {
        // An exec attach replaces this process, so nothing buffered survives it.
        std::io::stdout().flush()?;
        ctx.tmux()
            .attach(&created.tmux_session, Some(&created.session.tmux_pane))?;
    }
    Ok(())
}

/// The Quest `q tpl run` makes, as a library call that stops at the attach.
///
/// The TUI's Templates tab lands in the master its own way (SPEC §17: out of
/// TUI mode, through `handoff`, back again unless `[ui] return_after_detach`
/// is off), so it cannot use `instantiate`, which `exec`s tmux over this
/// process. Everything *before* that is identical, and shared from here:
/// the expansion, the `cwd` check, the unused-`--arg` warning, and the one
/// `new::create` that also counts the run.
pub fn instantiate_with(
    ctx: &Ctx,
    template: &Template,
    args: &BTreeMap<String, String>,
    detach: bool,
) -> anyhow::Result<new::Created> {
    let (goal, prompt) = expand_fields(template, &templates::today(), args)?;
    // NULL cwd is SPEC §11's "wherever `q tpl run` was called"; a stored one
    // has to be here *now*, which is the check `check` deliberately does not
    // do at store time.
    let cwd = run_cwd(template)?;
    warn_unused_args(ctx, template, args);
    new::create(
        ctx,
        &new::Args {
            goal: goal.as_deref(),
            dir: cwd.as_deref(),
            workflow: template.workflow.as_deref(),
            repo: template.beads_repo.as_deref(),
            prompt: prompt.as_deref(),
            detach,
            template: Some(template),
            // SPEC §14: a template's stored `create_brain` decides whether the
            // Quest it instantiates gets a brain session — shared by `q tpl
            // run` and the TUI Templates tab, which both land here.
            brain: template.create_brain,
            ..new::Args::default()
        },
    )
}

/// A template with `{{date}}` filled in and **no** `--arg` to give — what the
/// TUI's new-Quest form can offer, since a form has nowhere to type one
/// (SPEC §17).
///
/// A template that wants an argument is refused there rather than quietly
/// instantiated with the braces still in it; the message points at the CLI,
/// which is where `--arg` lives.
pub fn expanded_without_args(template: &Template) -> anyhow::Result<Template> {
    let (goal, master_prompt) = expand_fields(template, &templates::today(), &BTreeMap::new())?;
    let mut out = template.clone();
    out.goal = goal;
    out.master_prompt = master_prompt;
    Ok(out)
}

/// `goal` and `master_prompt`, expanded against the same day and the same
/// `--arg` set — and, when they cannot be, **one** error naming every hole in
/// both. Reporting the first field's alone costs a second failed run to
/// discover what this one already knows.
fn expand_fields(
    template: &Template,
    date: &str,
    args: &BTreeMap<String, String>,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let mut filled: Vec<Option<String>> = Vec::new();
    let mut bad: Vec<(String, templates::Unresolved)> = Vec::new();
    for (field, text) in [
        ("goal", template.goal.as_deref()),
        ("master_prompt", template.master_prompt.as_deref()),
    ] {
        match text.map(|t| templates::expand(t, date, args)) {
            None => filled.push(None),
            Some(Ok(text)) => filled.push(templates::blank_to_none(&text)),
            Some(Err(unresolved)) => {
                filled.push(None);
                bad.push((field.to_string(), unresolved));
            }
        }
    }
    if !bad.is_empty() {
        return Err(templates::unresolved_error(bad));
    }
    Ok((filled[0].clone(), filled[1].clone()))
}

/// A `--arg` no placeholder consumed is almost always a typo
/// (`--arg tikcet=…`), and it is only ever caught today by leaving some
/// *other* key unfilled. A warning rather than an error: the run itself is
/// complete, and refusing it would make an extra argument fatal in a routine
/// that a template edit has just stopped using.
fn warn_unused_args(ctx: &Ctx, template: &Template, args: &BTreeMap<String, String>) {
    let used = wanted_args(template);
    let unused: Vec<&str> = args
        .keys()
        .filter(|k| !used.iter().any(|u| u == *k))
        .map(String::as_str)
        .collect();
    if unused.is_empty() {
        return;
    }
    ctx.warn(format!(
        "warning: template {} has no {} for --arg {}",
        template.name,
        if unused.len() == 1 {
            "placeholder"
        } else {
            "placeholders"
        },
        unused
            .iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
}

/// Every `{{arg.k}}` key this template needs, in the order it first uses them.
///
/// `--arg` is how the CLI supplies them; the TUI has no command line, so this
/// is also what its run form asks for before a routine with arguments can run
/// at all (SPEC §17).
pub fn wanted_args(template: &Template) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for text in [template.goal.as_deref(), template.master_prompt.as_deref()] {
        for key in templates::arg_keys(text.unwrap_or_default()) {
            if !out.contains(&key) {
                out.push(key);
            }
        }
    }
    out
}

fn export(ctx: &Ctx, target: Option<&str>) -> anyhow::Result<()> {
    let db = ctx.db()?;
    let rows = match target {
        Some(target) => vec![db.resolve_template(target)?],
        None => db.list_templates()?,
    };
    let toml = templates::render(&rows)?;
    // `--json` gets the same document as data; the TOML is the human form.
    let doc = templates::Document {
        templates: rows.iter().map(Definition::of).collect(),
    };
    output::emit(ctx.json, &doc, || toml.trim_end().to_string())?;
    Ok(())
}

fn import(ctx: &Ctx, path: &str, replace: bool) -> anyhow::Result<()> {
    let text = read_source(path)?;
    let doc = templates::parse(&text)?;
    // `template = []` is what `q tpl export` writes for an empty database, so
    // a scripted export-then-import of one is a no-op rather than a failure.
    // A file that never mentions `template` is a different thing: not a
    // template file at all.
    if doc.templates.is_empty() && !templates::declares_templates(&text) {
        return Err(QError::Invalid(format!("no [[template]] in {path}")).into());
    }
    let db = ctx.db()?;

    // Whole file or nothing: a half-applied import leaves the user guessing
    // which half.
    let (added, replaced) = db.transaction(|db| {
        let mut added: Vec<String> = Vec::new();
        let mut replaced: Vec<String> = Vec::new();
        for definition in &doc.templates {
            let name = definition.name.trim().to_string();
            new::validate_template_name(&name)?;
            // Absolute, so the file pins a directory; never checked, so a
            // definition may arrive before the directory does.
            let mut definition = definition.clone();
            definition.cwd = portable_cwd(&definition.cwd);
            let definition = &definition;
            if added.contains(&name) || replaced.contains(&name) {
                return Err(
                    QError::Invalid(format!("{path} defines template `{name}` twice")).into(),
                );
            }
            match db.get_template_by_name(&name)? {
                None => {
                    insert(db, definition)?;
                    added.push(name);
                }
                Some(_) if !replace => {
                    return Err(QError::Conflict(format!(
                        "template `{name}` already exists; pass --replace to overwrite it"
                    ))
                    .into());
                }
                Some(existing) => {
                    // The id and the run stats are the row's, not the file's.
                    let mut row = existing.clone();
                    definition.apply(&mut row);
                    check(&row)?;
                    db.update_template(&existing.id, &row)?;
                    replaced.push(name);
                }
            }
        }
        Ok((added, replaced))
    })?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({ "added": added, "replaced": replaced }),
            || {
                let mut parts = Vec::new();
                if !added.is_empty() {
                    parts.push(format!("added {}", added.join(", ")));
                }
                if !replaced.is_empty() {
                    parts.push(format!("replaced {}", replaced.join(", ")));
                }
                if parts.is_empty() {
                    return format!("{path} has no templates; nothing was imported");
                }
                format!(
                    "imported {} template(s) · {}",
                    added.len() + replaced.len(),
                    parts.join(" · ")
                )
            },
        )?;
    }
    Ok(())
}

/// `-` is stdin, anything else is a path.
fn read_source(path: &str) -> anyhow::Result<String> {
    if path == "-" {
        return std::io::read_to_string(std::io::stdin())
            .map_err(|e| QError::Other(format!("cannot read the import from stdin: {e}")).into());
    }
    std::fs::read_to_string(path)
        .map_err(|e| QError::Invalid(format!("cannot read {path}: {e}")).into())
}

/// `q tpl from <quest> <name>` — the Quest's own settings as a definition, plus
/// whatever its master was first told (SPEC §11).
fn from_quest(ctx: &Ctx, target: &str, name: &str) -> anyhow::Result<()> {
    let db = ctx.db()?;
    let quest = db.resolve_quest(target)?;
    let definition = Definition {
        name: name.trim().to_string(),
        description: format!("from quest {}", quest.slug),
        cwd: quest.cwd.clone(),
        workflow: quest.workflow.clone().unwrap_or_default(),
        // The Quest's text was never template syntax: `q new` accepted
        // `{{user.name}}` in it, so capturing it must not be the one place
        // that refuses it. Escaped, it expands back to exactly what the Quest
        // said (`crate::templates::escape`).
        goal: templates::escape(quest.goal.as_deref().unwrap_or_default()),
        master_prompt: templates::escape(&first_master_prompt(db, &quest)?.unwrap_or_default()),
        beads_repo: quest.beads_repo.clone().unwrap_or_default(),
        create_brain: quest.brain_session.is_some(),
        tags: Vec::new(),
    };
    let stored = insert(db, &definition)?;
    report(ctx, &stored, "created")
}

/// What the Quest's **first** master was started with. A Quest that has been
/// resumed has several masters, and a later one's prompt continued work the
/// template is not about — so a first master that was given no prompt means
/// the template has none, rather than borrowing a resume's.
fn first_master_prompt(db: &Db, quest: &Quest) -> anyhow::Result<Option<String>> {
    Ok(db
        .list_sessions_by_quest(&quest.id)?
        .into_iter()
        .find(|s| s.role == SessionRole::Master)
        .and_then(|s| s.first_prompt))
}

/// `definition` as a new row: name validated, name free, fields checked.
fn insert(db: &Db, definition: &Definition) -> anyhow::Result<Template> {
    let name = definition.name.trim();
    new::validate_template_name(name)?;
    taken(db, name)?;
    let mut row = Template::new(name);
    definition.apply(&mut row);
    check(&row)?;
    db.insert_template(&row)
}

fn taken(db: &Db, name: &str) -> anyhow::Result<()> {
    if db.get_template_by_name(name)?.is_some() {
        return Err(QError::Conflict(format!(
            "template `{name}` already exists; edit it with `q tpl edit {name}`"
        ))
        .into());
    }
    Ok(())
}

/// Everything a stored template must satisfy, wherever it came from — a flag,
/// an editor, or an imported file.
///
/// `cwd` is deliberately not among them: a definition is portable and a
/// directory is not, so it is canonicalized when it is *set* ([`pin_cwd`])
/// and required to exist when it is *used* ([`run_cwd`]). See the module docs.
fn check(row: &Template) -> anyhow::Result<()> {
    templates::check_placeholders(&[
        ("goal", row.goal.as_deref()),
        ("master_prompt", row.master_prompt.as_deref()),
    ])?;
    if let Some(repo) = &row.beads_repo {
        crate::beads::validate_repo_label(repo)?;
    }
    // `workflow` is deliberately not among them either, and for the same
    // reason as `cwd`: it names a file in the config directory, which is not
    // part of a portable definition. It is checked when it is *set*
    // ([`check_workflow`]) and required to exist when it is *used* — at
    // `q tpl run`, inside `new::create`. See the module docs.
    Ok(())
}

/// The `workflow` a caller is **setting** (`q tpl add --workflow`, `q tpl edit
/// --workflow`, an editor or a TUI form that changed it), checked against the
/// registry (SPEC §11).
///
/// `was` is what the row already held, so a write that did not touch the field
/// is not a set: `q tpl edit weekly --description "…"` must not fail because
/// the workflow file that template names was deleted last week. That is
/// [`pin_cwd`]'s rule, applied to the other field a definition carries that
/// points outside the database.
fn check_workflow(ctx: &Ctx, definition: &Definition, was: Option<&str>) -> anyhow::Result<()> {
    let workflow = definition.workflow.trim();
    if workflow.is_empty() || Some(workflow) == was {
        return Ok(());
    }
    ctx.workflows().require(workflow)
}

/// The `cwd` a caller is **setting** (`q tpl add --cwd`, `q tpl edit --cwd`,
/// an editor that changed it), canonicalized the way `q new --dir` is
/// (`new::resolve_dir`).
///
/// Storing the raw string is what made `q tpl add rel --cwd .` pin nothing:
/// the template behaved exactly like a NULL `cwd`, re-resolving against
/// whatever directory the routine was later run from. A `cwd` that did not
/// change is left alone — `was` is what the row already held.
fn pin_cwd(definition: &mut Definition, was: Option<&str>) -> anyhow::Result<()> {
    let cwd = definition.cwd.trim().to_string();
    if cwd.is_empty() || Some(cwd.as_str()) == was {
        return Ok(());
    }
    let name = definition.name.trim();
    let path = new::resolve_dir(Some(&cwd)).map_err(|e| cwd_error(name, &cwd, &e))?;
    definition.cwd = path.to_string_lossy().to_string();
    Ok(())
}

/// One line for a `cwd` that will not resolve: ``<name>: cwd `<path>`: <why>``.
///
/// [`new::resolve_dir`]'s error carries its own kind prefix and names the path
/// again, so wrapping it whole read
/// ``not found: t: cwd `/nope`: not found: no such directory: /nope`` — the
/// prefix twice and the path twice. Only the reason is kept, and the kind
/// travels so `--json` still reports `not_found` for a directory that is not
/// there, exactly as `q new --dir /nope` does.
fn cwd_error(name: &str, cwd: &str, e: &anyhow::Error) -> QError {
    let line = |why: &str| format!("{name}: cwd `{cwd}`: {why}");
    match e.downcast_ref::<QError>() {
        Some(QError::NotFound(_)) => QError::NotFound(line("no such directory")),
        Some(QError::Invalid(_)) => QError::Invalid(line("not a directory")),
        // Anything else is `resolve_dir` failing to *read* the path; it names
        // no kind worth repeating, so its own message is the reason.
        _ => QError::Invalid(line(&format!("{e:#}"))),
    }
}

/// A `cwd` an import brought in: made absolute, so it pins a directory rather
/// than following whichever one the importing shell happened to be in, but
/// **never** checked for existence.
///
/// A template is allowed to travel to a machine that has not checked its
/// repository out yet — that is what `q tpl export | ssh <alias> q tpl
/// import -` is for — and one absent directory must not fail an all-or-nothing
/// import of the other nine definitions.
fn portable_cwd(cwd: &str) -> String {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return String::new();
    }
    std::path::absolute(cwd)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| cwd.to_string())
}

/// The directory `q tpl run` will start the master in: the template's, if it
/// has one, and it has to be here now.
fn run_cwd(template: &Template) -> anyhow::Result<Option<String>> {
    let Some(cwd) = template.cwd.as_deref() else {
        return Ok(None);
    };
    match new::resolve_dir(Some(cwd)) {
        Ok(path) => Ok(Some(path.to_string_lossy().to_string())),
        Err(_) => Err(QError::NotFound(format!(
            "template `{}`: its cwd `{cwd}` is not a directory on this machine; \
             point it somewhere else with `q tpl edit {} --cwd <path>`",
            template.name, template.name
        ))
        .into()),
    }
}

/// `base` with every flag that was given written over it. An omitted flag
/// leaves the field alone; a blank one clears it.
fn patched(base: &Definition, name: &str, fields: &TplFields) -> anyhow::Result<Definition> {
    let mut out = base.clone();
    out.name = name.trim().to_string();
    let set = |slot: &mut String, given: Option<&String>| {
        if let Some(value) = given {
            *slot = value.trim().to_string();
        }
    };
    set(&mut out.description, fields.description.as_ref());
    set(&mut out.cwd, fields.cwd.as_ref());
    set(&mut out.workflow, fields.workflow.as_ref());
    set(&mut out.goal, fields.goal.as_ref());
    set(&mut out.beads_repo, fields.repo.as_ref());
    if fields.prompt.is_some() || fields.prompt_file.is_some() {
        out.master_prompt =
            new::resolve_prompt(fields.prompt.as_deref(), fields.prompt_file.as_deref())?
                .unwrap_or_default();
    }
    if fields.brain {
        out.create_brain = true;
    } else if fields.no_brain {
        out.create_brain = false;
    }
    if !fields.tags.is_empty() {
        out.tags = fields.tags.clone();
    }
    Ok(out)
}

/// What the caller said explicitly, for [`Merge`]. Every field is the flag of
/// the same name on `q new`; the TUI's form fills the three it has.
#[derive(Debug, Default)]
pub struct Given<'a> {
    pub goal: Option<&'a str>,
    pub dir: Option<&'a str>,
    pub workflow: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub prompt: Option<&'a str>,
    pub no_beads: bool,
}

/// SPEC §16's `q new --template`: **the template fills the blanks and a value
/// the caller gave always wins.**
///
/// `q new --template` and the TUI's new-Quest form are the same operation
/// asked two ways, so they share this rather than each deciding what "fills
/// the blanks" means — which is how the CLI came to have no `--template` at
/// all while the form had the merge. It is not `q tpl run`: that takes the
/// definition whole and has no `--name`/`--goal` to lose.
#[derive(Debug, Default)]
pub struct Merge {
    pub goal: Option<String>,
    pub dir: Option<String>,
    pub workflow: Option<String>,
    pub repo: Option<String>,
    pub prompt: Option<String>,
}

impl Merge {
    pub fn new(template: Option<&Template>, given: &Given) -> Merge {
        let from = |typed: Option<&str>, of: fn(&Template) -> Option<String>| -> Option<String> {
            typed.map(str::to_string).or_else(|| template.and_then(of))
        };
        Merge {
            goal: from(given.goal, |t| t.goal.clone()),
            dir: from(given.dir, |t| t.cwd.clone()),
            workflow: from(given.workflow, |t| t.workflow.clone()),
            // A *typed* `--repo` alongside `--no-beads` is a contradiction,
            // and `q new` says so. A template's is not the caller's mistake —
            // it is dropped, because there is no epic for the label to go on.
            repo: given.repo.map(str::to_string).or_else(|| {
                template
                    .and_then(|t| t.beads_repo.clone())
                    .filter(|_| !given.no_beads)
            }),
            prompt: from(given.prompt, |t| t.master_prompt.clone()),
        }
    }
}

/// The template `q new --template` / the TUI form starts from: resolved, and
/// with `{{date}}` filled in. A template that wants a `{{arg.k}}` is refused
/// with the one command that can give it one.
pub fn for_new(ctx: &Ctx, target: &str) -> anyhow::Result<Template> {
    let template = ctx.db()?.resolve_template(target)?;
    expanded_without_args(&template).map_err(|e| {
        QError::Invalid(format!(
            "run it from the CLI: q tpl run {} --arg k=v — {e:#}",
            template.name
        ))
        .into()
    })
}

fn report(ctx: &Ctx, template: &Template, verb: &str) -> anyhow::Result<()> {
    if ctx.json || !ctx.quiet {
        output::emit(ctx.json, template, || {
            format!("{verb} template {} ({})", template.name, template.id)
        })?;
    }
    Ok(())
}

fn human_list(rows: &[Template]) -> String {
    if rows.is_empty() {
        return "no templates".to_string();
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|t| {
            vec![
                t.name.clone(),
                super::fmt::oneline(t.description.as_deref().unwrap_or("-"), 48),
                super::fmt::or_dash(t.workflow.as_deref()),
                t.run_count.to_string(),
                t.last_run_at
                    .map(super::fmt::age)
                    .unwrap_or("-".to_string()),
            ]
        })
        .collect();
    super::fmt::table(
        &["NAME", "DESCRIPTION", "WORKFLOW", "RUNS", "LAST RUN"],
        &cells,
    )
}

fn human_show(t: &Template) -> String {
    let mut out = format!("{} ({})", t.name, t.id);
    let mut line = |label: &str, value: String| {
        out.push_str(&format!("\n  {label:<14}{value}"));
    };
    line("description", super::fmt::or_dash(t.description.as_deref()));
    line(
        "cwd",
        t.cwd
            .as_deref()
            .map(super::fmt::tilde)
            .unwrap_or_else(|| "- (the current directory at run time)".to_string()),
    );
    line("workflow", super::fmt::or_dash(t.workflow.as_deref()));
    line("goal", super::fmt::or_dash(t.goal.as_deref()));
    line("beads repo", super::fmt::or_dash(t.beads_repo.as_deref()));
    line(
        "brain",
        if t.create_brain { "yes" } else { "no" }.to_string(),
    );
    line(
        "tags",
        t.tags
            .as_ref()
            .filter(|v| !v.is_empty())
            .map(|v| v.join(", "))
            .unwrap_or_else(|| "-".to_string()),
    );
    line("runs", t.run_count.to_string());
    line(
        "last run",
        t.last_run_at
            .map(super::fmt::stamp)
            .unwrap_or_else(|| "never".to_string()),
    );
    if let Some(prompt) = &t.master_prompt {
        out.push_str("\n\nmaster prompt:\n");
        out.push_str(prompt);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::TplFields;

    fn fields() -> TplFields {
        TplFields::default()
    }

    #[test]
    fn a_flag_that_was_not_given_leaves_its_field_alone() {
        let base = Definition {
            goal: "old goal".to_string(),
            description: "old".to_string(),
            create_brain: true,
            tags: vec!["a".to_string()],
            ..Definition::default()
        };

        let mut given = fields();
        given.goal = Some("  new goal  ".to_string());
        let out = patched(&base, "routine", &given).unwrap();

        assert_eq!(out.name, "routine");
        assert_eq!(out.goal, "new goal");
        assert_eq!(out.description, "old");
        assert!(out.create_brain);
        assert_eq!(out.tags, ["a"]);
    }

    #[test]
    fn a_blank_flag_clears_its_field() {
        let base = Definition {
            goal: "old".to_string(),
            tags: vec!["a".to_string(), "b".to_string()],
            create_brain: true,
            ..Definition::default()
        };

        let mut given = fields();
        given.goal = Some(String::new());
        given.tags = vec![String::new()];
        given.no_brain = true;
        let out = patched(&base, "routine", &given).unwrap();

        assert_eq!(out.goal, "");
        assert!(!out.create_brain);
        // A blank tag survives to `apply`, which is what turns it into NULL.
        let mut row = Template::new("routine");
        out.apply(&mut row);
        assert_eq!(row.tags, None);
        assert_eq!(row.goal, None);
    }

    #[test]
    fn a_prompt_flag_is_the_only_thing_that_touches_the_master_prompt() {
        let base = Definition {
            master_prompt: "old".to_string(),
            ..Definition::default()
        };
        assert_eq!(patched(&base, "r", &fields()).unwrap().master_prompt, "old");

        let mut given = fields();
        given.prompt = Some("new".to_string());
        assert_eq!(patched(&base, "r", &given).unwrap().master_prompt, "new");

        let mut blank = fields();
        blank.prompt = Some("   ".to_string());
        assert_eq!(patched(&base, "r", &blank).unwrap().master_prompt, "");
    }

    #[test]
    fn any_reports_whether_the_command_line_said_anything() {
        assert!(!fields().any());
        let mut given = fields();
        given.no_brain = true;
        assert!(given.any());
        let mut tagged = fields();
        tagged.tags = vec!["x".to_string()];
        assert!(tagged.any());
    }

    #[test]
    fn a_stored_template_may_not_carry_a_placeholder_nothing_can_fill() {
        let mut row = Template::new("routine");
        row.goal = Some("{{date}} {{arg.x}}".to_string());
        assert!(check(&row).is_ok());
        row.master_prompt = Some("{{oops}}".to_string());
        let e = check(&row).unwrap_err();
        assert!(e.to_string().contains("master_prompt"), "{e}");
        assert!(e.to_string().contains("oops"), "{e}");
    }

    /// A `cwd` is checked when it is set and when it is run, and at no other
    /// time — `check` runs on every write, including ones that never touched
    /// it (see the module docs).
    #[test]
    fn a_cwd_is_refused_when_it_is_set_and_ignored_when_it_is_not() {
        let mut row = Template::new("routine");
        row.cwd = Some("/definitely/not/here".to_string());
        assert!(check(&row).is_ok(), "an unrelated write must not care");

        let mut setting = Definition {
            name: "routine".to_string(),
            cwd: "/definitely/not/here".to_string(),
            ..Definition::default()
        };
        let e = pin_cwd(&mut setting, None).unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("not_found"),
            "{e}"
        );
        // The field and the template, so the message says what to fix — and
        // one prefix, one path: `resolve_dir`'s error used to be wrapped
        // whole, which said "not found:" twice and named the directory twice.
        assert_eq!(
            e.to_string(),
            "not found: routine: cwd `/definitely/not/here`: no such directory"
        );

        // A path that is there but is not a directory is the other kind, and
        // says so without borrowing "not found".
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut at_a_file = Definition {
            name: "routine".to_string(),
            cwd: file.path().to_string_lossy().to_string(),
            ..Definition::default()
        };
        let e = pin_cwd(&mut at_a_file, None).unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("invalid"),
            "{e}"
        );
        assert!(e.to_string().ends_with(": not a directory"), "{e}");
        assert!(!e.to_string().contains("not found"), "{e}");

        // The same value it already had is not a set at all.
        let mut unchanged = setting.clone();
        pin_cwd(&mut unchanged, Some("/definitely/not/here")).unwrap();
        assert_eq!(unchanged.cwd, "/definitely/not/here");

        // Run time is where it has to exist, named with the template.
        row.name = "routine".to_string();
        let e = run_cwd(&row).unwrap_err();
        assert!(e.to_string().contains("template `routine`"), "{e}");
        assert!(e.to_string().contains("/definitely/not/here"), "{e}");
        row.cwd = None;
        assert_eq!(run_cwd(&row).unwrap(), None);
    }

    /// The blocking half of `--cwd`: a relative one has to pin the directory
    /// it was typed in, exactly as `q new --dir .` does.
    #[test]
    fn a_relative_cwd_is_stored_as_the_directory_it_named() {
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let mut definition = Definition {
            name: "routine".to_string(),
            cwd: dir.to_string_lossy().to_string(),
            ..Definition::default()
        };
        pin_cwd(&mut definition, None).unwrap();
        assert_eq!(definition.cwd, dir.to_string_lossy());
        assert!(
            std::path::Path::new(&definition.cwd).is_absolute(),
            "{}",
            definition.cwd
        );
    }

    #[test]
    fn an_imported_cwd_is_made_absolute_but_never_checked() {
        assert_eq!(portable_cwd(""), "");
        assert_eq!(portable_cwd("  "), "");
        assert_eq!(portable_cwd("/x/gone"), "/x/gone");
        let relative = portable_cwd("gone-for-sure");
        assert!(std::path::Path::new(&relative).is_absolute(), "{relative}");
        assert!(relative.ends_with("gone-for-sure"), "{relative}");
    }

    #[test]
    fn a_template_reports_every_field_it_cannot_fill_in_one_error() {
        let mut row = Template::new("both");
        row.goal = Some("g {{arg.a}}".to_string());
        row.master_prompt = Some("p {{arg.b}}".to_string());
        let e = expand_fields(&row, "2026-08-28", &BTreeMap::new()).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("goal: no --arg for `a`"), "{msg}");
        assert!(msg.contains("master_prompt: no --arg for `b`"), "{msg}");

        let filled = expand_fields(
            &row,
            "2026-08-28",
            &[
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
            ]
            .into_iter()
            .collect(),
        )
        .unwrap();
        assert_eq!(filled.0.as_deref(), Some("g 1"));
        assert_eq!(filled.1.as_deref(), Some("p 2"));
    }

    #[test]
    fn an_empty_listing_says_so_rather_than_printing_a_bare_header() {
        assert_eq!(human_list(&[]), "no templates");
        let mut t = Template::new("weekly-hygiene");
        t.run_count = 3;
        let rendered = human_list(std::slice::from_ref(&t));
        assert!(rendered.starts_with("NAME"), "{rendered}");
        assert!(rendered.contains("weekly-hygiene"), "{rendered}");
    }

    #[test]
    fn show_names_every_field_including_the_empty_ones() {
        let t = Template::new("weekly-hygiene");
        let rendered = human_show(&t);
        for label in ["description", "cwd", "workflow", "goal", "runs", "last run"] {
            assert!(rendered.contains(label), "{label} missing:\n{rendered}");
        }
        assert!(rendered.contains("never"), "{rendered}");
        // A NULL cwd is not "nothing", it is "wherever the run happens".
        assert!(rendered.contains("the current directory"), "{rendered}");
    }
}
