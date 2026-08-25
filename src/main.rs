mod cli;
mod config;
mod error;
mod output;

use clap::Parser;

use cli::{Cli, Command, ConfigAction};
use config::Config;
use error::QError;

/// Everything a command needs beyond its own arguments. Built once by the
/// dispatcher and handed to each command.
pub struct Ctx {
    pub json: bool,
    pub quiet: bool,
    pub config: Config,
    machine_override: Option<String>,
}

impl Ctx {
    /// The targeted machine name for this invocation: `--machine` if given
    /// (validated, never written to the config), else the configured one.
    pub fn machine(&self) -> &str {
        self.machine_override
            .as_deref()
            .unwrap_or(&self.config.machine.name)
    }

    fn new(args: &Cli, config: Config) -> anyhow::Result<Ctx> {
        let machine_override = match &args.machine {
            Some(m) => {
                config::validate_machine_name(m)?;
                Some(m.clone())
            }
            None => None,
        };
        Ok(Ctx {
            json: args.json,
            quiet: args.quiet,
            config,
            machine_override,
        })
    }

    fn load(args: &Cli) -> anyhow::Result<Ctx> {
        Ctx::new(args, Config::load()?)
    }

    /// For commands that must work while the file is broken — that is the
    /// state `q config path` and `q config edit` exist to get out of.
    fn lenient(args: &Cli) -> Ctx {
        let config = Config::load().unwrap_or_default();
        Ctx::new(args, config).unwrap_or_else(|_| Ctx {
            json: args.json,
            quiet: args.quiet,
            config: Config::default(),
            machine_override: None,
        })
    }
}

fn main() {
    let args = parse_cli();
    if let Err(e) = run(&args) {
        let code = e
            .downcast_ref::<QError>()
            .map(QError::code)
            .unwrap_or("other");
        output::emit_error(args.json, &format!("{e:#}"), code);
        std::process::exit(1);
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

fn run(args: &Cli) -> anyhow::Result<()> {
    let Some(command) = &args.command else {
        // TODO(M3): launch the TUI.
        let version = env!("CARGO_PKG_VERSION");
        if args.json || !args.quiet {
            output::emit(
                args.json,
                &serde_json::json!({ "version": version, "tui": false }),
                || format!("q {version} — run `q --help` for commands"),
            )?;
        }
        return Ok(());
    };

    match command {
        Command::Config { action } => {
            let ctx = if needs_valid_config(action.as_ref()) {
                Ctx::load(args)?
            } else {
                Ctx::lenient(args)
            };
            config::run(&ctx, action.as_ref())
        }
        other => {
            let _ctx = Ctx::load(args)?;
            Err(QError::not_implemented(other.name()).into())
        }
    }
}

fn needs_valid_config(action: Option<&ConfigAction>) -> bool {
    !matches!(action, Some(ConfigAction::Path) | Some(ConfigAction::Edit))
}
