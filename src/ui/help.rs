use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, InputMode};
use crate::ui::styles;

const HELP_WIDTH: u16 = 42;
const KEY_COL_WIDTH: usize = 16;

struct Section {
    title: &'static str,
    bindings: &'static [(&'static str, &'static str)],
}

const TUICR_SECTION: Section = Section {
    title: "Review",
    bindings: &[("r", "Review changes"), ("R", "Review PR (--pr)")],
};

const DEBUG_SECTION: Section = Section {
    title: "Debug",
    bindings: &[
        ("Ctrl+r", "Reset (kill + clear pid)"),
        ("Ctrl+e", "Inspect issue JSON"),
    ],
};

const PROJECTS_SECTION: Section = Section {
    title: "Projects",
    bindings: &[
        ("Ctrl+P", "Toggle project sidebar"),
        ("Space", "Toggle swimlane (sidebar)"),
        ("Tab / S-Tab", "Switch swimlane focus"),
    ],
};

const SECTIONS: &[Section] = &[
    Section {
        title: "Navigation",
        bindings: &[
            ("h / l / \u{2190}\u{2192}", "Jump column"),
            ("j / k / \u{2191}\u{2193}", "Move up / down"),
            ("g / G", "Top / bottom"),
        ],
    },
    Section {
        title: "Issues",
        bindings: &[
            ("n", "New issue"),
            ("a", "Add in column"),
            ("e", "Edit issue"),
            ("d", "Delete issue"),
        ],
    },
    Section {
        title: "Sessions",
        bindings: &[
            ("Enter", "Open session"),
            ("t", "Terminal"),
            ("x", "Kill session"),
        ],
    },
    Section {
        title: "Move Issues",
        bindings: &[
            ("H / L", "Move left / right"),
            ("D", "Move to Done"),
            ("T", "Move to To Do"),
        ],
    },
    Section {
        title: "Other",
        bindings: &[
            ("/", "Search"),
            ("P", "Sync PRs"),
            ("o", "Open PR in browser"),
            ("O", "Open in Linear"),
            ("I", "Import from Linear"),
            ("W", "Assign worktree"),
            ("q", "Quit"),
        ],
    },
];

fn content_height(tuicr: bool, debug: bool, multi_project: bool) -> u16 {
    let mut section_rows: u16 = SECTIONS.iter().map(|s| 1 + s.bindings.len() as u16).sum();
    let mut count = SECTIONS.len();
    if tuicr {
        section_rows += 1 + TUICR_SECTION.bindings.len() as u16;
        count += 1;
    }
    if multi_project {
        section_rows += 1 + PROJECTS_SECTION.bindings.len() as u16;
        count += 1;
    }
    if debug {
        section_rows += 1 + DEBUG_SECTION.bindings.len() as u16;
        count += 1;
    }
    let gaps = count.saturating_sub(1) as u16;
    section_rows + gaps + 3 // +3: top padding, bottom padding, footer
}

pub fn render_help(frame: &mut Frame, app: &App) {
    if app.input_mode != InputMode::Help {
        return;
    }

    let area = frame.area();
    let debug = app.project().config.debug;
    let tuicr = app.project().tuicr_available;
    let multi_project = app.sidebar.is_some();
    let width = HELP_WIDTH.min(area.width);
    let height = (content_height(tuicr, debug, multi_project) + 2).min(area.height); // +2 for border
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;

    let popup_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(styles::ACCENT))
        .title(Span::styled(
            " Keybindings ",
            Style::default()
                .fg(styles::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 5 || inner.width < 10 {
        return;
    }

    let max_row = inner.height.saturating_sub(1); // reserve last row for footer
    let mut row: u16 = 0;

    let mut all_sections: Vec<&Section> = SECTIONS.iter().collect();
    if tuicr {
        all_sections.push(&TUICR_SECTION);
    }
    if multi_project {
        all_sections.push(&PROJECTS_SECTION);
    }
    if debug {
        all_sections.push(&DEBUG_SECTION);
    }

    for (i, section) in all_sections.iter().enumerate() {
        if i > 0 {
            row += 1;
        }
        if row >= max_row {
            break;
        }

        let title_line = Line::from(Span::styled(
            format!("  {}", section.title),
            Style::default()
                .fg(styles::TEXT)
                .add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(
            Paragraph::new(title_line),
            Rect::new(inner.x, inner.y + row, inner.width, 1),
        );
        row += 1;

        for (key, desc) in section.bindings {
            if row >= max_row {
                break;
            }
            let line = Line::from(vec![
                Span::styled(
                    format!("  {:<width$}", key, width = KEY_COL_WIDTH),
                    styles::statusbar_key_style(),
                ),
                Span::styled(*desc, styles::statusbar_desc_style()),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, inner.y + row, inner.width, 1),
            );
            row += 1;
        }
    }

    let footer = Line::from(vec![
        Span::styled("  Esc", styles::statusbar_key_style()),
        Span::styled(" to close", styles::statusbar_desc_style()),
    ]);
    frame.render_widget(
        Paragraph::new(footer),
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    );
}
