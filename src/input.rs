use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    Next,
    Previous,
    First,
    Last,
    ToggleQueueCurrent,
    UnqueueCurrent,
    ShowDeleteQueueGrid,
    ToggleGrid,
    OpenHighlighted,
    PageDown,
    PageUp,
    ConfirmDeleteQueued,
    ConfirmYes,
    ConfirmNo,
    ToggleFullscreenUi,
    ToggleRecursive,
    Rescan,
    ToggleTimeSort,
    ToggleNameSort,
    ToggleInfoOverlay,
    ToggleHelpOverlay,
    ToggleZoom,
    Noop,
}

pub fn action_for_key(key: KeyEvent, confirm_delete: bool) -> Action {
    if key.kind != KeyEventKind::Press {
        return Action::Noop;
    }

    if confirm_delete {
        return match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Action::ConfirmYes,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Action::ConfirmNo,
            _ => Action::Noop,
        };
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), KeyModifiers::NONE) => Action::Quit,
        (KeyCode::Char('j'), KeyModifiers::NONE) | (KeyCode::Down, KeyModifiers::NONE) => {
            Action::Next
        }
        (KeyCode::Char('k'), KeyModifiers::NONE) | (KeyCode::Up, KeyModifiers::NONE) => {
            Action::Previous
        }
        (KeyCode::Left, KeyModifiers::NONE) => Action::Previous,
        (KeyCode::Right, KeyModifiers::NONE) => Action::Next,
        (KeyCode::Home, KeyModifiers::NONE) => Action::First,
        (KeyCode::End, KeyModifiers::NONE) => Action::Last,
        (KeyCode::Char('d'), KeyModifiers::NONE) | (KeyCode::Char(' '), KeyModifiers::NONE) => {
            Action::ToggleQueueCurrent
        }
        (KeyCode::Char('u'), KeyModifiers::NONE) => Action::UnqueueCurrent,
        (KeyCode::Char('D'), KeyModifiers::SHIFT) | (KeyCode::Char('D'), KeyModifiers::NONE) => {
            Action::ShowDeleteQueueGrid
        }
        (KeyCode::Char('g'), KeyModifiers::NONE) => Action::ToggleGrid,
        (KeyCode::Enter, KeyModifiers::NONE) | (KeyCode::Char('l'), KeyModifiers::NONE) => {
            Action::OpenHighlighted
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => Action::PageDown,
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => Action::PageUp,
        (KeyCode::Char('r'), KeyModifiers::CONTROL) => Action::ConfirmDeleteQueued,
        (KeyCode::Char('f'), KeyModifiers::NONE) => Action::ToggleFullscreenUi,
        (KeyCode::Char('r'), KeyModifiers::NONE) => Action::ToggleRecursive,
        (KeyCode::Char('R'), KeyModifiers::SHIFT) | (KeyCode::Char('R'), KeyModifiers::NONE) => {
            Action::Rescan
        }
        (KeyCode::Char('t'), KeyModifiers::NONE) => Action::ToggleTimeSort,
        (KeyCode::Char('n'), KeyModifiers::NONE) => Action::ToggleNameSort,
        (KeyCode::Char('i'), KeyModifiers::NONE) => Action::ToggleInfoOverlay,
        (KeyCode::Char('h'), KeyModifiers::NONE) | (KeyCode::Char('?'), KeyModifiers::SHIFT) => {
            Action::ToggleHelpOverlay
        }
        (KeyCode::Char('z'), KeyModifiers::NONE) => Action::ToggleZoom,
        _ => Action::Noop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn maps_required_global_keys() {
        assert_eq!(
            action_for_key(key(KeyCode::Char('D'), KeyModifiers::NONE), false),
            Action::ShowDeleteQueueGrid
        );
        assert_eq!(
            action_for_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL), false),
            Action::PageDown
        );
        assert_eq!(
            action_for_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL), false),
            Action::PageUp
        );
        assert_eq!(
            action_for_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL), false),
            Action::ConfirmDeleteQueued
        );
    }

    #[test]
    fn confirmation_modal_captures_n() {
        assert_eq!(
            action_for_key(key(KeyCode::Char('n'), KeyModifiers::NONE), true),
            Action::ConfirmNo
        );
        assert_eq!(
            action_for_key(key(KeyCode::Char('n'), KeyModifiers::NONE), false),
            Action::ToggleNameSort
        );
    }
}
