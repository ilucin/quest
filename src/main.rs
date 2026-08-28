mod beads;
mod brief;
mod cli;
mod commands;
mod config;
mod db;
mod doctor;
mod error;
mod hooks;
mod model;
mod naming;
mod output;
mod proc;
mod registry;
mod remote;
mod tmux;
mod tui;

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use clap::Parser;

use cli::{ArtifactAction, Cli, Command, ConfigAction, HookAction, LinkAction};
use config::Config;
use db::Db;
use error::QError;
use remote::Ssh;
use tmux::Tmux;

/// Everything a command needs beyond its own arguments. Built once by the
/// dispatcher and handed to each command.
pub struct Ctx {
    pub json: bool,
    pub quiet: bool,
    pub config: Config,
    machine_override: Option<String>,
    /// SPEC §15's recursion guard: set on the `q list` we run over ssh, so the
    /// far end answers out of its own database instead of fanning out again.
    no_remote: bool,
    /// A human already answered this command's confirmation, on the machine
    /// that had the terminal — see [`cli::Cli::confirmed`].
    confirmed: bool,
    /// The Quest identity this command was pinned to before it crossed the
    /// wire — see [`cli::Cli::expect`] and [`commands::proxy::verify`].
    expect: Option<String>,
    /// Absent only for `q config`, which has to work before — and in order to
    /// fix — a broken environment.
    db: Option<Db>,
    tmux: Box<dyn Tmux>,
    /// ssh, like tmux, behind a trait so no test reaches a real host. `Arc`
    /// rather than `Box` because the TUI's remote poller runs the same client
    /// from its own thread (`Ssh` is `Send + Sync` for exactly that).
    ssh: Arc<dyn Ssh>,
    /// `bd`, like `tmux`, behind a trait and owned rather than discovered, so
    /// a test drives the beads paths without the process environment and the
    /// TUI can hand `bd` a client that does not chatter at the screen.
    bd: Box<dyn beads::Bd>,
    /// Diagnostics for the human, buffered instead of written.
    ///
    /// A library call reached from `q new` may print to stderr; the same call
    /// reached from the TUI is running in raw mode on the alternate screen,
    /// where any write tears the frame up and ratatui's diff renderer never
    /// repaints it. So nothing below this line writes: warnings land here and
    /// the caller decides — `run` prints them, the TUI puts them in the status
    /// bar or in the form's error line.
    warnings: Mutex<Vec<String>>,
}

impl Ctx {
    /// The machine to filter listings by, i.e. `--machine` only when it was
    /// actually given.
    pub fn machine_filter(&self) -> Option<&str> {
        self.machine_override.as_deref()
    }

    /// The targeted machine name for this invocation: `--machine` if given
    /// (validated, never written to the config), else the configured one.
    pub fn machine(&self) -> &str {
        self.machine_override
            .as_deref()
            .unwrap_or(&self.config.machine.name)
    }

    /// False under `--no-remote`: no command may touch a remote machine.
    pub fn remote_enabled(&self) -> bool {
        !self.no_remote
    }

    /// True under `--confirmed`: skip the `[y/N]`, and only that. Nothing in
    /// `q` may read this as `-f`.
    pub fn confirmed(&self) -> bool {
        self.confirmed
    }

    /// `--expect`'s value: the Quest identity the sending machine resolved and
    /// the human confirmed. `None` for a command nobody proxied.
    pub fn expected(&self) -> Option<&str> {
        self.expect.as_deref()
    }

