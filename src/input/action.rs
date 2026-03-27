#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    MoveUp,
    MoveDown,
    /// h/l: navigate to adjacent issue across columns (flat list traversal)
    FocusLeft,
    FocusRight,
    /// Tab/Shift+Tab: jump to next/prev column
    JumpColumnLeft,
    JumpColumnRight,
    OpenSession,
    KillSession,
    MoveIssueLeft,
    MoveIssueRight,
    ScrollToTop,
    ScrollToBottom,
    ConfirmYes,
    ConfirmNo,
    Noop,
}
