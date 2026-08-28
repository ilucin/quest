//! Templates tab (SPEC §11, §17): the routine list, the one-keypress run, the
//! add/edit form, the delete confirm and the TOML export.
//!
//! Nothing about a template is decided here. The listing is
//! [`crate::db::Db::list_templates`], the same rows and the same order
//! `q tpl list` prints; storing one is [`tpl::create`] / [`tpl::save`],
//! deleting one is [`tpl::remove`], running one is [`tpl::instantiate_with`],
//! and the TOML is [`crate::templates::render`]. This module turns those into
//! lines and keys, so a name the CLI refuses is a name the form refuses, with
//! the same message.
//!
//! **`x` stays "refresh now".** SPEC §11 gives `x` to export on this tab,
//! which collides with the global reload every other tab has (SPEC §17, and
//! `App::handle_global`, which claims it before a tab ever sees it). Honouring
//! §11 would mean one key meaning "reload" on three tabs and "hand the
//! terminal to a pager" on the fourth — a surprise exactly where muscle memory
//! is strongest, for the sake of a letter. Export is `X` instead: the same
//! letter, shifted, unused by anything else, and named in both the footer and
//! the `?` overlay.

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::Ctx;
use crate::commands::{fmt, tpl};
use crate::error::QError;
use crate::model::Template;
use crate::templates::Definition;

use super::app::{Action, App, Prompt, TemplateTarget};
use super::form::Form;
use super::keys::Input;
use super::layout::{self, RowMode};

/// The tab's own half of the `?` overlay.
pub const HELP: &[(&str, &str)] = &[
    ("Enter / r", "run it (one keypress; {{arg.k}} asks first)"),
    ("a / e", "add · edit (a form)"),
    ("d", "delete (says how many quests lose the link)"),
    ("X", "export its TOML to a pager (x is still refresh)"),
    ("g / G", "first / last row"),
];

/// How far the second line is indented under the name.
const INDENT: &str = "    ";

// The form field labels. Constants because the openers write them and
// [`submit`] reads them back; a typo in either would silently mean "blank".
const F_NAME: &str = "name";
const F_DESCRIPTION: &str = "description";
const F_CWD: &str = "cwd";
const F_WORKFLOW: &str = "workflow";
const F_GOAL: &str = "goal";
const F_PROMPT: &str = "master prompt";
const F_REPO: &str = "beads repo";
const F_BRAIN: &str = "brain";
const F_TAGS: &str = "tags";

/// An argument field's label. Prefixed rather than bare so a template with
/// `{{arg.action}}` cannot collide with [`super::form::ACTION`] and turn the
/// guard row into a text field.
fn arg_label(key: &str) -> String {
    format!("arg {key}")
}

/// Where a finished run wants the terminal to go: the master's tmux session
/// and pane, and what to call it in the status bar.
///
/// Carried rather than attached to on the spot because leaving TUI mode is the
/// event loop's business — the same rule that keeps `o` and `b` out of
/// `App::handle` (see [`super::land`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landing {
    pub tmux_session: String,
    pub pane: String,
    pub name: String,
    /// What the run wanted to say — a missing beads epic, an unused `--arg`.
    ///
    /// Carried with the landing rather than left on the `Ctx`, because the
    /// landing is the last thing that happens: `land` overwrites the status
    /// line, and on the exec shape it hands the terminal away and this
    /// process never draws again. [`super::land`] is what says them, once,
    /// on whichever of its two shapes it took.
    pub warnings: Vec<String>,
}

/// Per-tab state, owned by `App`.
#[derive(Debug, Default)]
pub struct State {
    /// Every template, in `q tpl list`'s order (by name).
    rows: Vec<Template>,
    /// How many Quests each row would unlink if it were deleted, index-aligned
    /// with `rows`. Loaded here rather than looked up when `d` is pressed, so
    /// the keymap stays pure — and so the confirm can say what it will cost
    /// before it happens rather than afterwards, the way `q tpl rm` does.
    linked: Vec<usize>,
    /// Whether [`refresh`] has ever run. A cold tab has nothing to be empty
    /// *against*, and "no templates yet" is a claim about the database that
    /// must not be made before anyone asked it.
    loaded: bool,
    /// Index into `rows`.
    selected: usize,
    /// The selected template's id, so a reload that renamed or added one keeps
    /// the selection on the same routine rather than on the same line.
    selected_id: Option<String>,
    /// First row drawn.
    offset: usize,
    /// The master a run just made, waiting for the loop to hand it the
    /// terminal.
    landing: Option<Landing>,
}

impl State {
    pub fn loaded(&self) -> bool {
        self.loaded
    }

    fn selected_row(&self) -> Option<&Template> {
        self.rows.get(self.selected)
    }

    /// Quests the selection would unlink. Zero when the counts have not caught
    /// up with the rows, which reads as "nothing to lose" — the safe way round
    /// for a number that only ever appears in a warning.
    fn selected_linked(&self) -> usize {
        self.linked.get(self.selected).copied().unwrap_or(0)
    }

    /// Keep the selection on the template it was on; clamp when that template
    /// is gone.
    fn resync(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
            self.selected_id = None;
            self.offset = 0;
            return;
        }
        if let Some(id) = self.selected_id.as_deref()
            && let Some(at) = self.rows.iter().position(|t| t.id == id)
        {
            self.selected = at;
        } else {
            self.selected = self.selected.min(self.rows.len() - 1);
        }
        self.selected_id = Some(self.rows[self.selected].id.clone());
        self.offset = self.offset.min(self.selected).min(self.rows.len() - 1);
    }

    /// Put the selection on a specific template — after adding or renaming
    /// one, where "the row that is there now" is not what the user means.
    ///
    /// Deliberately not [`State::resync`]: a template that was just created is
    /// not in `rows` yet, and `resync` answers "that id is not here" by
    /// overwriting `selected_id` with whatever sits at the clamped index.
    fn focus_on(&mut self, id: &str) {
        if let Some(at) = self.rows.iter().position(|t| t.id == id) {
            self.selected = at;
        }
        self.selected_id = Some(id.to_string());
    }

    fn move_by(&mut self, delta: isize, viewport: usize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.settle(viewport);
    }

    fn move_to(&mut self, at: usize, viewport: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = at.min(self.rows.len() - 1);
        self.settle(viewport);
    }

    /// Remember which template is selected, and scroll only as far as it takes
    /// to keep it on screen.
    fn settle(&mut self, viewport: usize) {
        self.selected_id = self.rows.get(self.selected).map(|t| t.id.clone());
        let viewport = viewport.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + viewport {
            self.offset = self.selected + 1 - viewport;
        }
        // Both branches only push `offset` forward, so a viewport that GREW
        // since the last frame would strand rows above the fold.
        self.offset = self.offset.min(self.rows.len().saturating_sub(viewport));
    }

    /// The master a run left behind, taken so it is landed in exactly once.
    pub(super) fn take_landing(&mut self) -> Option<Landing> {
        self.landing.take()
    }
}

// ------------------------------------------------------------------ loading

/// Reload this tab's data. Called by the event loop on tick and on `x`, never
/// from the state machine, so `App::handle` stays pure.
pub fn refresh(ctx: &Ctx, app: &mut App) -> anyhow::Result<()> {
    let rows = ctx.db()?.list_templates()?;
    app.templates.linked = tpl::linked_counts(ctx, &rows)?;
    app.templates.rows = rows;
    app.templates.loaded = true;
    app.templates.resync();
    settle_view(app);
    Ok(())
}

