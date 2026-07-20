use ratatui::crossterm::event::KeyCode;

/// A mapped operator intent. The app applies these against the GateHost.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    Go,
    NoGoBegin,
    HandEdit,
    Discuss,
    OpenDiff,
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
    None,
}

/// Map a key to an action. When composing a remedy, typing feeds the remedy
/// buffer; otherwise gate/global keys apply.
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
        KeyCode::Char('o') => Action::OpenDiff,
        KeyCode::Char('l') => Action::ToggleLink,
        KeyCode::Tab => Action::None,
        KeyCode::Char('y') => Action::Ready,
        KeyCode::Char('p') => Action::PromptBegin,
        KeyCode::Char('?') => Action::Help,
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('k') => Action::AbandonBegin,
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
}
