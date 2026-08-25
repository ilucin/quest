# q — repo conventions

- Single crate `q`, Rust 2024 edition. No async runtime; external processes via `std::process`.
- Deps: clap (derive), rusqlite (bundled), serde/serde_json, toml, ratatui, crossterm, chrono, dirs, regex, thiserror/anyhow, sha2, unicode-width.
- Every CLI command supports `--json` (global flag). Human output to stdout, errors to stderr, non-zero exit on failure.
- Overrides via env: `Q_DB` (SQLite path), `Q_CONFIG` (config path). Tests must always set both to temp paths — never touch the real `~/.local/share/q` or `~/.config/q`.
- tmux is wrapped in `src/tmux.rs` behind a trait so tests can stub it (`Q_FIXTURE`).
- Quality gates before every commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- Module layout follows SPEC §21: `main.rs cli.rs config.rs db/ model.rs tmux.rs doctor.rs ...`.
- Comments: sparse, only where code doesn't explain itself.
- Commits: conventional (`feat:`, `fix:`, `chore:`), one PR per beads issue, PR title references the bead id.
