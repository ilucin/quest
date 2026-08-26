//! Templates tab — placeholder shell (SPEC §17).
//!
//! Template list and one-keypress run (SPEC §11) lands in **bd-8lz.5**. The seams below are what that bead fills in:
//! `State` grows the tab's own data and selection, `handle` its keymap,
//! `refresh` its loading. Nothing here reaches a terminal.
#![allow(dead_code)]

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::Ctx;

use super::app::{Action, App};
use super::keys::Input;

/// Per-tab state, owned by `App`.
#[derive(Debug, Default)]
pub struct State {}

/// Reload this tab's data. Called by the event loop on tick and on `x`, never
/// from the state machine, so `App::handle` stays pure.
pub fn refresh(_ctx: &Ctx, _app: &mut App) -> anyhow::Result<()> {
    Ok(())
}

/// Keys the shell did not claim. Returning `Action::None` leaves them unbound.
pub fn handle(_app: &mut App, _input: Input) -> Action {
    Action::None
}

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    super::placeholder(frame, area, "Templates", "bd-8lz.5", app);
}
