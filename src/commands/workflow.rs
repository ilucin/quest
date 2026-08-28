//! `q workflow` — SPEC §11's workflow files.
//!
//! ```text
//! q workflow list | show | add | edit | rm | set
//! ```
//!
//! The registry itself is [`crate::workflows`]; this is the CLI over it, and it
//! follows the conventions `q tpl` established (`crate::commands::tpl`):
//! `--json` on every subcommand, `output::emit` for the one payload, `confirm`
//! before a delete, and `--machine <other>` refused rather than ignored.
//!
//! Two decisions worth naming:
//!
//! * **`q workflow set` is `q set <quest> workflow` under another spelling.**
//!   SPEC §11 lists it, SPEC §16 lists the other, and they are one write. It
//!   delegates rather than duplicating, so there is one validation, one
//!   `quest.updated` event and one payload — and `q workflow set` on a Quest
//!   that lives on another machine travels exactly as `q set` does.
//! * **`q workflow edit <builtin>` copies before it opens.** Editing an
//!   embedded string is not a thing, and opening an empty buffer over a
//!   built-in would silently replace it with whatever the user typed. The copy
//!   is written only after the editor comes back with something.

use crate::Ctx;
use crate::cli::{SetKey, WorkflowAction};
use crate::commands::confirm;
use crate::error::QError;
use crate::output;
use crate::workflows::{Entry, Registry, Source, Workflow};

pub fn run(ctx: &Ctx, action: &WorkflowAction) -> anyhow::Result<()> {
    match action {
        // Only `set` names a Quest, and a Quest can be elsewhere; the file
        // subcommands are about this machine's config directory.
        WorkflowAction::Set { quest, name } => return set(ctx, quest, name),
        _ => refuse_remote(ctx)?,
    }
    let registry = ctx.workflows();
    match action {
        WorkflowAction::List => list(ctx, &registry),
        WorkflowAction::Show { name, worker } => show(ctx, &registry, name, *worker),
        WorkflowAction::Add { name, file } => add(ctx, &registry, name, file.as_deref()),
        WorkflowAction::Edit { name, file } => edit(ctx, &registry, name, file.as_deref()),
        WorkflowAction::Rm { name, force } => rm(ctx, &registry, name, *force),
        WorkflowAction::Set { .. } => unreachable!("handled above"),
    }
}

/// `--machine <other>` cannot mean anything for a workflow file: the files are
/// in *this* machine's config directory, and `q --machine ws workflow show x`
/// would print this machine's copy while claiming to be about `ws`. Refused,
/// with the way to move one — the same shape as `q tpl`'s
/// ([`crate::commands::tpl::refuse_remote`]).
fn refuse_remote(ctx: &Ctx) -> anyhow::Result<()> {
    let Some(machine) = ctx.machine_filter() else {
        return Ok(());
    };
    if machine == ctx.config.machine.name {
        return Ok(());
    }
    Err(QError::Invalid(format!(
        "--machine {machine}: workflows are files in this machine's config directory and \
         `q workflow` never reaches another one; copy one over with \
         `q workflow show <name> | ssh {machine} q workflow add <name> --file -`"
    ))
    .into())
}

fn list(ctx: &Ctx, registry: &Registry) -> anyhow::Result<()> {
    let rows = registry.list()?;
    if ctx.json || !ctx.quiet {
        output::emit(ctx.json, &rows, || human_list(&rows))?;
    }
    Ok(())
}

