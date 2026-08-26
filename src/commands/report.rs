//! Where an agent's self-report lands: the Quest from `$Q_QUEST` (or
//! `--quest`), the session from `$Q_SESSION` when the caller runs inside a
//! Quest pane (SPEC §7).

use crate::Ctx;
use crate::error::QError;
use crate::model::{Quest, Session};

pub struct Target {
    pub quest: Quest,
    /// `None` when invoked outside a Quest pane (manual CLI use).
    pub session: Option<Session>,
}

impl Target {
    pub fn session_id(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.id.as_str())
    }

    /// For commands that only make sense from inside a session (`q phase`).
    pub fn require_session(&self) -> anyhow::Result<&Session> {
        self.session.as_ref().ok_or_else(|| {
            QError::Other(
                "no session: run this inside a Quest pane ($Q_SESSION is not set)".to_string(),
            )
            .into()
        })
    }
}

pub fn resolve(ctx: &Ctx, quest_override: Option<&str>) -> anyhow::Result<Target> {
    let db = ctx.db()?;
    let target = match quest_override {
        Some(t) => t.to_string(),
        None => env("Q_QUEST").ok_or_else(|| {
            QError::Other(
                "no quest: run this inside a Quest pane ($Q_QUEST) or pass --quest".to_string(),
            )
        })?,
    };
    let quest = db.resolve_quest(&target)?;

    let session = match env("Q_SESSION") {
        None => None,
        Some(id) => {
            let session = db
                .get_session(&id)?
                .ok_or_else(|| QError::NotFound(format!("session `{id}` ($Q_SESSION)")))?;
            if session.quest_id != quest.id {
                return Err(QError::Invalid(format!(
                    "session `{id}` ($Q_SESSION) belongs to quest {}, not {}",
                    session.quest_id, quest.id
                ))
                .into());
            }
            Some(session)
        }
    };
    Ok(Target { quest, session })
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}
