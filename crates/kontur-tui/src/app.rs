use std::io::{self, Stdout};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::Terminal;
use std::time::Duration;

use crate::input::{map_key, Action};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Restores the terminal on drop — including on panic (a panic hook runs the
/// same restore before the default hook). Constructing it enters raw mode +
/// the alternate screen.
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> io::Result<(Self, Tui)> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, event::EnableBracketedPaste)?;
        // Restore the terminal on panic BEFORE the default hook prints, so the
        // backtrace isn't swallowed by the alternate screen / raw mode.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            TerminalGuard::restore();
            prev(info);
        }));
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok((TerminalGuard, terminal))
    }

    /// Restore the terminal explicitly (idempotent with Drop).
    pub fn restore() {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            event::DisableBracketedPaste,
            LeaveAlternateScreen
        );
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        TerminalGuard::restore();
    }
}

/// Poll for the next operator action, or `None` on timeout (so the loop can
/// refresh the view periodically). `composing_remedy` switches key semantics
/// to text input; `plan_mode` switches j/k/e/d/</>  to plan-selection actions.
pub fn poll_action(
    timeout: Duration,
    composing_remedy: bool,
    plan_mode: bool,
    clarify_mode: bool,
) -> io::Result<Option<Action>> {
    if event::poll(timeout)? {
        match event::read()? {
            Event::Key(key) => {
                return Ok(Some(map_key(
                    key.code,
                    key.modifiers,
                    composing_remedy,
                    plan_mode,
                    clarify_mode,
                )));
            }
            // Bracketed paste: only meaningful while composing; inserted
            // verbatim so embedded newlines can never submit mid-paste.
            Event::Paste(text) if composing_remedy => {
                return Ok(Some(Action::PasteText(text)));
            }
            _ => {}
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_is_safe_to_call_without_entering() {
        // Should not panic even when raw mode was never enabled.
        TerminalGuard::restore();
    }
}
