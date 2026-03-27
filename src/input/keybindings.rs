use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::InputMode;
use crate::input::action::Action;

pub fn map_key_to_action(key: KeyEvent, mode: InputMode) -> Action {
    match mode {
        InputMode::Normal => map_normal_key(key),
        InputMode::Confirm => map_confirm_key(key),
    }
}

fn map_normal_key(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Action::Quit,
            _ => Action::Noop,
        };
    }

    match key.code {
        KeyCode::Char('q') => Action::Quit,

        KeyCode::Char('j') | KeyCode::Down => Action::MoveDown,
        KeyCode::Char('k') | KeyCode::Up => Action::MoveUp,
        KeyCode::Char('h') | KeyCode::Left => Action::FocusLeft,
        KeyCode::Char('l') | KeyCode::Right => Action::FocusRight,

        KeyCode::Tab => Action::JumpColumnRight,
        KeyCode::BackTab => Action::JumpColumnLeft,

        KeyCode::Enter => Action::OpenSession,
        KeyCode::Char('x') => Action::KillSession,

        KeyCode::Char('H') => Action::MoveIssueLeft,
        KeyCode::Char('L') => Action::MoveIssueRight,

        KeyCode::Char('g') => Action::ScrollToTop,
        KeyCode::Char('G') => Action::ScrollToBottom,

        _ => Action::Noop,
    }
}

fn map_confirm_key(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => Action::ConfirmYes,
        KeyCode::Char('n') | KeyCode::Esc => Action::ConfirmNo,
        _ => Action::Noop,
    }
}
