//! `q completions <shell>` — print a shell completion script to stdout.
//!
//! No database and no config: the script is generated straight off clap's
//! command tree (SPEC §21), so it works before the environment is set up and is
//! safe to `eval` from a shell rc file. Handled in `main.rs` alongside the other
//! no-DB commands, before any database is opened.

use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::Cli;

/// Write the completion script for `shell` to stdout.
pub fn run(shell: Shell) -> anyhow::Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    generate(shell, &mut cmd, name, &mut std::io::stdout());
    Ok(())
}