/// Scroll the selection back into view for the current terminal size — after a
/// reload that reordered the rows, and after a resize.
pub fn settle_view(app: &mut App) {
    let page = viewport(app);
    app.templates.settle(page);
}

/// How many rows the body can show.
fn viewport(app: &App) -> usize {
    let body = app.height.saturating_sub(2) as usize;
    (body / app.row_mode().lines() as usize).max(1)
}

/// The template a run or an export would act on, cloned so the borrow of `app`
/// ends with the lookup.
pub fn selected(app: &App) -> Option<Template> {
    app.templates.selected_row().cloned()
}

// ------------------------------------------------------------------- keymap

/// Keys the shell did not claim. Pure: anything needing the terminal or the
/// database leaves through an `Action`, never from in here.
pub fn handle(app: &mut App, input: Input) -> Action {
    let page = viewport(app);
    match input {
        Input::Up | Input::Char('k') => {
            app.templates.move_by(-1, page);
            Action::None
        }
        Input::Down | Input::Char('j') => {
            app.templates.move_by(1, page);
            Action::None
        }
        Input::PageUp => {
            app.templates.move_by(-(page as isize), page);
            Action::None
        }
        Input::PageDown => {
            app.templates.move_by(page as isize, page);
            Action::None
        }
        Input::Home | Input::Char('g') => {
            app.templates.move_to(0, page);
            Action::None
        }
        Input::End | Input::Char('G') => {
            app.templates.move_to(usize::MAX, page);
            Action::None
        }
        // SPEC §11: `⏎`/`r` run. Unlike the Quests tab, `Enter` here is not
        // the detail panel — this milestone's whole point is that a routine
        // runs in one keypress, and the row already carries everything a panel
        // would (the full definition is `X`).
        Input::Enter | Input::Char('r') => run_selection(app),
        Input::Char('a') => open_add(app),
        Input::Char('e') => open_edit(app),
        Input::Char('d') => open_delete(app),
        Input::Char('X') => export_selection(app),
        _ => Action::None,
    }
}

/// `⏎`/`r`. A template with no `{{arg.k}}` goes straight to the loop; one that
/// wants arguments gets the form that stands in for `--arg k=v`.
fn run_selection(app: &mut App) -> Action {
    let Some(template) = app.templates.selected_row() else {
        return Action::None;
    };
    let wanted = tpl::wanted_args(template);
    if wanted.is_empty() {
        return Action::Run;
    }
    let target = target_of(template);
    let mut form = Form::new(format!("run {}", target.name)).hint(
        "Tab field \u{b7} \u{2190}\u{2192} chooses \u{b7} \u{23ce} runs the action \u{b7} Esc cancels",
    );
    for key in &wanted {
        form = form.text(&arg_label(key), "", "(empty)");
    }
    form = form.note("the CLI spells these `q tpl run --arg k=v`");
    // Last, so typing still lands in the first argument. A run starts a tmux
    // session and a Claude inside it; that is not something a stray Enter gets
    // to do from a box that is holding the keyboard.
    app.open(Prompt::RunTemplate(target), form.action("run"));
    Action::None
}

/// What a prompt records about the template it was opened against — see
/// [`TemplateTarget`].
fn target_of(template: &Template) -> TemplateTarget {
    TemplateTarget {
        id: template.id.clone(),
        name: template.name.clone(),
        created_at: template.created_at,
    }
}

/// `a` — SPEC §11's `q tpl add`, as the form of SPEC §17.
fn open_add(app: &mut App) -> Action {
    let form = fields(Form::new("new template"), &Definition::default())
        .note("blank cwd means wherever the run is started from")
        .action("create");
    app.open(Prompt::AddTemplate, form);
    Action::None
}

/// `e` — the same form over an existing definition.
///
/// Deliberately the form and not `$EDITOR`: `q tpl edit` with no flags opens
/// the TOML in one, and a TUI that spawned an editor over its own alternate
/// screen would be handing the terminal away for a field the form already has.
/// `X` is there for reading the whole definition.
fn open_edit(app: &mut App) -> Action {
    let Some(template) = app.templates.selected_row() else {
        return Action::None;
    };
    let target = target_of(template);
    let form = fields(
        Form::new(format!("edit {}", target.name)),
        &Definition::of(template),
    )
    .note("run stats are history: editing never touches them")
    .action("save");
    app.open(Prompt::EditTemplate(target), form);
    Action::None
}

/// The nine definition fields, in the order `q tpl show` prints them.
fn fields(form: Form, d: &Definition) -> Form {
    form.hint(
        "Tab field \u{b7} \u{2190}\u{2192} chooses \u{b7} \u{23ce} runs the action \u{b7} Esc cancels",
    )
    .text(F_NAME, &d.name, "")
    .text(F_DESCRIPTION, &d.description, "(none)")
    .text(F_CWD, &d.cwd, "(the directory the run starts in)")
    // Free text, unlike the new-Quest form's select (`crate::tui::quests`): a
    // stored definition may name a workflow whose file is not here — it can
    // travel ahead of its files, and `q tpl edit` only re-checks the field when
    // it *changes* (`tpl::check_workflow`). A select cannot hold a value that
    // is not in its list, so opening this form over such a template would
    // silently rewrite its workflow on the way to editing something else.
    .text(F_WORKFLOW, &d.workflow, "(default)")
    .text(F_GOAL, &d.goal, "(none)")
    .text(F_PROMPT, &d.master_prompt, "(none)")
    .text(F_REPO, &d.beads_repo, "(none)")
    .toggle(F_BRAIN, d.create_brain)
    .text(F_TAGS, &d.tags.join(", "), "(none)")
}

/// The form read back as a definition. Tags are comma-separated because a form
/// has one line per field; [`crate::templates::clean_tags`] does the rest.
fn definition_of(form: &Form) -> Definition {
    Definition {
        name: form.trimmed(F_NAME).to_string(),
        description: form.trimmed(F_DESCRIPTION).to_string(),
        cwd: form.trimmed(F_CWD).to_string(),
        workflow: form.trimmed(F_WORKFLOW).to_string(),
        goal: form.trimmed(F_GOAL).to_string(),
        master_prompt: form.trimmed(F_PROMPT).to_string(),
        beads_repo: form.trimmed(F_REPO).to_string(),
        create_brain: form.is_on(F_BRAIN),
        tags: form
            .trimmed(F_TAGS)
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect(),
    }
}

/// `d` — the confirm, saying what a delete costs before it happens.
///
/// The count is read here rather than reported afterwards the way `q tpl rm`
/// reports it: a confirmation that does not say what it will do is not one.
fn open_delete(app: &mut App) -> Action {
    let Some(template) = app.templates.selected_row() else {
        return Action::None;
    };
    let target = target_of(template);
    let linked = app.templates.selected_linked();
    // The action row is first and starts on `cancel`, like the close prompt:
    // there is nothing to type here, so the affirmative is one arrow key away
    // and a keystroke that merely arrived is never it.
    let form = Form::new(format!("delete {}?", target.name))
        .hint("\u{2190}\u{2192} chooses \u{b7} \u{23ce} runs the action \u{b7} Esc cancels")
        .action("delete")
        .note(match linked {
            0 => "no quest was made from it".to_string(),
            1 => "1 quest keeps its history and loses the link".to_string(),
            n => format!("{n} quests keep their history and lose the link"),
        });
    app.open(Prompt::DeleteTemplate(target), form);
    Action::None
}