    fn new(args: &Cli, config: Config, db: Option<Db>) -> anyhow::Result<Ctx> {
        let machine_override = match &args.machine {
            Some(m) => {
                remote::validate_target(&config, m)?;
                // `--machine <remote>` says "that machine only"; `--no-remote`
                // says "no machine but this one". Together they can only
                // produce an empty answer, and an empty answer about a machine
                // reads as a fact about that machine — so it is refused rather
                // than silently returned.
                if args.no_remote && *m != config.machine.name {
                    return Err(QError::Other(format!(
                        "--machine {m} with --no-remote: `{m}` is a remote machine, \
                         and --no-remote suppresses every remote"
                    ))
                    .into());
                }
                Some(m.clone())
            }
            None => None,
        };
        Ok(Ctx {
            json: args.json,
            quiet: args.quiet,
            config,
            machine_override,
            no_remote: args.no_remote,
            confirmed: args.confirmed,
            expect: args.expect.clone(),
            db,
            tmux: tmux::tmux(),
            ssh: remote::ssh().into(),
            bd: beads::client(),
            warnings: Mutex::new(Vec::new()),
        })
    }

    /// Config only, strictly validated. Every `q config` action stops here:
    /// the commands that inspect or repair the environment must not depend on
    /// a database being openable.
    fn config_only(args: &Cli) -> anyhow::Result<Ctx> {
        Ctx::new(args, Config::load()?, None)
    }

    /// Config plus an open database — everything that is not `q config`.
    fn with_db(args: &Cli) -> anyhow::Result<Ctx> {
        let mut ctx = Ctx::config_only(args)?;
        ctx.db = Some(Db::open_default()?);
        Ok(ctx)
    }

    /// The same, for the TUI: a `bd` whose "still waiting…" progress notices
    /// are off. They are stderr writes from inside the call, so nothing the
    /// caller does afterwards can keep them off the alternate screen — and
    /// unlike a warning they are worthless once the call has returned.
    fn for_tui(args: &Cli) -> anyhow::Result<Ctx> {
        let mut ctx = Ctx::with_db(args)?;
        ctx.bd = beads::client_quiet();
        Ok(ctx)
    }

    /// For commands that must work while the file is broken — that is the
    /// state `q config path` and `q config edit` exist to get out of.
    fn lenient(args: &Cli) -> Ctx {
        let config = Config::load().unwrap_or_default();
        Ctx::new(args, config, None).unwrap_or_else(|_| Ctx {
            json: args.json,
            quiet: args.quiet,
            config: Config::default(),
            machine_override: None,
            no_remote: args.no_remote,
            confirmed: args.confirmed,
            expect: args.expect.clone(),
            db: None,
            tmux: tmux::tmux(),
            ssh: remote::ssh().into(),
            bd: beads::client(),
            warnings: Mutex::new(Vec::new()),
        })
    }

    pub fn db(&self) -> anyhow::Result<&Db> {
        self.db
            .as_ref()
            .ok_or_else(|| QError::Db("this command runs without a database".to_string()).into())
    }

    pub fn tmux(&self) -> &dyn Tmux {
        self.tmux.as_ref()
    }

    pub fn ssh(&self) -> &dyn Ssh {
        self.ssh.as_ref()
    }

    /// The same client, for a thread that outlives this borrow — the TUI's
    /// [`remote::Poller`].
    pub fn ssh_shared(&self) -> Arc<dyn Ssh> {
        Arc::clone(&self.ssh)
    }

    pub fn bd(&self) -> &dyn beads::Bd {
        self.bd.as_ref()
    }

    /// Buffer one diagnostic for whoever owns the output — see
    /// [`Ctx::warnings`]. Poison is not news: the message is advisory and the
    /// lock is only ever held for a push.
    pub fn warn(&self, message: impl Into<String>) {
        self.warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(message.into());
    }

    /// Everything buffered since the last drain. Every command that can warn
    /// drains it, so nothing is carried into the next one.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut self.warnings.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// A `Ctx` over an explicit database and tmux, for the in-crate tests.
    /// Both are passed rather than discovered so a test never depends on the
    /// process environment (`Q_DB`, `Q_FIXTURE`) another test may be changing.
    ///
    /// `bd` defaults to one that refuses every call: an in-crate test that
    /// reaches beads without saying so would otherwise shell out to the real
    /// `bd` against the real tracker. [`Ctx::with_bd`] is how a test that
    /// means to exercise the beads paths says so.
    #[cfg(test)]
    pub fn for_tests(config: Config, db: Db, tmux: Box<dyn Tmux>) -> Ctx {
        Ctx {
            json: false,
            quiet: true,
            config,
            machine_override: None,
            no_remote: false,
            confirmed: false,
            expect: None,
            db: Some(db),
            tmux,
            ssh: Arc::new(remote::stub::NoSsh),
            bd: Box::new(beads::stub::NoBd),
            warnings: Mutex::new(Vec::new()),
        }
    }