fn show(ctx: &Ctx, registry: &Registry, name: &str, worker: bool) -> anyhow::Result<()> {
    // Trim as the set paths do (`check_opt`), so `q workflow show " solo "` and
    // `q new --workflow " solo "` agree on what the name is instead of one
    // accepting the string and the other refusing it.
    let name = name.trim();
    let workflow = registry.get(name)?;
    // `q tpl show`'s gate, which the module docs say this follows.
    if !ctx.json && ctx.quiet {
        return Ok(());
    }
    // `--worker` answers "what would a worker actually be handed", which is the
    // question a workflow author has while writing the `## worker` section.
    let part = workflow.for_role(if worker {
        crate::model::SessionRole::Worker
    } else {
        crate::model::SessionRole::Master
    });
    let text = part.text().to_string();
    let whole_for_worker = matches!(part, crate::workflows::Part::WholeForWorker(_));
    // The brief says this out loud, and `--worker` asks the same question the
    // brief answers. On stderr so `q workflow show x --worker | …` still pipes
    // the workflow and nothing else.
    if whole_for_worker && !ctx.json {
        eprintln!(
            "note: `{name}` defines no `## worker` section, so this is the whole file — \
             the master's copy, which is what a worker would be handed"
        );
    }
    output::emit(
        ctx.json,
        &serde_json::json!({
            "name": workflow.name,
            "source": workflow.source,
            "path": workflow.path,
            "for": if worker { "worker" } else { "master" },
            "has_worker_section": crate::workflows::worker_section(&workflow.body).is_some(),
            "whole_file": !worker || whole_for_worker,
            "body": text,
        }),
        || text.trim_end().to_string(),
    )
}

fn add(ctx: &Ctx, registry: &Registry, name: &str, file: Option<&str>) -> anyhow::Result<()> {
    crate::workflows::validate_name(name)?;
    if registry.file(name)?.is_some() {
        return Err(QError::Conflict(format!(
            "workflow `{name}` already exists; change it with `q workflow edit {name}`"
        ))
        .into());
    }
    // Shadowing a built-in is supported, but `add` is the wrong door for it:
    // it would open an empty skeleton over a workflow that already has a body,
    // and `:wq` would replace `orchestrator` with four comment lines.
    if crate::workflows::is_builtin(name) {
        return Err(QError::Conflict(format!(
            "`{name}` is a built-in workflow; `q workflow edit {name}` opens its text and \
             saves your copy over it. `q workflow add` is for a name that is not taken"
        ))
        .into());
    }
    let body = match file {
        Some(path) => read_source(path)?,
        None => crate::editor::edit(&template_for(name), ".md")?,
    };
    let path = registry.write(name, &body)?;
    report(ctx, registry, name, &path, "created")
}

fn edit(ctx: &Ctx, registry: &Registry, name: &str, file: Option<&str>) -> anyhow::Result<()> {
    // `get` first: an unknown name must fail before an editor opens, and its
    // error is the one that lists what does exist.
    let current = registry.get(name)?;
    let body = match file {
        Some(path) => read_source(path)?,
        // SPEC §11: editing a built-in opens the built-in's own text, and the
        // save is what creates the shadowing file.
        None => crate::editor::edit(&current.body, ".md")?,
    };
    let path = registry.write(name, &body)?;
    let verb = if current.source == Source::Builtin {
        "copied and updated"
    } else {
        "updated"
    };
    report(ctx, registry, name, &path, verb)
}

fn rm(ctx: &Ctx, registry: &Registry, name: &str, force: bool) -> anyhow::Result<()> {
    // Whether there is a file at all decides both questions: a built-in with
    // no file is about to be refused, and asking "remove workflow review?"
    // before saying "there is nothing to remove" is a confirm for a delete
    // that was never going to happen.
    let file = registry.file(name)?;
    let reveals = crate::workflows::is_builtin(name) && file.is_some();
    if !force && file.is_some() {
        let note = if reveals {
            format!("remove your workflow {name}? (the built-in comes back)")
        } else {
            format!("remove workflow {name}?")
        };
        confirm(ctx, &note)?;
    }
    let path = registry.remove(name)?;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "name": name,
                "path": path,
                "removed": true,
                "reveals_builtin": reveals,
            }),
            || {
                let tail = if reveals {
                    " · the built-in is visible again"
                } else {
                    ""
                };
                format!("removed workflow {name} ({}){tail}", path.display())
            },
        )?;
    }
    Ok(())
}

/// SPEC §11's `q workflow set <quest> <name>`, which is SPEC §16's
/// `q set <quest> workflow <name>` — one write, one event, one payload. See the
/// module docs.
fn set(ctx: &Ctx, quest: &str, name: &str) -> anyhow::Result<()> {
    crate::commands::set::run(ctx, quest, SetKey::Workflow, name)
}

