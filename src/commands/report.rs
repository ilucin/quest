//! Where an agent's self-report lands: the Quest from `$Q_QUEST` (or
//! `--quest`), the session from `$Q_SESSION` when the caller runs inside a
//! Quest pane (SPEC §7).

use crate::Ctx;
use crate::error::QError;
use crate::model::{Quest, Session, SessionStatus};

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

/// Read-only commands (`q links <quest>`) only need the Quest; `$Q_SESSION`
/// is ignored so a pane may look at any Quest.
pub fn resolve_quest(ctx: &Ctx, quest_override: Option<&str>) -> anyhow::Result<Quest> {
    let target = match quest_override {
        Some(t) => t.to_string(),
        None => env("Q_QUEST").ok_or_else(|| {
            QError::Other(
                "no quest: run this inside a Quest pane ($Q_QUEST) or pass --quest".to_string(),
            )
        })?,
    };
    ctx.db()?.resolve_quest(&target)
}

/// For writes: the session from `$Q_SESSION` must exist, be alive and belong
/// to the resolved Quest.
pub fn resolve(ctx: &Ctx, quest_override: Option<&str>) -> anyhow::Result<Target> {
    let quest = resolve_quest(ctx, quest_override)?;
    let db = ctx.db()?;

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
            if session.status == SessionStatus::Ended {
                return Err(QError::Invalid(format!(
                    "session `{id}` ($Q_SESSION) has ended; start a new session or pass --quest"
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