    #[cfg(test)]
    pub fn with_bd(mut self, bd: Box<dyn beads::Bd>) -> Ctx {
        self.bd = bd;
        self
    }

    #[cfg(test)]
    pub fn with_ssh(mut self, ssh: Arc<dyn Ssh>) -> Ctx {
        self.ssh = ssh;
        self
    }

    #[cfg(test)]
    pub fn with_machine(mut self, machine: Option<&str>) -> Ctx {
        self.machine_override = machine.map(str::to_string);
        self
    }

    #[cfg(test)]
    pub fn with_no_remote(mut self, no_remote: bool) -> Ctx {
        self.no_remote = no_remote;
        self
    }

    #[cfg(test)]
    pub fn with_confirmed(mut self, confirmed: bool) -> Ctx {
        self.confirmed = confirmed;
        self
    }
}

fn main() {
    let args = parse_cli();
    match run(&args) {
        Ok(0) => {}
        // Commands report their own exit code; only `main` ends the process.
        Ok(code) => std::process::exit(code.into()),
        Err(e) => {
            let code = e
                .downcast_ref::<QError>()
                .map(QError::code)
                .unwrap_or("other");
            output::emit_error(args.json, &format!("{e:#}"), code);
            std::process::exit(1);
        }
    }
}

/// Like `Cli::parse()`, but a `--json` caller gets usage errors as
/// `{"error": ..., "code": "usage"}` on stderr instead of clap's plain text.
/// Help/version "errors" (exit code 0) are left to clap either way.
fn parse_cli() -> Cli {
    match Cli::try_parse() {
        Ok(args) => args,
        Err(err) => {
            let json_requested = std::env::args().any(|a| a == "--json");
            if json_requested && err.exit_code() != 0 {
                output::emit_error(true, err.to_string().trim_end(), "usage");
                std::process::exit(2);
            }
            err.exit();
        }
    }
}

