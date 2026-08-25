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

    #[error("`q {0}` is not implemented yet")]
    NotImplemented(String),

    #[error("{0}")]
    Other(String),
}

impl QError {
    pub fn not_implemented(what: &str) -> Self {
        QError::NotImplemented(what.to_string())
    }

    /// Stable, snake_case identifier used in `--json` error payloads.
    pub fn code(&self) -> &'static str {
        match self {
            QError::NotFound(_) => "not_found",
            QError::Ambiguous { .. } => "ambiguous",
            QError::Tmux(_) => "tmux",
            QError::Db(_) => "db",
            QError::Config(_) => "config",
            QError::Io(_) => "io",
            QError::NotImplemented(_) => "not_implemented",
            QError::Other(_) => "other",
        }
    }
}
