use ratatui::crossterm::event::{KeyCode, KeyModifiers};

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
    /// Text arriving via bracketed paste while composing — inserted verbatim
    /// (newlines included; never submits mid-paste).
    PasteText(String),
    /// Cursor movement within the compose buffer.
    CursorLeft,
    CursorRight,
    CursorHome,
    CursorEnd,
    /// Insert a newline into the compose buffer (alt+enter).
    NewLine,
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
    /// Move plan selection down (PlanReview phase only).
    PlanSelectDown,
    /// Move plan selection up (PlanReview phase only).
    PlanSelectUp,
    /// Begin editing the selected task (PlanReview phase only).
    PlanEditBegin,
    /// Delete the selected task (PlanReview phase only).
    PlanDeleteTask,
    /// Move selected task up in the list (PlanReview phase only).
    PlanMoveUp,
    /// Move selected task down in the list (PlanReview phase only).
    PlanMoveDown,
    /// Begin composing a plan steer (PlanReview phase only).
    PlanSteerBegin,
    None,
}

/// Map a key to an action.
///
/// When `composing_remedy` is true, typing feeds the remedy buffer.
/// When `plan_mode` is true (PlanReview phase, not composing), j/k/e/d/</>
/// drive plan selection and editing instead of their default bindings.
pub fn map_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    composing_remedy: bool,
    plan_mode: bool,
) -> Action {
    if composing_remedy {
        return match code {
            KeyCode::Enter if modifiers.contains(KeyModifiers::ALT) => Action::NewLine,
            KeyCode::Char(c) => Action::RemedyChar(c),
            KeyCode::Backspace => Action::RemedyBackspace,
            KeyCode::Enter => Action::RemedySubmit,
            KeyCode::Esc => Action::RemedyCancel,
            KeyCode::Left => Action::CursorLeft,
            KeyCode::Right => Action::CursorRight,
            KeyCode::Home => Action::CursorHome,
            KeyCode::End => Action::CursorEnd,
            _ => Action::None,
        };
    }

    if plan_mode {
        return match code {
            KeyCode::Char('r') => Action::PlanSteerBegin,
            KeyCode::Char('j') | KeyCode::Down => Action::PlanSelectDown,
            KeyCode::Char('k') | KeyCode::Up => Action::PlanSelectUp,
            KeyCode::Char('e') => Action::PlanEditBegin,
            KeyCode::Char('d') => Action::PlanDeleteTask,
            KeyCode::Char('<') => Action::PlanMoveUp,
            KeyCode::Char('>') => Action::PlanMoveDown,
            KeyCode::Char('y') => Action::Ready,
            KeyCode::Char('K') => Action::AbandonBegin,
            KeyCode::Char('q') => Action::Quit,
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
        assert_eq!(
            map_key(KeyCode::Char('g'), KeyModifiers::NONE, false, false),
            Action::Go
        );
        assert_eq!(
            map_key(KeyCode::Char('r'), KeyModifiers::NONE, false, false),
            Action::NoGoBegin
        );
        assert_eq!(
            map_key(KeyCode::Char('e'), KeyModifiers::NONE, false, false),
            Action::HandEdit
        );
        assert_eq!(
            map_key(KeyCode::Char('q'), KeyModifiers::NONE, false, false),
            Action::Quit
        );
        assert_eq!(
            map_key(KeyCode::Char('l'), KeyModifiers::NONE, false, false),
            Action::ToggleLink
        );
        assert_eq!(
            map_key(KeyCode::Char('p'), KeyModifiers::NONE, false, false),
            Action::PromptBegin
        );
        assert_eq!(
            map_key(KeyCode::Char('z'), KeyModifiers::NONE, false, false),
            Action::None
        );
    }

    #[test]
    fn remedy_composition_captures_text() {
        assert_eq!(
            map_key(KeyCode::Char('x'), KeyModifiers::NONE, true, false),
            Action::RemedyChar('x')
        );
        assert_eq!(
            map_key(KeyCode::Enter, KeyModifiers::NONE, true, false),
            Action::RemedySubmit
        );
        assert_eq!(
            map_key(KeyCode::Esc, KeyModifiers::NONE, true, false),
            Action::RemedyCancel
        );
        // 'g' while composing is text, not a Go verdict.
        assert_eq!(
            map_key(KeyCode::Char('g'), KeyModifiers::NONE, true, false),
            Action::RemedyChar('g')
        );
    }

    #[test]
    fn scroll_keys_always_active() {
        assert_eq!(
            map_key(KeyCode::Char('j'), KeyModifiers::NONE, false, false),
            Action::ScrollDown
        );
        assert_eq!(
            map_key(KeyCode::Down, KeyModifiers::NONE, false, false),
            Action::ScrollDown
        );
        assert_eq!(
            map_key(KeyCode::Char('k'), KeyModifiers::NONE, false, false),
            Action::ScrollUp
        );
        assert_eq!(
            map_key(KeyCode::Up, KeyModifiers::NONE, false, false),
            Action::ScrollUp
        );
        assert_eq!(
            map_key(KeyCode::PageDown, KeyModifiers::NONE, false, false),
            Action::PageDown
        );
        assert_eq!(
            map_key(KeyCode::PageUp, KeyModifiers::NONE, false, false),
            Action::PageUp
        );
    }

    #[test]
    fn abandon_begin_requires_uppercase_k() {
        assert_eq!(
            map_key(KeyCode::Char('K'), KeyModifiers::NONE, false, false),
            Action::AbandonBegin
        );
        // lowercase k is scroll up, not abandon
        assert_eq!(
            map_key(KeyCode::Char('k'), KeyModifiers::NONE, false, false),
            Action::ScrollUp
        );
    }

    #[test]
    fn tab_always_cycles_file() {
        assert_eq!(
            map_key(KeyCode::Tab, KeyModifiers::NONE, false, false),
            Action::CycleFile
        );
    }

    #[test]
    fn remedy_mode_overrides_scroll_keys() {
        // When composing a remedy, scroll keys do not fire.
        assert_eq!(
            map_key(KeyCode::Char('j'), KeyModifiers::NONE, true, false),
            Action::RemedyChar('j')
        );
        assert_eq!(
            map_key(KeyCode::Char('g'), KeyModifiers::NONE, true, false),
            Action::RemedyChar('g')
        );
    }

    #[test]
    fn plan_mode_maps_selection_and_edit_keys() {
        assert_eq!(
            map_key(KeyCode::Char('j'), KeyModifiers::NONE, false, true),
            Action::PlanSelectDown
        );
        assert_eq!(
            map_key(KeyCode::Down, KeyModifiers::NONE, false, true),
            Action::PlanSelectDown
        );
        assert_eq!(
            map_key(KeyCode::Char('k'), KeyModifiers::NONE, false, true),
            Action::PlanSelectUp
        );
        assert_eq!(
            map_key(KeyCode::Up, KeyModifiers::NONE, false, true),
            Action::PlanSelectUp
        );
        assert_eq!(
            map_key(KeyCode::Char('e'), KeyModifiers::NONE, false, true),
            Action::PlanEditBegin
        );
        assert_eq!(
            map_key(KeyCode::Char('d'), KeyModifiers::NONE, false, true),
            Action::PlanDeleteTask
        );
        assert_eq!(
            map_key(KeyCode::Char('<'), KeyModifiers::NONE, false, true),
            Action::PlanMoveUp
        );
        assert_eq!(
            map_key(KeyCode::Char('>'), KeyModifiers::NONE, false, true),
            Action::PlanMoveDown
        );
        assert_eq!(
            map_key(KeyCode::Char('y'), KeyModifiers::NONE, false, true),
            Action::Ready
        );
        assert_eq!(
            map_key(KeyCode::Char('K'), KeyModifiers::NONE, false, true),
            Action::AbandonBegin
        );
        assert_eq!(
            map_key(KeyCode::Char('q'), KeyModifiers::NONE, false, true),
            Action::Quit
        );
        // Unbound keys → None
        assert_eq!(
            map_key(KeyCode::Char('g'), KeyModifiers::NONE, false, true),
            Action::None
        );
    }

    #[test]
    fn plan_steer_begin_maps_r_in_plan_mode() {
        assert_eq!(
            map_key(KeyCode::Char('r'), KeyModifiers::NONE, false, true),
            Action::PlanSteerBegin
        );
        // r outside plan mode is NoGoBegin (unchanged)
        assert_eq!(
            map_key(KeyCode::Char('r'), KeyModifiers::NONE, false, false),
            Action::NoGoBegin
        );
    }

    #[test]
    fn composing_takes_priority_over_plan_mode() {
        // When both composing_remedy=true and plan_mode=true, text input wins.
        assert_eq!(
            map_key(KeyCode::Char('j'), KeyModifiers::NONE, true, true),
            Action::RemedyChar('j')
        );
        assert_eq!(
            map_key(KeyCode::Char('e'), KeyModifiers::NONE, true, true),
            Action::RemedyChar('e')
        );
    }
}
