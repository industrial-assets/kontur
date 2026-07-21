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
    None,
}

/// Map a key to an action.
///
/// When `composing_remedy` is true, typing feeds the remedy buffer.
/// When `plan_mode` is true (PlanReview phase, not composing), j/k/e/d/</>
/// drive plan selection and editing instead of their default bindings.
pub fn map_key(code: KeyCode, composing_remedy: bool, plan_mode: bool) -> Action {
    if composing_remedy {
        return match code {
            KeyCode::Char(c) => Action::RemedyChar(c),
            KeyCode::Backspace => Action::RemedyBackspace,
            KeyCode::Enter => Action::RemedySubmit,
            KeyCode::Esc => Action::RemedyCancel,
            _ => Action::None,
        };
    }

    if plan_mode {
        return match code {
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
    fn scroll_keys_always_active() {
        assert_eq!(map_key(KeyCode::Char('j'), false, false), Action::ScrollDown);
        assert_eq!(map_key(KeyCode::Down, false, false), Action::ScrollDown);
        assert_eq!(map_key(KeyCode::Char('k'), false, false), Action::ScrollUp);
        assert_eq!(map_key(KeyCode::Up, false, false), Action::ScrollUp);
        assert_eq!(map_key(KeyCode::PageDown, false, false), Action::PageDown);
        assert_eq!(map_key(KeyCode::PageUp, false, false), Action::PageUp);
    }

    #[test]
    fn abandon_begin_requires_uppercase_k() {
        assert_eq!(map_key(KeyCode::Char('K'), false, false), Action::AbandonBegin);
        // lowercase k is scroll up, not abandon
        assert_eq!(map_key(KeyCode::Char('k'), false, false), Action::ScrollUp);
    }

    #[test]
    fn tab_always_cycles_file() {
        assert_eq!(map_key(KeyCode::Tab, false, false), Action::CycleFile);
    }

    #[test]
    fn remedy_mode_overrides_scroll_keys() {
        // When composing a remedy, scroll keys do not fire.
        assert_eq!(map_key(KeyCode::Char('j'), true, false), Action::RemedyChar('j'));
        assert_eq!(map_key(KeyCode::Char('g'), true, false), Action::RemedyChar('g'));
    }

    #[test]
    fn plan_mode_maps_selection_and_edit_keys() {
        assert_eq!(map_key(KeyCode::Char('j'), false, true), Action::PlanSelectDown);
        assert_eq!(map_key(KeyCode::Down, false, true), Action::PlanSelectDown);
        assert_eq!(map_key(KeyCode::Char('k'), false, true), Action::PlanSelectUp);
        assert_eq!(map_key(KeyCode::Up, false, true), Action::PlanSelectUp);
        assert_eq!(map_key(KeyCode::Char('e'), false, true), Action::PlanEditBegin);
        assert_eq!(map_key(KeyCode::Char('d'), false, true), Action::PlanDeleteTask);
        assert_eq!(map_key(KeyCode::Char('<'), false, true), Action::PlanMoveUp);
        assert_eq!(map_key(KeyCode::Char('>'), false, true), Action::PlanMoveDown);
        assert_eq!(map_key(KeyCode::Char('y'), false, true), Action::Ready);
        assert_eq!(map_key(KeyCode::Char('K'), false, true), Action::AbandonBegin);
        assert_eq!(map_key(KeyCode::Char('q'), false, true), Action::Quit);
        // Unbound keys → None
        assert_eq!(map_key(KeyCode::Char('g'), false, true), Action::None);
    }

    #[test]
    fn composing_takes_priority_over_plan_mode() {
        // When both composing_remedy=true and plan_mode=true, text input wins.
        assert_eq!(map_key(KeyCode::Char('j'), true, true), Action::RemedyChar('j'));
        assert_eq!(map_key(KeyCode::Char('e'), true, true), Action::RemedyChar('e'));
    }
}