/// The process exit code, or an error `main` renders and exits 1 on.
fn run(args: &Cli) -> anyhow::Result<u8> {
    let Some(command) = &args.command else {
        // SPEC §16: bare `q` is the TUI — but only when there is a terminal to
        // draw on. `--json` and a redirected stdout keep the one-line banner,
        // which is what a script reads.
        if !args.json && std::io::stdout().is_terminal() {
            let ctx = Ctx::for_tui(args)?;
            tui::run(&ctx)?;
            return Ok(0);
        }
        let version = env!("CARGO_PKG_VERSION");
        if args.json || !args.quiet {
            output::emit(
                args.json,
                &serde_json::json!({ "version": version, "tui": false }),
                || format!("q {version} — run `q --help` for commands"),
            )?;
        }
        return Ok(0);
    };

    // The three commands that must work without a database — a broken
    // environment is what they exist to inspect or repair.
    match command {
        Command::Config { action } => {
            // No database, whichever action it is.
            let ctx = if needs_valid_config(action.as_ref()) {
                Ctx::config_only(args)?
            } else {
                Ctx::lenient(args)
            };
            return config::run(&ctx, action.as_ref()).map(|()| 0);
        }
        Command::Doctor { fix } => {
            // Diagnosing a broken environment must not require a working one:
            // doctor opens the config and the database itself, and reports
            // whatever it finds.
            return doctor::run(&Ctx::lenient(args), *fix);
        }
        Command::Hook { action } => {
            return match action {
                HookAction::Install { command } => {
                    commands::hook::install(&Ctx::config_only(args)?, command.as_deref())
                }
                HookAction::Uninstall => commands::hook::uninstall(&Ctx::config_only(args)?),
                HookAction::Status { command } => {
                    commands::hook::status(&Ctx::config_only(args)?, command.as_deref())
                }
                // Everything below is a hook firing inside a live Claude session.
                // A Claude q started for its own bookkeeping — naming (SPEC §10) —
                // must not be able to write to the Quest it is naming: its pane
                // environment is scrubbed and its settings are not loaded, and this
                // is the last line of that defence.
                _ if naming::suppressed() => Ok(0),
                // Lenient: a broken config must never break the statusline.
                HookAction::Statusline => commands::hook::statusline(&Ctx::lenient(args)),
                HookAction::SessionStart => hooks::run(hooks::Event::SessionStart),
                HookAction::UserPromptSubmit => hooks::run(hooks::Event::UserPromptSubmit),
                HookAction::Stop => hooks::run(hooks::Event::Stop),
                HookAction::Notification => hooks::run(hooks::Event::Notification),
                HookAction::PreCompact => hooks::run(hooks::Event::PreCompact),
                HookAction::SessionEnd => hooks::run(hooks::Event::SessionEnd),
                // No Ctx: reads `$Q_DB` itself and never creates the database.
                HookAction::PostToolUse => commands::hook_capture::run(),
            };
        }
        _ => {}
    }

    // Everything else runs against the database — and, before it runs here at
    // all, may turn out to belong to another machine (SPEC §15). The proxy is
    // one gate for every command rather than a branch inside each of them; it
    // answers `None` when this machine is the one, which is always the case
    // with no `[[remotes]]` configured or under `--no-remote`.
    let ctx = Ctx::with_db(args)?;
    if let Some(code) = commands::proxy::dispatch(&ctx, command)? {
        return Ok(code);
    }
    // The receiving half of that same gate: a command that arrived over ssh
    // carries the identity it was resolved against, and this machine refuses
    // it unless the target still names that Quest here (SPEC §15).
    commands::proxy::verify(&ctx, command)?;
    local(&ctx, command)
}

