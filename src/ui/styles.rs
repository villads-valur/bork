use ratatui::style::{Color, Modifier, Style};

use crate::types::{AgentStatus, ChecksStatus, ReviewDecision};

// All colors use ANSI 16 palette so they adapt to the user's terminal theme.
// Never use Color::Rgb or Color::Indexed(16+) here.
pub const ACCENT: Color = Color::Cyan;
pub const TEXT: Color = Color::Reset;
pub const DIM: Color = Color::Gray;
pub const BORDER: Color = Color::Gray;

pub fn column_header_style(selected: bool) -> Style {
    if selected {
        Style::default()
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
    }
}

pub fn card_border_style(selected: bool) -> Style {
    if selected {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(BORDER)
    }
}

pub fn card_title_style(selected: bool) -> Style {
    if selected {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT)
    }
}

pub fn agent_status_color(status: &AgentStatus) -> Color {
    match status {
        AgentStatus::Stopped => Color::Gray,
        AgentStatus::Idle => Color::Blue,
        AgentStatus::Busy => Color::Green,
        AgentStatus::WaitingInput
        | AgentStatus::WaitingPermission
        | AgentStatus::WaitingApproval => Color::Yellow,
        AgentStatus::Error => Color::Red,
    }
}

pub fn session_alive_style() -> Style {
    Style::default().fg(Color::Green)
}

pub fn session_dead_style() -> Style {
    Style::default().fg(Color::Gray)
}

pub fn dim_style() -> Style {
    Style::default().fg(DIM)
}

pub fn statusbar_key_style() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn statusbar_desc_style() -> Style {
    Style::default().fg(DIM)
}

pub fn checks_icon(status: Option<ChecksStatus>) -> (&'static str, Color) {
    match status {
        Some(ChecksStatus::Success) => ("✓", Color::Green),
        Some(ChecksStatus::Failure) | Some(ChecksStatus::Error) => ("✗", Color::Red),
        Some(ChecksStatus::Pending) => ("◌", Color::Yellow),
        None => ("–", Color::Gray),
    }
}

pub fn review_icon(decision: Option<ReviewDecision>) -> (&'static str, Color) {
    match decision {
        Some(ReviewDecision::Approved) => ("●", Color::Green),
        Some(ReviewDecision::ChangesRequested) => ("●", Color::Red),
        Some(ReviewDecision::ReviewRequired) => ("○", Color::Yellow),
        None => ("–", Color::Gray),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checks_icon_success() {
        let (icon, color) = checks_icon(Some(ChecksStatus::Success));
        assert_eq!(icon, "✓");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn test_checks_icon_failure() {
        let (icon, color) = checks_icon(Some(ChecksStatus::Failure));
        assert_eq!(icon, "✗");
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn test_checks_icon_error() {
        let (icon, color) = checks_icon(Some(ChecksStatus::Error));
        assert_eq!(icon, "✗");
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn test_checks_icon_pending() {
        let (icon, color) = checks_icon(Some(ChecksStatus::Pending));
        assert_eq!(icon, "◌");
        assert_eq!(color, Color::Yellow);
    }

    #[test]
    fn test_checks_icon_none() {
        let (icon, color) = checks_icon(None);
        assert_eq!(icon, "–");
        assert_eq!(color, Color::Gray);
    }

    #[test]
    fn test_review_icon_approved() {
        let (icon, color) = review_icon(Some(ReviewDecision::Approved));
        assert_eq!(icon, "●");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn test_review_icon_changes_requested() {
        let (icon, color) = review_icon(Some(ReviewDecision::ChangesRequested));
        assert_eq!(icon, "●");
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn test_review_icon_review_required() {
        let (icon, color) = review_icon(Some(ReviewDecision::ReviewRequired));
        assert_eq!(icon, "○");
        assert_eq!(color, Color::Yellow);
    }

    #[test]
    fn test_review_icon_none() {
        let (icon, color) = review_icon(None);
        assert_eq!(icon, "–");
        assert_eq!(color, Color::Gray);
    }
}