/// `-` is stdin, anything else is a path — as `q tpl import` reads its file.
fn read_source(path: &str) -> anyhow::Result<String> {
    if path == "-" {
        return std::io::read_to_string(std::io::stdin()).map_err(|e| {
            QError::Other(format!("cannot read the workflow from stdin: {e}")).into()
        });
    }
    std::fs::read_to_string(path)
        .map_err(|e| QError::Invalid(format!("cannot read {path}: {e}")).into())
}

/// What `q workflow add` opens the editor on: enough of a skeleton that the
/// two things a workflow author has to know — the H1 and the `## worker`
/// split — are in front of them rather than in the help text.
fn template_for(name: &str) -> String {
    format!(
        "# {name}\n\n\
         <!-- The whole of this file goes into the master's brief (section 3).\n\
         \x20    Be concrete: which `q` commands to call, and when. -->\n\n\
         ## worker\n\n\
         <!-- Only this section goes into a worker's brief. Delete the heading\n\
         \x20    and workers get the whole file instead. -->\n"
    )
}

fn report(
    ctx: &Ctx,
    registry: &Registry,
    name: &str,
    path: &std::path::Path,
    verb: &str,
) -> anyhow::Result<()> {
    // Re-read so the payload reports what is actually on disk now, including
    // whether the write turned a built-in into a shadow.
    let workflow = registry.get(name)?;
    if ctx.json || !ctx.quiet {
        output::emit(ctx.json, &payload(&workflow, path, verb), || {
            format!(
                "{verb} workflow {name} ({}) · {}",
                path.display(),
                workflow.source
            )
        })?;
    }
    // A shadow is the one state that surprises people later ("I edited it and
    // `q` still runs the old one" is the mirror of it), so it is said once,
    // here, rather than only in `q workflow list`.
    if workflow.source == Source::Shadow && !ctx.quiet && !ctx.json {
        eprintln!(
            "note: this file now shadows the built-in `{name}`; \
             `q workflow rm {name}` brings the built-in back"
        );
    }
    // Only the first `## worker` section is read; a second is silently
    // master-only text as far as a worker is concerned. Say so once, on save,
    // rather than let the author wonder why half their worker prose never
    // reaches a worker.
    if crate::workflows::worker_heading_count(&workflow.body) > 1 && !ctx.quiet && !ctx.json {
        eprintln!(
            "note: `{name}` defines more than one `## worker` section; only the first is \
             used — the rest stays in the master's copy"
        );
    }
    Ok(())
}

fn payload(workflow: &Workflow, path: &std::path::Path, verb: &str) -> serde_json::Value {
    serde_json::json!({
        "name": workflow.name,
        "source": workflow.source,
        "path": path,
        "action": verb,
        "has_worker_section": crate::workflows::worker_section(&workflow.body).is_some(),
        "chars": workflow.body.chars().count(),
    })
}

fn human_list(rows: &[Entry]) -> String {
    if rows.is_empty() {
        return "no workflows".to_string();
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|e| {
            vec![
                e.name.clone(),
                e.source.as_str().to_string(),
                if e.has_worker_section { "yes" } else { "-" }.to_string(),
                super::fmt::oneline(&e.summary, 48),
            ]
        })
        .collect();
    super::fmt::table(&["NAME", "SOURCE", "WORKER", "SUMMARY"], &cells)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::BUILTIN;

    #[test]
    fn an_empty_listing_says_so_and_a_full_one_marks_the_source() {
        assert_eq!(human_list(&[]), "no workflows");
        let registry = Registry::new("/nonexistent/q/workflows");
        let rendered = human_list(&registry.list().unwrap());
        assert!(rendered.starts_with("NAME"), "{rendered}");
        for (name, _) in BUILTIN {
            assert!(rendered.contains(name), "{name} missing:\n{rendered}");
        }
        assert!(rendered.contains("builtin"), "{rendered}");
    }

    #[test]
    fn the_add_skeleton_shows_both_halves_of_the_split() {
        let skeleton = template_for("triage");
        assert!(skeleton.starts_with("# triage\n"), "{skeleton}");
        assert!(skeleton.contains("\n## worker\n"), "{skeleton}");
        // It has to be a legal workflow body, or `q workflow add` would refuse
        // the very buffer it just handed the user.
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path());
        registry.write("triage", &skeleton).unwrap();
        assert!(registry.get("triage").unwrap().source == Source::User);
    }
}
