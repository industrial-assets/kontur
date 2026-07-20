use ratatui::crossterm::event::KeyCode;

/// A mapped operator intent. The app applies these against the GateHost.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    Go,
    NoGoBegin,
    HandEdit,
    Discuss,
    ToggleLink,
    Ready,
    Help,
    Quit,
    AbandonBegin,
    AbandonConfirm,
    PromptBegin,
    RemedyChar(char),
    RemedyBackspace,
    RemedySubmit,
    RemedyCancel,
    /// Scroll the diff viewport down by one line.
    ScrollDown,
    /// Scroll the diff viewport up by one line.
    ScrollUp,
    /// Scroll the diff viewport down by one page.
    PageDown,
    /// Scroll the diff viewport up by one page.
    PageUp,
    /// Cycle the selected file when multiple files are in the diff.
    CycleFile,
    None,
}

/// Map a key to an action.
///
/// When `composing_remedy` is true, typing feeds the remedy buffer.
pub fn map_key(code: KeyCode, composing_remedy: bool) -> Action {
    if composing_remedy {
        return match code {
            KeyCode::Char(c) => Action::RemedyChar(c),
            KeyCode::Backspace => Action::RemedyBackspace,
            KeyCode::Enter => Action::RemedySubmit,
            KeyCode::Esc => Action::RemedyCancel,
            _ => Action::None,
        };
    }

    match code {
        KeyCode::Char('g') => Action::Go,
        KeyCode::Char('r') => Action::NoGoBegin,
        KeyCode::Char('e') => Action::HandEdit,
        KeyCode::Char('d') => Action::Discuss,
        KeyCode::Char('l') => Action::ToggleLink,
        KeyCode::Tab => Action::CycleFile,
        KeyCode::Char('y') => Action::Ready,
        KeyCode::Char('p') => Action::PromptBegin,
        KeyCode::Char('?') => Action::Help,
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('K') => Action::AbandonBegin,
        KeyCode::Char('j') | KeyCode::Down => Action::ScrollDown,
        KeyCode::Char('k') | KeyCode::Up => Action::ScrollUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::PageUp => Action::PageUp,
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_keys_map() {
        assert_eq!(map_key(KeyCode::Char('g'), false), Action::Go);
        assert_eq!(map_key(KeyCode::Char('r'), false), Action::NoGoBegin);
        assert_eq!(map_key(KeyCode::Char('e'), false), Action::HandEdit);
        assert_eq!(map_key(KeyCode::Char('q'), false), Action::Quit);
        assert_eq!(map_key(KeyCode::Char('l'), false), Action::ToggleLink);
        assert_eq!(map_key(KeyCode::Char('p'), false), Action::PromptBegin);
        assert_eq!(map_key(KeyCode::Char('z'), false), Action::None);
    }

    #[test]
    fn remedy_composition_captures_text() {
        assert_eq!(map_key(KeyCode::Char('x'), true), Action::RemedyChar('x'));
        assert_eq!(map_key(KeyCode::Enter, true), Action::RemedySubmit);
        assert_eq!(map_key(KeyCode::Esc, true), Action::RemedyCancel);
        // 'g' while composing is text, not a Go verdict.
        assert_eq!(map_key(KeyCode::Char('g'), true), Action::RemedyChar('g'));
    }

    #[test]
    fn scroll_keys_always_active() {
        assert_eq!(map_key(KeyCode::Char('j'), false), Action::ScrollDown);
        assert_eq!(map_key(KeyCode::Down, false), Action::ScrollDown);
        assert_eq!(map_key(KeyCode::Char('k'), false), Action::ScrollUp);
        assert_eq!(map_key(KeyCode::Up, false), Action::ScrollUp);
        assert_eq!(map_key(KeyCode::PageDown, false), Action::PageDown);
        assert_eq!(map_key(KeyCode::PageUp, false), Action::PageUp);
    }

    #[test]
    fn abandon_begin_requires_uppercase_k() {
        assert_eq!(map_key(KeyCode::Char('K'), false), Action::AbandonBegin);
        // lowercase k is scroll up, not abandon
        assert_eq!(map_key(KeyCode::Char('k'), false), Action::ScrollUp);
    }

    #[test]
    fn tab_always_cycles_file() {
        assert_eq!(map_key(KeyCode::Tab, false), Action::CycleFile);
    }

    #[test]
    fn remedy_mode_overrides_scroll_keys() {
        // When composing a remedy, scroll keys do not fire.
        assert_eq!(map_key(KeyCode::Char('j'), true), Action::RemedyChar('j'));
        assert_eq!(map_key(KeyCode::Char('g'), true), Action::RemedyChar('g'));
    }
}
