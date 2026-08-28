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
//!   the command and names every offending key at once
//!   ([`crate::templates::expand`]). The alternative — leaving the braces in —
//!   hands an agent a prompt with a hole in it, which is worse than not
//!   starting.
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
//! * **`q tpl` is never proxied.** A template is a row in *this* machine's
//!   database, like the config and the hooks; `q --machine ws tpl run x` would
//!   have to mean a template over there, which is not the one the user just
//!   listed. See [`crate::commands::proxy::route`].

use std::collections::BTreeMap;
use std::io::Write;

use crate::Ctx;
use crate::cli::{TplAction, TplFields};
use crate::commands::{AttachMode, attach_mode, confirm, flush_warnings, new};
use crate::db::Db;
use crate::error::QError;
use crate::model::{Quest, SessionRole, Template, now};
use crate::output;
use crate::templates::{self, Definition};

pub fn run(ctx: &Ctx, action: &TplAction) -> anyhow::Result<()> {
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
    let db = ctx.db()?;
    let definition = patched(&Definition::default(), name, fields)?;
    let stored = insert(db, &definition)?;
    report(ctx, &stored, "created")
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
    if definition.name != current.name {
        new::validate_template_name(&definition.name)?;
        taken(db, &definition.name)?;
    }
    let mut row = current.clone();
    definition.apply(&mut row);
    check(&row)?;
    let stored = db.update_template(&current.id, &row)?;
    report(ctx, &stored, "updated")
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
    let unlinked = db.delete_template(&template.id)?;
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

/// `q tpl run` — SPEC §11's instantiation, through the one `q new` code path so
/// a Quest from a template is a Quest in every other respect.
///
/// The run is counted **before** the attach, not after: an attach outside tmux
/// `exec`s and this process is gone, so anything left until afterwards would
/// only ever be recorded for `-d`.
fn instantiate(ctx: &Ctx, target: &str, raw_args: &[String], detach: bool) -> anyhow::Result<()> {
    let db = ctx.db()?;
    let template = db.resolve_template(target)?;
    let args = templates::parse_args(raw_args)?;
    let date = templates::today();
    let goal = fill("goal", template.goal.as_deref(), &date, &args)?;
    let prompt = fill(
        "master_prompt",
        template.master_prompt.as_deref(),
        &date,
        &args,
    )?;

    let created = new::create(
        ctx,
        &new::Args {
            goal: goal.as_deref(),
            // NULL cwd is SPEC §11's "wherever `q tpl run` was called".
            dir: template.cwd.as_deref(),
            workflow: template.workflow.as_deref(),
            repo: template.beads_repo.as_deref(),
            prompt: prompt.as_deref(),
            detach,
            template: Some(&template.id),
            ..new::Args::default()
        },
    );
    flush_warnings(ctx);
    let created = created?;

    let template = db.bump_template_run(&template.id, now())?;
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

/// A template with `{{date}}` filled in and **no** `--arg` to give — what the
/// TUI's new-Quest form can offer, since a form has nowhere to type one
/// (SPEC §17).
///
/// A template that wants an argument is refused there rather than quietly
/// instantiated with the braces still in it; the message points at the CLI,
/// which is where `--arg` lives.
pub fn expanded_for_form(template: &Template) -> anyhow::Result<Template> {
    let date = templates::today();
    let args = BTreeMap::new();
    let mut out = template.clone();
    out.goal = fill("goal", template.goal.as_deref(), &date, &args)?;
    out.master_prompt = fill(
        "master_prompt",
        template.master_prompt.as_deref(),
        &date,
        &args,
    )?;
    Ok(out)
}

/// One placeholder-expanded field, or an error naming what could not be filled.
fn fill(
    field: &str,
    text: Option<&str>,
    date: &str,
    args: &BTreeMap<String, String>,
) -> anyhow::Result<Option<String>> {
    let Some(text) = text else {
        return Ok(None);
    };
    let filled = templates::expand(text, date, args).map_err(|bad| bad.into_error(field))?;
    Ok(templates::blank_to_none(&filled))
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
    if doc.templates.is_empty() {
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
        goal: quest.goal.clone().unwrap_or_default(),
        master_prompt: first_master_prompt(db, &quest)?.unwrap_or_default(),
        beads_repo: quest.beads_repo.clone().unwrap_or_default(),
        create_brain: quest.brain_session.is_some(),
        tags: Vec::new(),
    };
    let stored = insert(db, &definition)?;
    report(ctx, &stored, "created")
}

/// What the Quest's first master was started with. A Quest that has been
/// resumed has several masters; the first one is the one whose prompt started
/// the work.
fn first_master_prompt(db: &Db, quest: &Quest) -> anyhow::Result<Option<String>> {
    Ok(db
        .list_sessions_by_quest(&quest.id)?
        .into_iter()
        .filter(|s| s.role == SessionRole::Master)
        .find_map(|s| s.first_prompt))
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
fn check(row: &Template) -> anyhow::Result<()> {
    templates::check_placeholders("goal", row.goal.as_deref())?;
    templates::check_placeholders("master_prompt", row.master_prompt.as_deref())?;
    if let Some(cwd) = &row.cwd {
        // Caught here rather than at `q tpl run`, when a routine is halfway
        // through starting a tmux session.
        new::resolve_dir(Some(cwd))?;
    }
    if let Some(repo) = &row.beads_repo {
        crate::beads::validate_repo_label(repo)?;
    }
    // TODO(bd-8lz.6.3): validate `workflow` against the workflow registry; it
    // is a stored string until the registry exists.
    Ok(())
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

    #[test]
    fn a_cwd_that_is_not_a_directory_is_refused_before_it_is_stored() {
        let mut row = Template::new("routine");
        row.cwd = Some("/definitely/not/here".to_string());
        let e = check(&row).unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("not_found"),
            "{e}"
        );
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