/// `X` — the loop pages it (SPEC §17's one pager mechanism).
fn export_selection(app: &mut App) -> Action {
    match app.templates.selected_row() {
        Some(_) => Action::Export,
        None => Action::None,
    }
}

// ------------------------------------------------------------- running them

/// `⏎`/`r` on a template with no arguments, as the loop runs it.
///
/// The selection is read here rather than recorded by `handle`, exactly as the
/// attach reads it: nothing happens between the keypress and this call, and a
/// target carried across would be one more thing that can go stale.
pub fn run_now(ctx: &Ctx, app: &mut App) {
    let Some(template) = selected(app) else {
        return;
    };
    if let Err(e) = instantiate(ctx, app, &template, &BTreeMap::new()) {
        app.say(format!("cannot run {}: {e:#}", template.name));
    }
}

/// The run itself, shared by the one-keypress path and the argument form.
///
/// `detach` is true: `new::create` makes the Quest and stops, and the landing
/// is left to [`super::land`], which leaves TUI mode the way every other
/// hand-over does and honours `[ui] return_after_detach`.
fn instantiate(
    ctx: &Ctx,
    app: &mut App,
    template: &Template,
    args: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    // `q tpl` refuses `--machine <other>` for every subcommand, and this is
    // the same instantiation: without the guard `q --machine ws`, tab `3`,
    // `⏎` makes the Quest *here* and stamps it `ws` — the local row
    // indistinguishable from a real remote one that both siblings were
    // hardened against (`tpl::refuse_remote`, and the new-Quest form's route
    // through `proxy::create_remote`).
    tpl::refuse_remote(ctx)?;
    let created = tpl::instantiate_with(ctx, template, args, true)?;
    app.templates.landing = Some(Landing {
        tmux_session: created.tmux_session.clone(),
        pane: created.session.tmux_pane.clone(),
        name: created.quest.slug.clone(),
        // Drained here for `tpl::instantiate`'s reason: the landing that comes
        // next hands the terminal over — outside tmux it `exec`s and this
        // process is gone — so a warning left in the buffer for a later
        // redraw would only ever be seen when the run did not attach.
        // A *failed* run leaves them buffered on purpose: `tui::submit` puts
        // them next to the error in the form, and `refresh_now` next to the
        // status message, and neither path attaches.
        warnings: ctx.take_warnings(),
    });
    app.say(format!(
        "{} from {} \u{b7} entering it",
        created.quest.slug, template.name
    ));
    Ok(())
}

/// Run the open form. Called by the event loop, never from `handle`: each of
/// these writes to the database, and one of them starts tmux.
pub fn submit(ctx: &Ctx, app: &mut App, prompt: &Prompt, form: &Form) -> anyhow::Result<()> {
    match prompt {
        Prompt::AddTemplate => add(ctx, app, form),
        Prompt::EditTemplate(target) => edit(ctx, app, target, form),
        Prompt::DeleteTemplate(target) => delete(ctx, app, target),
        Prompt::RunTemplate(target) => run_with_args(ctx, app, target, form),
        // A Quests or Sessions prompt never reaches here; `tui::submit`
        // dispatches on the variant first. Listed rather than `_` so a new
        // Templates prompt added without wiring fails to compile here instead
        // of silently doing nothing.
        Prompt::NewQuest
        | Prompt::Rename(_)
        | Prompt::Close(_)
        | Prompt::Resume(_)
        | Prompt::Send(_)
        | Prompt::Kill(_)
        | Prompt::Reset(_) => Ok(()),
    }
}

/// The template the box was opened against, re-read now — by id, not by the
/// selection: a tick can have reordered the listing while the prompt was up,
/// and `q tpl rm` in another terminal can have removed it outright.
///
/// The id alone is not identity: `new_id` is 16 bits and its retry only checks
/// live rows, so a delete and an add can hand a gone template's id to a new
/// one. `created_at` is the column nothing can change. The name is checked too
/// — the box put it in its title, and a rename underneath would make it name a
/// routine the user did not pick.
fn template_for(ctx: &Ctx, target: &TemplateTarget) -> anyhow::Result<Template> {
    let template = ctx
        .db()?
        .get_template(&target.id)?
        .filter(|t| t.created_at == target.created_at)
        .ok_or_else(|| {
            QError::NotFound(format!("template {} ({}) is gone", target.name, target.id))
        })?;
    if template.name != target.name {
        return Err(QError::Invalid(format!(
            "{} was renamed to {} while this box was up; Esc and try again",
            target.name, template.name
        ))
        .into());
    }
    Ok(template)
}

fn add(ctx: &Ctx, app: &mut App, form: &Form) -> anyhow::Result<()> {
    let stored = tpl::create(ctx, &definition_of(form))?;
    app.templates.focus_on(&stored.id);
    app.say(format!("created template {}", stored.name));
    Ok(())
}

fn edit(ctx: &Ctx, app: &mut App, target: &TemplateTarget, form: &Form) -> anyhow::Result<()> {
    let current = template_for(ctx, target)?;
    let stored = tpl::save(ctx, &current, &definition_of(form))?;
    app.templates.focus_on(&stored.id);
    app.say(format!("updated template {}", stored.name));
    Ok(())
}

fn delete(ctx: &Ctx, app: &mut App, target: &TemplateTarget) -> anyhow::Result<()> {
    let template = template_for(ctx, target)?;
    let unlinked = tpl::remove(ctx, &template)?;
    // The selection is not moved: the row drops out of the listing and
    // `resync` clamps the index, which lands on the row that took its place.
    app.say(format!(
        "removed template {}{}",
        template.name,
        unlinked_note(unlinked)
    ));
    Ok(())
}

/// `q tpl rm`'s wording, so the confirm, the status line and the CLI all
/// count the same thing the same way.
fn unlinked_note(unlinked: usize) -> String {
    match unlinked {
        0 => String::new(),
        1 => " \u{b7} 1 quest unlinked".to_string(),
        n => format!(" \u{b7} {n} quests unlinked"),
    }
}

fn run_with_args(
    ctx: &Ctx,
    app: &mut App,
    target: &TemplateTarget,
    form: &Form,
) -> anyhow::Result<()> {
    let template = template_for(ctx, target)?;
    let mut args: BTreeMap<String, String> = BTreeMap::new();
    for key in tpl::wanted_args(&template) {
        // `raw`, not `trimmed`: an argument is text a placeholder drops into,
        // and `--arg pad=" x "` keeps its spaces (`templates::parse_args`
        // stores the value untouched), so this must too or the same routine
        // expands two ways. Blank is allowed; `expand` is what decides whether
        // the result still makes sense.
        args.insert(key.clone(), form.raw(&arg_label(&key)).to_string());
    }
    instantiate(ctx, app, &template, &args)
}

// -------------------------------------------------------------------- render

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    // The size the shell has just published is the one this frame is drawn
    // for, so a resize scrolls the selection back into view before it is
    // missed rather than on the next keypress.
    settle_view(app);
    let app = &*app;
    let state = &app.templates;
    if state.rows.is_empty() {
        frame.render_widget(Paragraph::new(empty_lines(state)), inset(area));
        return;
    }

    let mode = app.row_mode();
    let width = area.width as usize;
    let capacity = (area.height as usize).max(1);
    let mut lines: Vec<Line> = Vec::new();
    for (n, template) in state.rows.iter().enumerate().skip(state.offset) {
        if lines.len() >= capacity {
            break;
        }
        lines.extend(row_lines(template, n == state.selected, mode, width));
    }
    lines.truncate(capacity);
    frame.render_widget(Paragraph::new(lines), area);
}

