mod cli;
mod error;
mod output;

use clap::Parser;

use cli::Cli;
use error::QError;

fn main() {
    let args = Cli::parse();
    if let Err(e) = run(&args) {
        output::emit_error(args.json, &format!("{e:#}"));
        std::process::exit(1);
    }
}

fn run(args: &Cli) -> anyhow::Result<()> {
    let Some(command) = &args.command else {
        // TODO(M3): launch the TUI.
        if !args.quiet {
            let version = env!("CARGO_PKG_VERSION");
            output::emit(
                args.json,
                &serde_json::json!({ "version": version, "tui": false }),
                || format!("q {version} — run `q --help` for commands"),
            );
        }
        return Ok(());
    };

    Err(QError::not_implemented(command.name()).into())
}
