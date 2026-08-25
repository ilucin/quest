mod cli;
mod error;
mod output;

use clap::Parser;

use cli::Cli;
use error::QError;

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

    Err(QError::not_implemented(command.name()).into())
}