/// A fresh database has no templates, and a blank box says nothing about what
/// the tab is for.
fn empty_lines(state: &State) -> Vec<Line<'static>> {
    let why = if state.loaded {
        "no templates yet"
    } else {
        "loading templates…"
    };
    vec![
        Line::from(Span::raw(why).bold()),
        Line::from(""),
        Line::from(
            Span::raw("a routine you run again and again belongs here \u{b7} a adds one").dim(),
        ),
        Line::from(Span::raw("q tpl from <quest> <name> makes one out of a quest").dim()),
    ]
}

fn inset(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(1),
        height: area.height,
    }
}

/// SPEC §11's row — name, description, `run_count`, last run:
/// ```text
/// ▸ weekly-hygiene                              12 runs · last 3h
///      refresh the work repo · routine · ~/Code/work
/// ```
/// Three-line mode moves the run stats onto a line of their own, the way the
/// Quests tab moves its right-hand facts down (SPEC §17).
fn row_lines<'a>(t: &Template, selected: bool, mode: RowMode, width: usize) -> Vec<Line<'a>> {
    let head = format!("{} {}", if selected { "▸" } else { " " }, t.name);
    let style = if selected {
        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default()
    };

    let (moved, tail) = match mode {
        RowMode::Two => (String::new(), runs_cell(t)),
        RowMode::Three => (runs_cell(t), String::new()),
    };

    let mut out = vec![Line::from(Span::styled(pack(&head, &tail, width), style))];
    out.push(Line::from(Span::raw(layout::truncate(
        &format!("{INDENT}{}", description_cell(t)),
        width,
    ))));
    if !moved.is_empty() {
        out.push(Line::from(
            Span::raw(layout::truncate(&format!("{INDENT}{moved}"), width)).dim(),
        ));
    }
    out
}

/// `left` flush left and `right` flush right on one `width`-column line; the
/// left half gives way first, because the run stats are fixed-size and a name
/// is not.
fn pack(left: &str, right: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let rw = layout::width(right);
    if right.is_empty() || rw + 2 >= width {
        return layout::truncate(left, width);
    }
    let room = width - rw - 1;
    let left = layout::truncate(left, room);
    let pad = room - layout::width(&left) + 1;
    format!("{left}{}{right}", " ".repeat(pad))
}

/// SPEC §11's `run_count` and last run. A template nobody has run says so,
/// rather than showing `0 runs · -` twice over.
fn runs_cell(t: &Template) -> String {
    match t.last_run_at {
        Some(at) => format!(
            "{} {} \u{b7} last {}",
            t.run_count,
            if t.run_count == 1 { "run" } else { "runs" },
            fmt::age(at)
        ),
        None => "never run".to_string(),
    }
}

