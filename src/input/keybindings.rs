use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{DialogField, InputMode};
use crate::input::action::Action;

pub fn map_key_to_action(
    key: KeyEvent,
    mode: InputMode,
    dialog_field: Option<DialogField>,
) -> Action {
    match mode {
        InputMode::Normal => map_normal_key(key),
        InputMode::Confirm => map_confirm_key(key),
        InputMode::Dialog => {
            if dialog_field == Some(DialogField::Prompt) {
                map_dialog_prompt_key(key)
            } else {
                map_dialog_key(key)
            }
        }
        InputMode::Search => map_search_key(key),
        InputMode::LinearPicker => map_linear_picker_key(key),
        InputMode::Help => map_help_key(key),
        InputMode::DebugInspector => map_debug_inspector_key(key),
        InputMode::Sidebar => map_sidebar_key(key),
    }
}

fn map_normal_key(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Action::Quit,
            KeyCode::Char('p') => Action::ToggleSidebar,
            KeyCode::Char('r') => Action::DebugReset,
            KeyCode::Char('e') => Action::DebugInspect,
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
        KeyCode::Char('t') => Action::OpenTerminal,
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

        KeyCode::Char('r') => Action::OpenReview,
        KeyCode::Char('R') => Action::OpenReviewPR,

        KeyCode::Char('P') => Action::SyncPRs,
        KeyCode::Char('o') => Action::OpenPR,
        KeyCode::Char('O') => Action::OpenLinear,
        KeyCode::Char('W') => Action::AssignWorktree,

        KeyCode::Char('/') => Action::SearchStart,
        KeyCode::Char('?') => Action::ShowHelp,
        KeyCode::Esc => Action::ClearSearch,
        KeyCode::Char('I') => Action::OpenLinearPicker,

        _ => Action::Noop,
    }
}

fn map_sidebar_key(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Action::Quit,
            KeyCode::Char('p') => Action::ToggleSidebar,
            _ => Action::Noop,
        };
    }

    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('j') | KeyCode::Down => Action::SidebarDown,
        KeyCode::Char('k') | KeyCode::Up => Action::SidebarUp,
        KeyCode::Enter => Action::SidebarSelect,
        KeyCode::Esc => Action::ToggleSidebar,
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
            KeyCode::Char('r') => Action::LinearPickerRefresh,
            KeyCode::Char('l') => Action::PickerSwitchTab,
            KeyCode::Char('h') => Action::PickerSwitchTab,
            _ => Action::Noop,
        };
    }

    match key.code {
        KeyCode::Esc => Action::LinearPickerClose,
        KeyCode::Enter => Action::LinearPickerSelect,
        KeyCode::Down => Action::LinearPickerDown,
        KeyCode::Up => Action::LinearPickerUp,
        KeyCode::Tab => Action::PickerSwitchTab,
        KeyCode::BackTab => Action::PickerSwitchTab,
        KeyCode::Left => Action::PickerSwitchTab,
        KeyCode::Right => Action::PickerSwitchTab,
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

fn map_debug_inspector_key(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => Action::DebugInspectorClose,
        KeyCode::Char('j') | KeyCode::Down => Action::DebugInspectorScrollDown,
        KeyCode::Char('k') | KeyCode::Up => Action::DebugInspectorScrollUp,
        KeyCode::Char('g') => Action::DebugInspectorScrollTop,
        KeyCode::Char('G') => Action::DebugInspectorScrollBottom,
        _ => Action::Noop,
    }
}

fn map_dialog_prompt_key(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::SHIFT) && key.code == KeyCode::Enter {
        return Action::DialogSubmit;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => return Action::DialogCancel,
            KeyCode::Char('s') => return Action::DialogSubmit,
            KeyCode::Char('e') => return Action::DialogOpenEditor,
            _ => {}
        }
    }

    match key.code {
        KeyCode::Esc => Action::DialogCancel,
        KeyCode::Tab => Action::DialogNextField,
        KeyCode::BackTab => Action::DialogPrevField,
        _ => Action::DialogPromptKey(key),
    }
}

#[cfg(test)]
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[cfg(test)]
fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

#[cfg(test)]
fn shift_enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
}

