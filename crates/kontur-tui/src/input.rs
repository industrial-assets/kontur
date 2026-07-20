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
    /// Scroll the diff viewport down by one line (valid while diff open).
    ScrollDown,
    /// Scroll the diff viewport up by one line (valid while diff open).
    ScrollUp,
    /// Scroll the diff viewport down by one page (valid while diff open).
    PageDown,
    /// Scroll the diff viewport up by one page (valid while diff open).
    PageUp,
    /// Cycle the selected file when multiple files are in the diff.
    CycleFile,
    None,
}

/// Map a key to an action.
///
/// When `composing_remedy` is true, typing feeds the remedy buffer.
/// When `diff_open` is true, scroll keys are active and `g` approves from the
/// viewer (FR-24: operators can approve from inside the diff viewer).
pub fn map_key(code: KeyCode, composing_remedy: bool, diff_open: bool) -> Action {
    if composing_remedy {
        return match code {
            KeyCode::Char(c) => Action::RemedyChar(c),
            KeyCode::Backspace => Action::RemedyBackspace,
            KeyCode::Enter => Action::RemedySubmit,
            KeyCode::Esc => Action::RemedyCancel,
            _ => Action::None,
        };
    }

    // Diff-open mode: scroll keys, approve, and file cycle all active.
    // `o` still works to close the diff.
    if diff_open {
        return match code {
            KeyCode::Char('j') | KeyCode::Down => Action::ScrollDown,
            KeyCode::Char('k') | KeyCode::Up => Action::ScrollUp,
            KeyCode::PageDown => Action::PageDown,
            KeyCode::PageUp => Action::PageUp,
            KeyCode::Tab => Action::CycleFile,
            // FR-24: approve from inside the viewer.
            KeyCode::Char('g') => Action::Go,
            // No-go is always castable.
            KeyCode::Char('r') => Action::NoGoBegin,
            // Close the diff viewer.
            KeyCode::Char('o') => Action::OpenDiff,
            // Hand-edit from the viewer.
            KeyCode::Char('e') => Action::HandEdit,
            KeyCode::Char('q') => Action::Quit,
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
        assert_eq!(map_key(KeyCode::Char('g'), false, false), Action::Go);
        assert_eq!(map_key(KeyCode::Char('r'), false, false), Action::NoGoBegin);
        assert_eq!(map_key(KeyCode::Char('e'), false, false), Action::HandEdit);
        assert_eq!(map_key(KeyCode::Char('q'), false, false), Action::Quit);
        assert_eq!(map_key(KeyCode::Char('l'), false, false), Action::ToggleLink);
        assert_eq!(map_key(KeyCode::Char('p'), false, false), Action::PromptBegin);
        assert_eq!(map_key(KeyCode::Char('z'), false, false), Action::None);
    }

    #[test]
    fn remedy_composition_captures_text() {
        assert_eq!(map_key(KeyCode::Char('x'), true, false), Action::RemedyChar('x'));
        assert_eq!(map_key(KeyCode::Enter, true, false), Action::RemedySubmit);
        assert_eq!(map_key(KeyCode::Esc, true, false), Action::RemedyCancel);
        // 'g' while composing is text, not a Go verdict.
        assert_eq!(map_key(KeyCode::Char('g'), true, false), Action::RemedyChar('g'));
    }

    #[test]
    fn scroll_keys_active_when_diff_open() {
        assert_eq!(map_key(KeyCode::Char('j'), false, true), Action::ScrollDown);
        assert_eq!(map_key(KeyCode::Down, false, true), Action::ScrollDown);
        assert_eq!(map_key(KeyCode::Char('k'), false, true), Action::ScrollUp);
        assert_eq!(map_key(KeyCode::Up, false, true), Action::ScrollUp);
        assert_eq!(map_key(KeyCode::PageDown, false, true), Action::PageDown);
        assert_eq!(map_key(KeyCode::PageUp, false, true), Action::PageUp);
    }

    #[test]
    fn scroll_keys_inactive_when_diff_closed() {
        // j/k are not scroll actions when the diff is closed.
        assert_eq!(map_key(KeyCode::Char('j'), false, false), Action::None);
        // k is AbandonBegin when diff closed.
        assert_eq!(map_key(KeyCode::Char('k'), false, false), Action::AbandonBegin);
    }

    #[test]
    fn go_from_diff_viewer() {
        // FR-24: 'g' in diff-open mode maps to Go.
        assert_eq!(map_key(KeyCode::Char('g'), false, true), Action::Go);
    }

    #[test]
    fn tab_cycles_file_when_diff_open() {
        assert_eq!(map_key(KeyCode::Tab, false, true), Action::CycleFile);
    }

    #[test]
    fn tab_is_none_when_diff_closed() {
        assert_eq!(map_key(KeyCode::Tab, false, false), Action::None);
    }

    #[test]
    fn close_diff_with_o_when_diff_open() {
        assert_eq!(map_key(KeyCode::Char('o'), false, true), Action::OpenDiff);
    }

    #[test]
    fn remedy_mode_overrides_diff_open() {
        // When composing a remedy, scroll/diff keys do not fire.
        assert_eq!(map_key(KeyCode::Char('j'), true, true), Action::RemedyChar('j'));
        assert_eq!(map_key(KeyCode::Char('g'), true, true), Action::RemedyChar('g'));
    }
}
