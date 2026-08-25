use thiserror::Error;

/// Domain errors. Top-level code uses `anyhow::Result`; these carry the cases
/// callers (and `--json` output) need to distinguish. Variants are constructed
/// by the milestones that own each subsystem.
#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum QError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("ambiguous target `{target}`: {}", candidates.join(", "))]
    Ambiguous {
        target: String,
        candidates: Vec<String>,
    },

    #[error("tmux: {0}")]
    Tmux(String),

    #[error("db: {0}")]
    Db(String),

    #[error("config: {0}")]
    Config(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl QError {
    pub fn not_implemented(what: &str) -> Self {
        QError::Other(format!("`q {what}` is not implemented yet"))
    }
}
