use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::InputMode;
use crate::input::action::Action;

pub fn map_key_to_action(key: KeyEvent, mode: InputMode) -> Action {
    match mode {
        InputMode::Normal => map_normal_key(key),
        InputMode::Confirm => map_confirm_key(key),
        InputMode::Dialog => map_dialog_key(key),
        InputMode::Search => map_search_key(key),
        InputMode::LinearPicker => map_linear_picker_key(key),
        InputMode::Help => map_help_key(key),
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

        KeyCode::Char('n') => Action::CreateIssue,
        KeyCode::Char('a') => Action::AddIssue,
        KeyCode::Char('e') => Action::EditIssue,
        KeyCode::Char('d') => Action::DeleteIssue,

        KeyCode::Char('H') => Action::MoveIssueLeft,
        KeyCode::Char('L') => Action::MoveIssueRight,
        KeyCode::Char('D') => Action::MoveToDone,
        KeyCode::Char('T') => Action::MoveToTodo,

        KeyCode::Char('g') => Action::ScrollToTop,
        KeyCode::Char('G') => Action::ScrollToBottom,

        KeyCode::Char('P') => Action::SyncPRs,
        KeyCode::Char('o') => Action::OpenPR,
        KeyCode::Char('W') => Action::AssignWorktree,

        KeyCode::Char('/') => Action::SearchStart,
        KeyCode::Char('?') => Action::ShowHelp,
        KeyCode::Esc => Action::ClearSearch,
        KeyCode::Char('I') => Action::OpenLinearPicker,

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

fn map_search_key(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Action::SearchCancel,
            _ => Action::Noop,
        };
    }

    match key.code {
        KeyCode::Esc => Action::SearchCancel,
        KeyCode::Enter => Action::SearchConfirm,
        KeyCode::Backspace => Action::SearchBackspace,
        KeyCode::Char(c) => Action::SearchChar(c),
        _ => Action::Noop,
    }
}

fn map_linear_picker_key(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Action::LinearPickerClose,
            KeyCode::Char('n') => Action::LinearPickerDown,
            KeyCode::Char('p') => Action::LinearPickerUp,
            _ => Action::Noop,
        };
    }

    match key.code {
        KeyCode::Esc => Action::LinearPickerClose,
        KeyCode::Enter => Action::LinearPickerSelect,
        KeyCode::Down => Action::LinearPickerDown,
        KeyCode::Up => Action::LinearPickerUp,
        KeyCode::Tab => Action::LinearPickerDown,
        KeyCode::BackTab => Action::LinearPickerUp,
        KeyCode::Backspace => Action::LinearPickerBackspace,
        KeyCode::Char(c) => Action::LinearPickerChar(c),
        _ => Action::Noop,
    }
}

fn map_help_key(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => Action::CloseHelp,
        _ => Action::Noop,
    }
}

fn map_dialog_key(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::SHIFT) && key.code == KeyCode::Enter {
        return Action::DialogSubmit;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Action::DialogCancel,
            KeyCode::Char('w') => Action::DialogDeleteWord,
            KeyCode::Char('u') => Action::DialogClearToStart,
            KeyCode::Char('a') => Action::DialogMoveStart,
            KeyCode::Char('e') => Action::DialogMoveEnd,
            _ => Action::Noop,
        };
    }

    match key.code {
        KeyCode::Esc => Action::DialogCancel,
        KeyCode::Enter => Action::DialogNextField,
        KeyCode::Tab => Action::DialogNextField,
        KeyCode::BackTab => Action::DialogPrevField,
        KeyCode::Backspace => Action::DialogBackspace,
        KeyCode::Delete => Action::DialogDelete,
        KeyCode::Left => Action::DialogMoveLeft,
        KeyCode::Right => Action::DialogMoveRight,
        KeyCode::Home => Action::DialogMoveStart,
        KeyCode::End => Action::DialogMoveEnd,
        KeyCode::Char(c) => Action::DialogChar(c),
        _ => Action::Noop,
    }
}