/// The second line: what the routine is, then the two facts that decide
/// whether it will do what is expected — its workflow and where it runs.
fn description_cell(t: &Template) -> String {
    let mut parts: Vec<String> = Vec::new();
    match t.description.as_deref().map(str::trim) {
        Some(d) if !d.is_empty() => parts.push(fmt::oneline(d, 200)),
        _ => parts.push("no description".to_string()),
    }
    if let Some(workflow) = t.workflow.as_deref().filter(|w| !w.is_empty()) {
        parts.push(workflow.to_string());
    }
    // Before the cwd, which is the longest and least surprising thing on the
    // line: `row_lines` truncates the join from the right, and a nested `cwd`
    // used to take the marker with it at every ordinary width. Said on the row
    // rather than discovered by pressing `⏎` — a routine that asks a question
    // first is a different thing from one that just goes — so it is the last
    // part that may be dropped, not the first.
    let wanted = tpl::wanted_args(t);
    if !wanted.is_empty() {
        parts.push(format!("args {}", wanted.join(", ")));
    }
    match t.cwd.as_deref() {
        Some(cwd) => parts.push(fmt::tilde(cwd)),
        None => parts.push("anywhere".to_string()),
    }
    parts.join(" \u{b7} ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::model::now;
    use crate::tui::form::Field;
    use crate::tui::render;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // ------------------------------------------------------------- fixtures

    /// A real `Ctx` over an in-memory database and a fixture tmux, plus a
    /// directory a template can point at. Nothing here touches the process
    /// environment: `Q_DB`, `Q_CONFIG` and `Q_FIXTURE` are all bypassed by
    /// `Ctx::for_tests`, and `bd` is `stub::NoBd` — reaching the real one by
    /// accident is a failure, not a subprocess.
    struct Rig {
        ctx: Ctx,
        tmux: tempfile::TempDir,
        cwd: tempfile::TempDir,
    }

    impl Rig {
        fn new() -> Rig {
            let tmux = tempfile::tempdir().unwrap();
            let path = tmux.path().join("tmux.json");
            std::fs::write(&path, "{}").unwrap();
            Rig {
                ctx: Ctx::for_tests(
                    Config::default(),
                    Db::open_in_memory().unwrap(),
                    Box::new(crate::tmux::FixtureTmux::new(path)),
                )
                .with_bd(Box::new(crate::beads::stub::NoBd)),
                tmux,
                cwd: tempfile::tempdir().unwrap(),
            }
        }

        fn dir(&self) -> String {
            self.cwd.path().to_string_lossy().to_string()
        }

        fn fixture(&self) -> crate::tmux::FixtureTmux {
            crate::tmux::FixtureTmux::new(self.tmux.path().join("tmux.json"))
        }

        /// A stored template, straight into the database.
        fn template(&self, name: &str, f: impl FnOnce(&mut Template)) -> Template {
            let mut row = Template::new(name);
            row.cwd = Some(self.dir());
            f(&mut row);
            self.ctx.db().unwrap().insert_template(&row).unwrap()
        }

        fn app(&self) -> App {
            let mut app = App::new(&self.ctx.config, "laptop");
            app.tab = crate::tui::app::Tab::Templates;
            app.set_size(120, 40);
            refresh(&self.ctx, &mut app).unwrap();
            app
        }

        fn names(&self) -> Vec<String> {
            self.ctx
                .db()
                .unwrap()
                .list_templates()
                .unwrap()
                .into_iter()
                .map(|t| t.name)
                .collect()
        }

        fn quests(&self) -> Vec<crate::model::Quest> {
            self.ctx.db().unwrap().list_quests(true).unwrap()
        }

        /// The same rig under `q --machine <other>` — the invocation
        /// `tpl::refuse_remote` exists for.
        fn pinned_to(self, machine: &str) -> Rig {
            let mut ctx = self.ctx.with_machine(Some(machine));
            ctx.config.machine.name = "laptop".to_string();
            Rig {
                ctx,
                tmux: self.tmux,
                cwd: self.cwd,
            }
        }
    }

    fn screen_at(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn screen(app: &mut App) -> String {
        screen_at(app, 120, 40)
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle(Input::Char(c));
        }
    }

    /// Tab to a field by label; panics rather than typing into the wrong one.
    fn focus(app: &mut App, label: &str) {
        for _ in 0..24 {
            let at = app
                .modal
                .as_ref()
                .expect("no form is open")
                .form
                .focused()
                .map(Field::label);
            if at == Some(label) {
                return;
            }
            app.handle(Input::Tab);
        }
        panic!("no field labelled {label}");
    }

    fn set(app: &mut App, label: &str, value: &str) {
        focus(app, label);
        app.handle(Input::Ctrl('u'));
        type_text(app, value);
    }

    /// Move the action row off `cancel`, the way the user has to.
    fn choose_action(app: &mut App) {
        focus(app, crate::tui::form::ACTION);
        for _ in 0..3 {
            if app.modal.as_ref().unwrap().form.confirmed() {
                return;
            }
            app.handle(Input::Right);
        }
        panic!("the action row never left `{}`", crate::tui::form::CANCEL);
    }

    /// Exactly what the event loop does with `Action::Submit`.
    fn submit_form(rig: &Rig, app: &mut App) {
        choose_action(app);
        assert_eq!(app.handle(Input::Enter), Action::Submit);
        crate::tui::submit(&rig.ctx, app);
    }

    /// One key, treated as the event loop treats it: a run happens only
    /// because the state machine asked for it, and the landing is drained the
    /// way `land` drains it (without a terminal to hand over).
    fn press(rig: &Rig, app: &mut App, input: Input) -> Action {
        let action = app.handle(input);
        match action {
            Action::Run => run_now(&rig.ctx, app),
            Action::Submit => crate::tui::submit(&rig.ctx, app),
            _ => {}
        }
        action
    }

    // ---------------------------------------------------------------- list

    #[test]
    fn an_empty_database_says_what_the_tab_is_for() {
        let rig = Rig::new();
        let mut app = rig.app();
        let text = screen(&mut app);
        assert!(text.contains("no templates yet"), "{text}");
        assert!(text.contains("a adds one"), "{text}");
    }

    /// A cold tab must not claim the database is empty before it has asked it.
    #[test]
    fn a_tab_that_has_never_loaded_says_so_and_asks_for_a_reload() {
        let mut app = App::new(&Config::default(), "laptop");
        app.set_size(120, 40);
        assert!(!app.templates.loaded());
        // Switching to it asks for the rows, as Sessions and Events do.
        assert_eq!(app.handle(Input::Char('3')), Action::Refresh);
        let text = screen(&mut app);
        assert!(text.contains("loading templates"), "{text}");
        assert!(!text.contains("no templates yet"), "{text}");
    }

    #[test]
    fn the_list_shows_name_description_runs_and_the_last_run() {
        let rig = Rig::new();
        rig.template("weekly-hygiene", |t| {
            t.description = Some("refresh the work repo".to_string());
            t.workflow = Some("routine".to_string());
            t.run_count = 12;
            t.last_run_at = Some(now() - 3 * 3600);
        });
        let mut app = rig.app();
        let text = screen(&mut app);
        assert!(text.contains("weekly-hygiene"), "{text}");
        assert!(text.contains("refresh the work repo"), "{text}");
        assert!(text.contains("routine"), "{text}");
        assert!(text.contains("12 runs"), "{text}");
        assert!(text.contains("last 3h"), "{text}");
    }

    #[test]
    fn a_template_nobody_has_run_says_so_rather_than_showing_a_dash() {
        let rig = Rig::new();
        rig.template("deps-audit", |_| {});
        let mut app = rig.app();
        let text = screen(&mut app);
        assert!(text.contains("never run"), "{text}");
        assert!(text.contains("no description"), "{text}");
    }

    /// SPEC §17's narrow band: the run stats move onto a line of their own
    /// rather than being cut off the right of the first.
    #[test]
    fn a_narrow_terminal_moves_the_run_stats_onto_their_own_line() {
        let rig = Rig::new();
        rig.template("weekly-hygiene", |t| {
            t.description = Some("refresh the work repo".to_string());
            t.run_count = 4;
            t.last_run_at = Some(now() - 60);
        });
        let mut app = rig.app();
        let wide = screen_at(&mut app, 120, 20);
        let narrow = screen_at(&mut app, 60, 20);
        for text in [&wide, &narrow] {
            assert!(text.contains("4 runs"), "{text}");
        }
        // Wide: name and stats share a line. Narrow: they do not.
        assert!(
            wide.lines()
                .any(|l| l.contains("weekly-hygiene") && l.contains("4 runs")),
            "{wide}"
        );
        assert!(
            !narrow
                .lines()
                .any(|l| l.contains("weekly-hygiene") && l.contains("4 runs")),
            "{narrow}"
        );
    }

    #[test]
    fn the_listing_is_in_q_tpl_list_order_and_the_selection_moves() {
        let rig = Rig::new();
        for name in ["weekly-hygiene", "deps-audit", "pr-review-queue"] {
            rig.template(name, |_| {});
        }
        let mut app = rig.app();
        assert_eq!(
            rig.names(),
            ["deps-audit", "pr-review-queue", "weekly-hygiene"]
        );
        assert_eq!(selected(&app).unwrap().name, "deps-audit");
        app.handle(Input::Down);
        assert_eq!(selected(&app).unwrap().name, "pr-review-queue");
        app.handle(Input::Char('j'));
        assert_eq!(selected(&app).unwrap().name, "weekly-hygiene");
        // Clamped at the ends rather than wrapping.
        app.handle(Input::Char('j'));
        assert_eq!(selected(&app).unwrap().name, "weekly-hygiene");
        app.handle(Input::Char('g'));
        assert_eq!(selected(&app).unwrap().name, "deps-audit");
        app.handle(Input::Char('G'));
        assert_eq!(selected(&app).unwrap().name, "weekly-hygiene");
        app.handle(Input::Char('k'));
        assert_eq!(selected(&app).unwrap().name, "pr-review-queue");
    }

    /// A reload that added a row above the selection keeps the selection on
    /// the routine, not on the line.
    #[test]
    fn a_reload_keeps_the_selection_on_the_same_template() {
        let rig = Rig::new();
        rig.template("weekly-hygiene", |_| {});
        let mut app = rig.app();
        assert_eq!(selected(&app).unwrap().name, "weekly-hygiene");
        rig.template("deps-audit", |_| {});
        refresh(&rig.ctx, &mut app).unwrap();
        assert_eq!(selected(&app).unwrap().name, "weekly-hygiene");
    }

    /// More rows than the body can hold: the offset follows the selection to
    /// the end of the list and all the way back, and the selected row is on
    /// screen at both ends.
    #[test]
    fn a_list_taller_than_the_viewport_scrolls_with_the_selection() {
        let rig = Rig::new();
        for n in 0..12 {
            rig.template(&format!("t{n:02}"), |_| {});
        }
        let mut app = rig.app();
        // 12 rows tall: two for the tab bar and footer, five two-line rows.
        app.set_size(120, 12);
        settle_view(&mut app);
        let top = screen_at(&mut app, 120, 12);
        assert_eq!(app.templates.offset, 0);
        assert!(top.contains("t00"), "{top}");
        assert!(!top.contains("t11"), "{top}");

        app.handle(Input::Char('G'));
        let bottom = screen_at(&mut app, 120, 12);
        assert_eq!(selected(&app).unwrap().name, "t11");
        assert_eq!(app.templates.offset, 7, "{bottom}");
        assert!(bottom.contains("t11"), "{bottom}");
        assert!(!bottom.contains("t00"), "{bottom}");

        app.handle(Input::Char('g'));
        let back = screen_at(&mut app, 120, 12);
        assert_eq!(selected(&app).unwrap().name, "t00");
        assert_eq!(app.templates.offset, 0);
        assert!(back.contains("t00"), "{back}");
        assert!(!back.contains("t11"), "{back}");
    }

    // ----------------------------------------------------------------- run

    #[test]
    fn enter_runs_a_template_in_one_keypress_and_counts_the_run() {
        let rig = Rig::new();
        rig.template("weekly-hygiene", |t| {
            t.goal = Some("tidy the work repo".to_string());
        });
        let mut app = rig.app();

        assert_eq!(press(&rig, &mut app, Input::Enter), Action::Run);
        assert!(app.modal.is_none(), "a form went up: {}", screen(&mut app));

        let quests = rig.quests();
        assert_eq!(quests.len(), 1);
        assert_eq!(quests[0].goal.as_deref(), Some("tidy the work repo"));
        // The Quest records which definition made it (SPEC §11).
        let stored = rig.ctx.db().unwrap().list_templates().unwrap();
        assert_eq!(
            quests[0].template_id.as_deref(),
            Some(stored[0].id.as_str())
        );
        // The master is up in its own tmux session, the same as `q tpl run`.
        assert!(
            rig.fixture()
                .load()
                .unwrap()
                .panes
                .iter()
                .any(|p| p.session_name.starts_with("q-"))
        );
        // And the run is counted where the list will show it.
        assert_eq!(stored[0].run_count, 1);
        assert!(stored[0].last_run_at.is_some());

        // The loop is told where to land, once.
        let landing = app.templates.take_landing().expect("nowhere to land");
        assert_eq!(landing.name, quests[0].slug);
        assert!(app.templates.take_landing().is_none());
    }

    /// The stats the row shows come back changed after a run — the whole
    /// reason the loop reloads the tab afterwards.
    #[test]
    fn the_run_stats_on_the_row_move_after_a_run() {
        let rig = Rig::new();
        rig.template("deps-audit", |_| {});
        let mut app = rig.app();
        assert!(
            screen(&mut app).contains("never run"),
            "{}",
            screen(&mut app)
        );

        press(&rig, &mut app, Input::Char('r'));
        refresh(&rig.ctx, &mut app).unwrap();
        let text = screen(&mut app);
        assert!(text.contains("1 run "), "{text}");
        assert!(!text.contains("never run"), "{text}");
    }

    #[test]
    fn a_template_with_args_asks_for_them_and_then_goes() {
        let rig = Rig::new();
        rig.template("pr-review", |t| {
            t.goal = Some("review PR {{arg.pr}} on {{date}}".to_string());
        });
        let mut app = rig.app();

        assert_eq!(press(&rig, &mut app, Input::Enter), Action::None);
        let modal = app.modal.as_ref().expect("no form went up");
        assert!(matches!(modal.prompt, Prompt::RunTemplate(_)));
        assert!(app.templates.landing.is_none(), "it ran without the arg");

        set(&mut app, &arg_label("pr"), "4821");
        submit_form(&rig, &mut app);
        assert!(app.modal.is_none(), "form still up: {}", screen(&mut app));

        let quests = rig.quests();
        assert_eq!(quests.len(), 1);
        let goal = quests[0].goal.clone().unwrap();
        assert!(goal.starts_with("review PR 4821 on 2"), "{goal}");
        assert!(app.templates.take_landing().is_some());
    }

    /// The row says an argument is wanted before the user finds out by
    /// pressing Enter.
    #[test]
    fn the_row_names_the_arguments_a_routine_wants() {
        let rig = Rig::new();
        rig.template("pr-review", |t| {
            t.goal = Some("review PR {{arg.pr}}".to_string());
        });
        let mut app = rig.app();
        assert!(screen(&mut app).contains("args pr"), "{}", screen(&mut app));
    }

    /// And keeps saying it once the `cwd` is long: the marker is the least
    /// droppable fact on the line, so the path is what the truncation eats.
    #[test]
    fn the_args_marker_outlives_a_long_cwd_at_every_width() {
        let rig = Rig::new();
        rig.template("pr-review", |t| {
            t.description = Some("review the oldest PR in the queue".to_string());
            t.goal = Some("review PR {{arg.pr}}".to_string());
            t.cwd = Some("/a/very/deeply/nested/directory/for/this/routine".to_string());
        });
        let mut app = rig.app();
        for width in [200, 160, 120, 100, 80, 70] {
            app.set_size(width, 40);
            let text = screen_at(&mut app, width, 40);
            assert!(text.contains("args pr"), "{width} cols: {text}");
        }
        // The other half of the same rule: at 80 the path is the thing that
        // did not fit.
        app.set_size(80, 40);
        let narrow = screen_at(&mut app, 80, 40);
        assert!(!narrow.contains("this/routine"), "{narrow}");
    }

    /// `q tpl run t --arg pad=" x "` keeps its spaces (`templates::parse_args`
    /// stores the value untouched), so the form standing in for `--arg` keeps
    /// them too — one routine, one expansion.
    #[test]
    fn an_argument_reaches_the_run_exactly_as_it_was_typed() {
        let rig = Rig::new();
        rig.template("pad", |t| {
            t.goal = Some("[{{arg.pad}}]".to_string());
        });
        let mut app = rig.app();
        press(&rig, &mut app, Input::Enter);
        set(&mut app, &arg_label("pad"), " x ");
        submit_form(&rig, &mut app);
        assert!(app.modal.is_none(), "{}", screen(&mut app));
        let quests = rig.quests();
        assert_eq!(quests.len(), 1);
        assert_eq!(quests[0].goal.as_deref(), Some("[ x ]"));
    }

    /// `q --machine ws`, tab `3`, `⏎`: the Quest would be made *here* and
    /// stamped `ws`. `q tpl` refuses that shape and so does this — on both
    /// ways in, since the argument form reaches the same instantiation.
    #[test]
    fn a_run_pinned_to_another_machine_is_refused_as_q_tpl_refuses_it() {
        let rig = Rig::new().pinned_to("ws");
        rig.template("weekly-hygiene", |_| {});
        rig.template("pr-review", |t| {
            t.goal = Some("review PR {{arg.pr}}".to_string());
        });
        let mut app = rig.app();

        // The bare keypress.
        app.handle(Input::Char('G'));
        assert_eq!(selected(&app).unwrap().name, "weekly-hygiene");
        assert_eq!(press(&rig, &mut app, Input::Enter), Action::Run);
        assert!(app.status.contains("--machine ws"), "{}", app.status);
        assert!(app.status.contains("q tpl export"), "{}", app.status);
        assert!(rig.quests().is_empty(), "a local quest was minted anyway");
        assert!(app.templates.landing.is_none(), "it landed somewhere");

        // And the argument form, which refuses on submit and stays up.
        app.handle(Input::Char('g'));
        assert_eq!(press(&rig, &mut app, Input::Enter), Action::None);
        set(&mut app, &arg_label("pr"), "4821");
        submit_form(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .expect("the form went away")
            .form
            .error()
            .unwrap_or_default()
            .to_string();
        assert!(error.contains("--machine ws"), "{error}");
        assert!(rig.quests().is_empty(), "a local quest was minted anyway");
        assert!(app.templates.landing.is_none(), "it landed somewhere");
    }

    /// A `cwd` that has gone is a status message, not a half-made Quest.
    #[test]
    fn a_run_whose_directory_is_gone_is_reported_and_makes_nothing() {
        let rig = Rig::new();
        rig.template("weekly-hygiene", |t| {
            t.cwd = Some("/no/such/directory/here".to_string());
        });
        let mut app = rig.app();
        press(&rig, &mut app, Input::Enter);
        assert!(
            app.status.contains("cannot run weekly-hygiene"),
            "{}",
            app.status
        );
        assert!(rig.quests().is_empty());
        assert!(app.templates.take_landing().is_none());
    }

    // ------------------------------------------------------------ add/edit

    #[test]
    fn the_add_form_stores_a_template_and_selects_it() {
        let rig = Rig::new();
        let mut app = rig.app();
        app.handle(Input::Char('a'));
        set(&mut app, F_NAME, "weekly-hygiene");
        set(&mut app, F_DESCRIPTION, "refresh the work repo");
        set(&mut app, F_CWD, &rig.dir());
        set(&mut app, F_WORKFLOW, "routine");
        set(&mut app, F_GOAL, "tidy up");
        set(&mut app, F_PROMPT, "start with the beads queue");
        set(&mut app, F_TAGS, "work, weekly, work");
        submit_form(&rig, &mut app);

        assert!(app.modal.is_none(), "{}", screen(&mut app));
        let stored = rig.ctx.db().unwrap().list_templates().unwrap();
        assert_eq!(stored.len(), 1);
        let t = &stored[0];
        assert_eq!(t.name, "weekly-hygiene");
        assert_eq!(t.description.as_deref(), Some("refresh the work repo"));
        assert_eq!(t.workflow.as_deref(), Some("routine"));
        assert_eq!(t.goal.as_deref(), Some("tidy up"));
        assert_eq!(
            t.master_prompt.as_deref(),
            Some("start with the beads queue")
        );
        // Duplicates dropped, order kept — `clean_tags`, not a second copy.
        assert_eq!(
            t.tags.as_deref(),
            Some(["work".to_string(), "weekly".to_string()].as_slice())
        );
        // The cwd is pinned as `q tpl add --cwd` pins it: canonical, so the
        // routine runs where it was typed rather than following a symlink.
        let pinned = std::fs::canonicalize(rig.dir()).unwrap();
        assert_eq!(t.cwd.as_deref(), Some(pinned.to_string_lossy().as_ref()));

        refresh(&rig.ctx, &mut app).unwrap();
        assert_eq!(selected(&app).unwrap().name, "weekly-hygiene");
    }

    /// The CLI's validators, reached through the CLI's own entry points — so
    /// the message is the CLI's message and the form stays up over it.
    #[test]
    fn the_form_refuses_what_the_cli_refuses_and_keeps_what_was_typed() {
        let rig = Rig::new();
        let mut app = rig.app();

        for (name, wanted) in [
            ("Weekly Hygiene", "invalid template name"),
            ("", "invalid template name"),
        ] {
            app.handle(Input::Char('a'));
            set(&mut app, F_NAME, name);
            submit_form(&rig, &mut app);
            let modal = app.modal.as_ref().expect("the form went away");
            assert!(
                modal.form.error().unwrap_or_default().contains(wanted),
                "{name:?}: {:?}",
                modal.form.error()
            );
            assert_eq!(modal.form.trimmed(F_NAME), name.trim());
            app.handle(Input::Esc);
        }
        assert!(rig.names().is_empty());

        // A placeholder nothing can ever fill is refused at store time, as
        // `q tpl add` refuses it.
        app.handle(Input::Char('a'));
        set(&mut app, F_NAME, "typo");
        set(&mut app, F_GOAL, "as of {{today}}");
        submit_form(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .unwrap()
            .form
            .error()
            .unwrap()
            .to_string();
        assert!(error.contains("unknown placeholder"), "{error}");
        app.handle(Input::Esc);

        // A workflow that is not in the registry, likewise (SPEC §11). The
        // field is free text here on purpose — see `fields` — so the refusal
        // is what stops a definition naming a workflow nothing can read.
        app.handle(Input::Char('a'));
        set(&mut app, F_NAME, "typo");
        set(&mut app, F_WORKFLOW, "orchestartor");
        submit_form(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .unwrap()
            .form
            .error()
            .unwrap()
            .to_string();
        assert!(error.contains("unknown workflow `orchestartor`"), "{error}");
        assert!(
            error.contains("orchestrator"),
            "the list is offered: {error}"
        );
        app.handle(Input::Esc);

        // A built-in goes through.
        app.handle(Input::Char('a'));
        set(&mut app, F_NAME, "fine");
        set(&mut app, F_WORKFLOW, "routine");
        submit_form(&rig, &mut app);
        assert!(app.modal.is_none(), "{:?}", app.status);
        app.handle(Input::Esc);

        // And a name that is taken.
        rig.template("deps-audit", |_| {});
        refresh(&rig.ctx, &mut app).unwrap();
        app.handle(Input::Char('a'));
        set(&mut app, F_NAME, "deps-audit");
        submit_form(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .unwrap()
            .form
            .error()
            .unwrap()
            .to_string();
        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn the_edit_form_opens_on_the_stored_definition_and_keeps_the_run_stats() {
        let rig = Rig::new();
        rig.template("weekly-hygiene", |t| {
            t.description = Some("refresh the work repo".to_string());
            t.tags = Some(vec!["work".to_string(), "weekly".to_string()]);
            t.run_count = 7;
            t.last_run_at = Some(now() - 600);
        });
        let mut app = rig.app();
        app.handle(Input::Char('e'));
        let form = &app.modal.as_ref().expect("no form").form;
        assert_eq!(form.trimmed(F_NAME), "weekly-hygiene");
        assert_eq!(form.trimmed(F_DESCRIPTION), "refresh the work repo");
        assert_eq!(form.trimmed(F_TAGS), "work, weekly");

        set(&mut app, F_NAME, "weekly-tidy");
        set(&mut app, F_DESCRIPTION, "tidier");
        submit_form(&rig, &mut app);
        assert!(app.modal.is_none(), "{}", screen(&mut app));

        let stored = rig.ctx.db().unwrap().list_templates().unwrap();
        assert_eq!(
            stored.len(),
            1,
            "the edit added a row instead of writing one"
        );
        assert_eq!(stored[0].name, "weekly-tidy");
        assert_eq!(stored[0].description.as_deref(), Some("tidier"));
        // History survives a definition edit (SPEC §11).
        assert_eq!(stored[0].run_count, 7);
        assert!(stored[0].last_run_at.is_some());
    }

    /// The box named a routine; a rename underneath makes it a lie.
    #[test]
    fn a_prompt_refuses_a_template_that_changed_under_it() {
        let rig = Rig::new();
        let stored = rig.template("weekly-hygiene", |_| {});
        let mut app = rig.app();
        app.handle(Input::Char('e'));

        let mut renamed = stored.clone();
        renamed.name = "weekly-tidy".to_string();
        rig.ctx
            .db()
            .unwrap()
            .update_template(&stored.id, &renamed)
            .unwrap();

        set(&mut app, F_DESCRIPTION, "whatever");
        submit_form(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .expect("form went away")
            .form
            .error()
            .unwrap();
        assert!(error.contains("was renamed to weekly-tidy"), "{error}");

        // …and one that is gone outright.
        app.handle(Input::Esc);
        rig.ctx.db().unwrap().delete_template(&stored.id).unwrap();
        refresh(&rig.ctx, &mut app).unwrap();
        assert!(app.templates.rows.is_empty());
        // Nothing selected means nothing to edit; no form goes up at all.
        assert_eq!(app.handle(Input::Char('e')), Action::None);
        assert!(app.modal.is_none());
    }

    /// The same identity check on the other two prompts that carry a target:
    /// a delete whose row was renamed underneath, and a run whose definition
    /// was removed while the argument form was up.
    #[test]
    fn the_delete_and_run_prompts_refuse_a_template_that_changed_under_them() {
        let rig = Rig::new();
        let stored = rig.template("weekly-hygiene", |_| {});
        let mut app = rig.app();
        app.handle(Input::Char('d'));
        let mut renamed = stored.clone();
        renamed.name = "weekly-tidy".to_string();
        rig.ctx
            .db()
            .unwrap()
            .update_template(&stored.id, &renamed)
            .unwrap();
        submit_form(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .expect("the box went away")
            .form
            .error()
            .unwrap_or_default()
            .to_string();
        assert!(error.contains("was renamed to weekly-tidy"), "{error}");
        assert_eq!(rig.names(), ["weekly-tidy"], "it was deleted anyway");

        let rig = Rig::new();
        let stored = rig.template("pr-review", |t| {
            t.goal = Some("review PR {{arg.pr}}".to_string());
        });
        let mut app = rig.app();
        assert_eq!(press(&rig, &mut app, Input::Enter), Action::None);
        rig.ctx.db().unwrap().delete_template(&stored.id).unwrap();
        set(&mut app, &arg_label("pr"), "4821");
        submit_form(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .expect("the box went away")
            .form
            .error()
            .unwrap_or_default()
            .to_string();
        assert!(error.contains("is gone"), "{error}");
        assert!(rig.quests().is_empty(), "it ran a template that is gone");
        assert!(app.templates.landing.is_none());
    }

    // -------------------------------------------------------------- delete

    #[test]
    fn the_delete_confirm_says_what_it_costs_and_both_answers_work() {
        let rig = Rig::new();
        let stored = rig.template("weekly-hygiene", |_| {});
        let mut app = rig.app();
        // A Quest made from it, so the confirm has something to unlink.
        press(&rig, &mut app, Input::Enter);
        refresh(&rig.ctx, &mut app).unwrap();
        assert_eq!(rig.quests().len(), 1);

        // Cancel: Esc leaves everything alone.
        app.handle(Input::Char('d'));
        let text = screen(&mut app);
        assert!(text.contains("delete weekly-hygiene?"), "{text}");
        assert!(
            text.contains("1 quest keeps its history and loses the link"),
            "{text}"
        );
        app.handle(Input::Esc);
        assert!(app.modal.is_none());
        assert_eq!(rig.names(), ["weekly-hygiene"]);

        // A bare Enter is not the affirmative either (the action row starts
        // on `cancel`), which is the guard `Form::action` exists for: the box
        // stays up saying what is missing rather than deleting anything.
        app.handle(Input::Char('d'));
        assert_eq!(app.handle(Input::Enter), Action::None);
        let error = app.modal.as_ref().expect("the box went away").form.error();
        assert!(
            error.unwrap_or_default().contains("nothing done"),
            "{error:?}"
        );
        assert_eq!(rig.names(), ["weekly-hygiene"], "a bare Enter deleted it");
        app.handle(Input::Esc);

        // Confirm: gone, and the Quest survives with its link cleared.
        app.handle(Input::Char('d'));
        submit_form(&rig, &mut app);
        assert!(app.modal.is_none(), "{}", screen(&mut app));
        assert!(rig.names().is_empty());
        assert!(app.status.contains("1 quest unlinked"), "{}", app.status);
        let quests = rig.quests();
        assert_eq!(quests.len(), 1, "the delete took the quest with it");
        assert_eq!(quests[0].template_id, None);
        let _ = stored;
    }

    /// Deleting the row at the bottom of the list leaves the selection on the
    /// one before it, not on nothing.
    #[test]
    fn deleting_the_last_row_selects_the_one_above_it() {
        let rig = Rig::new();
        for name in ["alpha", "beta", "gamma"] {
            rig.template(name, |_| {});
        }
        let mut app = rig.app();
        app.handle(Input::Char('G'));
        assert_eq!(selected(&app).unwrap().name, "gamma");

        app.handle(Input::Char('d'));
        submit_form(&rig, &mut app);
        refresh(&rig.ctx, &mut app).unwrap();
        assert_eq!(rig.names(), ["alpha", "beta"]);
        assert_eq!(selected(&app).unwrap().name, "beta");

        // …and down to nothing, where there is no selection to keep.
        app.handle(Input::Char('d'));
        submit_form(&rig, &mut app);
        refresh(&rig.ctx, &mut app).unwrap();
        assert_eq!(selected(&app).unwrap().name, "alpha");
        app.handle(Input::Char('d'));
        submit_form(&rig, &mut app);
        refresh(&rig.ctx, &mut app).unwrap();
        assert!(rig.names().is_empty());
        assert!(selected(&app).is_none());
        assert_eq!(app.templates.offset, 0);
    }

    #[test]
    fn deleting_a_template_nothing_was_made_from_says_nothing_about_quests() {
        let rig = Rig::new();
        rig.template("deps-audit", |_| {});
        let mut app = rig.app();
        app.handle(Input::Char('d'));
        submit_form(&rig, &mut app);
        assert!(rig.names().is_empty());
        assert!(!app.status.contains("unlinked"), "{}", app.status);
    }

    // -------------------------------------------------------------- export

    #[test]
    fn export_asks_the_loop_for_a_pager_and_renders_the_stored_toml() {
        let rig = Rig::new();
        rig.template("weekly-hygiene", |t| {
            t.description = Some("refresh the work repo".to_string());
            t.goal = Some("tidy up".to_string());
        });
        let mut app = rig.app();
        assert_eq!(app.handle(Input::Char('X')), Action::Export);

        // What the loop is about to page: `q tpl export`'s document, for the
        // selection alone.
        let selection = selected(&app).unwrap();
        let toml = crate::templates::render(std::slice::from_ref(&selection)).unwrap();
        assert!(toml.contains("[[template]]"), "{toml}");
        assert!(toml.contains("weekly-hygiene"), "{toml}");
        assert!(toml.contains("tidy up"), "{toml}");
        // Run stats are history and never travel (SPEC §11).
        assert!(!toml.contains("run_count"), "{toml}");
    }

    /// Every key that acts on a selection is a no-op when there is none, and
    /// none of them may take the terminal away.
    #[test]
    fn the_action_keys_do_nothing_on_an_empty_listing() {
        let rig = Rig::new();
        let mut app = rig.app();
        for key in ['r', 'e', 'd', 'X'] {
            assert_eq!(
                press(&rig, &mut app, Input::Char(key)),
                Action::None,
                "{key}"
            );
            assert!(app.modal.is_none(), "{key} opened a form");
        }
        assert_eq!(app.handle(Input::Enter), Action::None);
        // `a` is the exception: adding needs no selection.
        assert_eq!(app.handle(Input::Char('a')), Action::None);
        assert!(app.modal.is_some());
    }

    /// `x` is the shell's reload on every tab, and the export lives on `X`.
    /// The two are one keystroke apart, so this pins which is which.
    #[test]
    fn x_is_still_refresh_here_and_the_export_is_on_shift_x() {
        let rig = Rig::new();
        rig.template("deps-audit", |_| {});
        let mut app = rig.app();
        assert_eq!(app.handle(Input::Char('x')), Action::Refresh);
        assert_eq!(app.handle(Input::Char('X')), Action::Export);
        // And the overlay says so, rather than leaving it to be discovered.
        let rows = crate::tui::app::help_rows(crate::tui::app::Tab::Templates);
        assert!(
            rows.iter().any(|(k, d)| *k == "X" && d.contains("export")),
            "{rows:?}"
        );
    }
}