fn map_dialog_key(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::SHIFT) && key.code == KeyCode::Enter {
        return Action::DialogSubmit;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Action::DialogCancel,
            KeyCode::Char('s') => Action::DialogSubmit,
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- Normal mode ---

    #[test]
    fn normal_quit() {
        assert_eq!(map_normal_key(key(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(map_normal_key(ctrl('c')), Action::Quit);
    }

    #[test]
    fn normal_navigation() {
        assert_eq!(map_normal_key(key(KeyCode::Char('j'))), Action::MoveDown);
        assert_eq!(map_normal_key(key(KeyCode::Down)), Action::MoveDown);
        assert_eq!(map_normal_key(key(KeyCode::Char('k'))), Action::MoveUp);
        assert_eq!(map_normal_key(key(KeyCode::Up)), Action::MoveUp);
        assert_eq!(map_normal_key(key(KeyCode::Char('h'))), Action::FocusLeft);
        assert_eq!(map_normal_key(key(KeyCode::Left)), Action::FocusLeft);
        assert_eq!(map_normal_key(key(KeyCode::Char('l'))), Action::FocusRight);
        assert_eq!(map_normal_key(key(KeyCode::Right)), Action::FocusRight);
    }

    #[test]
    fn normal_jump_columns() {
        assert_eq!(map_normal_key(key(KeyCode::Tab)), Action::JumpColumnRight);
        assert_eq!(
            map_normal_key(key(KeyCode::BackTab)),
            Action::JumpColumnLeft
        );
    }

    #[test]
    fn normal_session_actions() {
        assert_eq!(map_normal_key(key(KeyCode::Enter)), Action::OpenSession);
        assert_eq!(
            map_normal_key(key(KeyCode::Char('t'))),
            Action::OpenTerminal
        );
        assert_eq!(map_normal_key(key(KeyCode::Char('x'))), Action::KillSession);
    }

    #[test]
    fn normal_issue_crud() {
        assert_eq!(map_normal_key(key(KeyCode::Char('n'))), Action::CreateIssue);
        assert_eq!(map_normal_key(key(KeyCode::Char('a'))), Action::AddIssue);
        assert_eq!(map_normal_key(key(KeyCode::Char('e'))), Action::EditIssue);
        assert_eq!(map_normal_key(key(KeyCode::Char('d'))), Action::DeleteIssue);
    }

    #[test]
    fn normal_move_issue() {
        assert_eq!(
            map_normal_key(key(KeyCode::Char('H'))),
            Action::MoveIssueLeft
        );
        assert_eq!(
            map_normal_key(key(KeyCode::Char('L'))),
            Action::MoveIssueRight
        );
        assert_eq!(map_normal_key(key(KeyCode::Char('D'))), Action::MoveToDone);
        assert_eq!(map_normal_key(key(KeyCode::Char('T'))), Action::MoveToTodo);
    }

    #[test]
    fn normal_scroll() {
        assert_eq!(map_normal_key(key(KeyCode::Char('g'))), Action::ScrollToTop);
        assert_eq!(
            map_normal_key(key(KeyCode::Char('G'))),
            Action::ScrollToBottom
        );
    }

    #[test]
    fn normal_review() {
        assert_eq!(map_normal_key(key(KeyCode::Char('r'))), Action::OpenReview);
        assert_eq!(
            map_normal_key(key(KeyCode::Char('R'))),
            Action::OpenReviewPR
        );
    }

    #[test]
    fn normal_external() {
        assert_eq!(map_normal_key(key(KeyCode::Char('P'))), Action::SyncPRs);
        assert_eq!(map_normal_key(key(KeyCode::Char('o'))), Action::OpenPR);
        assert_eq!(map_normal_key(key(KeyCode::Char('O'))), Action::OpenLinear);
        assert_eq!(
            map_normal_key(key(KeyCode::Char('W'))),
            Action::AssignWorktree
        );
        assert_eq!(
            map_normal_key(key(KeyCode::Char('I'))),
            Action::OpenLinearPicker
        );
    }

    #[test]
    fn normal_search_and_help() {
        assert_eq!(map_normal_key(key(KeyCode::Char('/'))), Action::SearchStart);
        assert_eq!(map_normal_key(key(KeyCode::Char('?'))), Action::ShowHelp);
        assert_eq!(map_normal_key(key(KeyCode::Esc)), Action::ClearSearch);
    }

    #[test]
    fn normal_noop() {
        assert_eq!(map_normal_key(key(KeyCode::Char('z'))), Action::Noop);
        assert_eq!(map_normal_key(key(KeyCode::F(1))), Action::Noop);
        assert_eq!(map_normal_key(ctrl('x')), Action::Noop);
    }

    // --- Confirm mode ---

    #[test]
    fn confirm_yes() {
        assert_eq!(map_confirm_key(key(KeyCode::Char('y'))), Action::ConfirmYes);
        assert_eq!(map_confirm_key(key(KeyCode::Enter)), Action::ConfirmYes);
    }

    #[test]
    fn confirm_no() {
        assert_eq!(map_confirm_key(key(KeyCode::Char('n'))), Action::ConfirmNo);
        assert_eq!(map_confirm_key(key(KeyCode::Esc)), Action::ConfirmNo);
    }

    #[test]
    fn confirm_noop() {
        assert_eq!(map_confirm_key(key(KeyCode::Char('x'))), Action::Noop);
    }

    // --- Search mode ---

    #[test]
    fn search_cancel() {
        assert_eq!(map_search_key(key(KeyCode::Esc)), Action::SearchCancel);
        assert_eq!(map_search_key(ctrl('c')), Action::SearchCancel);
    }

    #[test]
    fn search_confirm_and_backspace() {
        assert_eq!(map_search_key(key(KeyCode::Enter)), Action::SearchConfirm);
        assert_eq!(
            map_search_key(key(KeyCode::Backspace)),
            Action::SearchBackspace
        );
    }

    #[test]
    fn search_char() {
        assert_eq!(
            map_search_key(key(KeyCode::Char('a'))),
            Action::SearchChar('a')
        );
    }

    // --- Linear picker mode ---

    #[test]
    fn picker_close() {
        assert_eq!(
            map_linear_picker_key(key(KeyCode::Esc)),
            Action::LinearPickerClose
        );
        assert_eq!(map_linear_picker_key(ctrl('c')), Action::LinearPickerClose);
    }

    #[test]
    fn picker_navigation() {
        assert_eq!(
            map_linear_picker_key(key(KeyCode::Enter)),
            Action::LinearPickerSelect
        );
        assert_eq!(
            map_linear_picker_key(key(KeyCode::Down)),
            Action::LinearPickerDown
        );
        assert_eq!(
            map_linear_picker_key(key(KeyCode::Up)),
            Action::LinearPickerUp
        );
        assert_eq!(map_linear_picker_key(ctrl('n')), Action::LinearPickerDown);
        assert_eq!(map_linear_picker_key(ctrl('p')), Action::LinearPickerUp);
    }

    #[test]
    fn picker_tab_switch() {
        assert_eq!(
            map_linear_picker_key(key(KeyCode::Tab)),
            Action::PickerSwitchTab
        );
        assert_eq!(
            map_linear_picker_key(key(KeyCode::BackTab)),
            Action::PickerSwitchTab
        );
        assert_eq!(
            map_linear_picker_key(key(KeyCode::Left)),
            Action::PickerSwitchTab
        );
        assert_eq!(
            map_linear_picker_key(key(KeyCode::Right)),
            Action::PickerSwitchTab
        );
        assert_eq!(map_linear_picker_key(ctrl('l')), Action::PickerSwitchTab);
        assert_eq!(map_linear_picker_key(ctrl('h')), Action::PickerSwitchTab);
    }

    #[test]
    fn picker_search() {
        assert_eq!(
            map_linear_picker_key(key(KeyCode::Backspace)),
            Action::LinearPickerBackspace
        );
        assert_eq!(
            map_linear_picker_key(key(KeyCode::Char('f'))),
            Action::LinearPickerChar('f')
        );
    }

    #[test]
    fn picker_refresh() {
        assert_eq!(
            map_linear_picker_key(ctrl('r')),
            Action::LinearPickerRefresh
        );
    }

    // --- Help mode ---

    #[test]
    fn help_close() {
        assert_eq!(map_help_key(key(KeyCode::Esc)), Action::CloseHelp);
    }

    #[test]
    fn help_noop() {
        assert_eq!(map_help_key(key(KeyCode::Char('a'))), Action::Noop);
    }

    // --- Dialog (non-prompt) ---

    #[test]
    fn dialog_submit_shift_enter() {
        assert_eq!(map_dialog_key(shift_enter()), Action::DialogSubmit);
    }

    #[test]
    fn dialog_cancel() {
        assert_eq!(map_dialog_key(key(KeyCode::Esc)), Action::DialogCancel);
        assert_eq!(map_dialog_key(ctrl('c')), Action::DialogCancel);
    }

    #[test]
    fn dialog_field_nav() {
        assert_eq!(map_dialog_key(key(KeyCode::Enter)), Action::DialogNextField);
        assert_eq!(map_dialog_key(key(KeyCode::Tab)), Action::DialogNextField);
        assert_eq!(
            map_dialog_key(key(KeyCode::BackTab)),
            Action::DialogPrevField
        );
    }

    #[test]
    fn dialog_editing() {
        assert_eq!(
            map_dialog_key(key(KeyCode::Backspace)),
            Action::DialogBackspace
        );
        assert_eq!(map_dialog_key(key(KeyCode::Delete)), Action::DialogDelete);
        assert_eq!(
            map_dialog_key(key(KeyCode::Char('a'))),
            Action::DialogChar('a')
        );
    }

    #[test]
    fn dialog_cursor() {
        assert_eq!(map_dialog_key(key(KeyCode::Left)), Action::DialogMoveLeft);
        assert_eq!(map_dialog_key(key(KeyCode::Right)), Action::DialogMoveRight);
        assert_eq!(map_dialog_key(key(KeyCode::Home)), Action::DialogMoveStart);
        assert_eq!(map_dialog_key(key(KeyCode::End)), Action::DialogMoveEnd);
    }

    #[test]
    fn dialog_ctrl_editing() {
        assert_eq!(map_dialog_key(ctrl('w')), Action::DialogDeleteWord);
        assert_eq!(map_dialog_key(ctrl('u')), Action::DialogClearToStart);
        assert_eq!(map_dialog_key(ctrl('a')), Action::DialogMoveStart);
        assert_eq!(map_dialog_key(ctrl('e')), Action::DialogMoveEnd);
    }

    // --- Dialog prompt mode ---

    #[test]
    fn prompt_submit_shift_enter() {
        assert_eq!(map_dialog_prompt_key(shift_enter()), Action::DialogSubmit);
    }

    #[test]
    fn prompt_cancel_and_nav() {
        assert_eq!(
            map_dialog_prompt_key(key(KeyCode::Esc)),
            Action::DialogCancel
        );
        assert_eq!(map_dialog_prompt_key(ctrl('c')), Action::DialogCancel);
        assert_eq!(
            map_dialog_prompt_key(key(KeyCode::Tab)),
            Action::DialogNextField
        );
        assert_eq!(
            map_dialog_prompt_key(key(KeyCode::BackTab)),
            Action::DialogPrevField
        );
    }

    #[test]
    fn prompt_passthrough() {
        // Regular keys get wrapped as DialogPromptKey
        let k = key(KeyCode::Enter);
        assert_eq!(map_dialog_prompt_key(k), Action::DialogPromptKey(k));
        let c = key(KeyCode::Char('x'));
        assert_eq!(map_dialog_prompt_key(c), Action::DialogPromptKey(c));
    }

    // --- Top-level dispatch ---

    #[test]
    fn dispatch_by_mode() {
        let k = key(KeyCode::Char('q'));
        assert_eq!(map_key_to_action(k, InputMode::Normal, None), Action::Quit,);
        assert_eq!(map_key_to_action(k, InputMode::Confirm, None), Action::Noop,);
        assert_eq!(
            map_key_to_action(k, InputMode::Search, None),
            Action::SearchChar('q'),
        );
    }

    #[test]
    fn dispatch_dialog_prompt_vs_other_field() {
        let enter = key(KeyCode::Enter);
        assert_eq!(
            map_key_to_action(enter, InputMode::Dialog, Some(DialogField::Title)),
            Action::DialogNextField,
        );
        assert_eq!(
            map_key_to_action(enter, InputMode::Dialog, Some(DialogField::Prompt)),
            Action::DialogPromptKey(enter),
        );
    }
}