/// The command, on this machine.
fn local(ctx: &Ctx, command: &Command) -> anyhow::Result<u8> {
    match command {
        Command::Config { .. } | Command::Doctor { .. } | Command::Hook { .. } => {
            unreachable!("handled before the database is opened")
        }
        Command::New {
            name,
            goal,
            dir,
            workflow,
            repo,
            no_beads,
            prompt,
            prompt_file,
            no_auto_reset,
            detach,
        } => {
            commands::new::run(
                ctx,
                &commands::new::Args {
                    name: name.as_deref(),
                    goal: goal.as_deref(),
                    dir: dir.as_deref(),
                    workflow: workflow.as_deref(),
                    repo: repo.as_deref(),
                    no_beads: *no_beads,
                    prompt: prompt.as_deref(),
                    prompt_file: prompt_file.as_deref(),
                    no_auto_reset: *no_auto_reset,
                    detach: *detach,
                    // The global `--machine` already decides this; only the
                    // TUI's form sets it per Quest.
                    machine: None,
                    // `q tpl run` (bd-8lz.5) is what will set this from the
                    // CLI; `q new` has no template.
                    template: None,
                },
            )
            .map(|()| 0)
        }
        Command::List { all, state } => commands::list::run(ctx, *all, *state).map(|()| 0),
        Command::Show { quest } => commands::show::run(ctx, quest).map(|()| 0),
        Command::Enter { quest, session } => {
            commands::enter::run(ctx, quest, session.as_deref()).map(|()| 0)
        }
        Command::Close {
            quest,
            force,
            close_epic,
        } => commands::close::run(ctx, quest, *force, *close_epic).map(|()| 0),
        Command::Resume {
            quest,
            prompt,
            detach,
        } => commands::resume::run(ctx, quest, prompt.as_deref(), *detach).map(|()| 0),
        Command::Spawn {
            quest,
            prompt,
            label,
            workflow,
            dir,
            no_attach,
        } => commands::spawn::run(
            ctx,
            &commands::spawn::Args {
                quest,
                label,
                prompt,
                workflow: workflow.as_deref(),
                dir: dir.as_deref(),
                no_attach: *no_attach,
            },
        )
        .map(|()| 0),
        Command::Sessions { quest, all } => commands::sessions::run(
            ctx,
            &commands::sessions::Args {
                quest: quest.as_deref(),
                all: *all,
            },
        )
        .map(|()| 0),
        Command::Peek { session, lines } => commands::peek::run(ctx, session, *lines).map(|()| 0),
        Command::Send {
            session,
            text,
            force,
        } => commands::send::run(
            ctx,
            &commands::send::Args {
                session,
                text,
                force: *force,
            },
        )
        .map(|()| 0),
        Command::Reset {
            session,
            delay,
            strategy,
        } => commands::reset::run(
            ctx,
            &commands::reset::Args {
                session,
                delay: *delay,
                strategy: strategy.map(Into::into),
            },
        ),
        Command::Kill { session, force } => commands::kill::run(ctx, session, *force).map(|()| 0),
        Command::Rename { quest, slug } => commands::rename::run(ctx, quest, slug).map(|()| 0),
        Command::Name {
            quest,
            auto,
            apply,
            refresh,
            detach,
            force,
        } => commands::name::run(
            ctx,
            &commands::name::Args {
                quest,
                auto: *auto,
                apply: *apply,
                refresh: *refresh,
                detach: *detach,
                force: *force,
            },
        )
        .map(|()| 0),
        Command::Set { quest, key, value } => {
            commands::set::run(ctx, quest, *key, value).map(|()| 0)
        }
        Command::Brief {
            quest,
            r#for,
            session,
        } => commands::brief::run(
            ctx,
            &commands::brief::Args {
                quest: quest.as_deref(),
                role: r#for.map(Into::into),
                session: session.as_deref(),
            },
        )
        .map(|()| 0),
        Command::Events {
            quest,
            follow,
            kinds,
            limit,
            session,
        } => commands::events::run(
            ctx,
            &commands::events::Args {
                quest: quest.as_deref(),
                kinds,
                session: session.as_deref(),
                limit: *limit,
                follow: *follow,
            },
        )
        .map(|()| 0),
        Command::Rm { quest, force } => commands::rm::run(ctx, quest, *force).map(|()| 0),
        // Agent self-report (bd-8lz.2.5).
        Command::Phase { text, quest } => {
            commands::phase::run(ctx, text, quest.as_deref()).map(|()| 0)
        }
        Command::Note {
            text,
            blocker,
            quest,
        } => commands::note::run(ctx, text, *blocker, quest.as_deref()).map(|()| 0),
        Command::Link { action } => match action {
            LinkAction::Add {
                r#ref,
                kind,
                title,
                quest,
            } => commands::link::add(
                ctx,
                &commands::link::AddArgs {
                    r#ref,
                    kind: *kind,
                    title: title.as_deref(),
                    quest: quest.as_deref(),
                },
            ),
            LinkAction::Rm { id, quest } => commands::link::rm(ctx, *id, quest.as_deref()),
        }
        .map(|()| 0),
        Command::Links { quest, refresh } => {
            commands::link::list(ctx, quest.as_deref(), *refresh).map(|()| 0)
        }
        Command::Artifact { action } => match action {
            ArtifactAction::Add { path, note, quest } => {
                commands::link::add_artifact(ctx, path, note.as_deref(), quest.as_deref())
            }
        }
        .map(|()| 0),
    }
}

fn needs_valid_config(action: Option<&ConfigAction>) -> bool {
    !matches!(
        action,
        Some(ConfigAction::Path) | Some(ConfigAction::Edit) | Some(ConfigAction::Set { .. })
    )
}
