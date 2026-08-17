use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::ui::styles;

const PICKER_MIN_WIDTH: u16 = 50;
const PICKER_MAX_WIDTH: u16 = 100;
const VISIBLE_ITEMS: usize = 10;

pub fn render_link_picker(frame: &mut Frame, app: &App) {
    let Some(picker) = &app.link_picker else {
        return;
    };

    let candidates = app.link_picker_candidates();
    let count = candidates.len();

    let area = frame.area();
    let width = (area.width * 70 / 100).clamp(PICKER_MIN_WIDTH, PICKER_MAX_WIDTH);
    let height = (VISIBLE_ITEMS as u16 + 7).min(area.height);
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;

    let picker_area = Rect::new(x, y, width, height);
    frame.render_widget(Clear, picker_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(styles::ACCENT))
        .title(Span::styled(
            format!(" Link {} to ", picker.anchor_id),
            Style::default()
                .fg(styles::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(picker_area);
    frame.render_widget(block, picker_area);

    if inner.height < 4 || inner.width < 10 {
        return;
    }

    let field_width = inner.width.saturating_sub(2) as usize;
    let mut row_y = inner.y + 1;

    let search_area = Rect::new(inner.x + 1, row_y, inner.width - 2, 1);
    let search_line = Line::from(vec![
        Span::styled(
            "Search: ",
            Style::default()
                .fg(styles::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(&picker.search, Style::default().fg(styles::TEXT)),
        Span::styled("\u{2588}", Style::default().fg(styles::ACCENT)),
    ]);
    frame.render_widget(Paragraph::new(search_line), search_area);
    row_y += 1;

    let divider_area = Rect::new(inner.x + 1, row_y, inner.width - 2, 1);
    let divider = Line::from(Span::styled(
        "\u{2500}".repeat(field_width),
        styles::dim_style(),
    ));
    frame.render_widget(Paragraph::new(divider), divider_area);
    row_y += 1;

    let list_start_y = row_y;
    let available_rows = inner.height.saturating_sub(row_y - inner.y + 2) as usize;
    let visible_count = available_rows.min(VISIBLE_ITEMS);

    let scroll = if visible_count == 0 || picker.selected < visible_count {
        0
    } else {
        picker.selected - visible_count + 1
    };

    if count == 0 {
        let empty_area = Rect::new(inner.x + 1, list_start_y, inner.width - 2, 1);
        let msg = if app.active_project().issues.len() <= 1 {
            "No other issues to link"
        } else {
            "No matching issues"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, styles::dim_style()))),
            empty_area,
        );
    } else {
        for i in 0..visible_count {
            let idx = scroll + i;
            if idx >= count {
                break;
            }

            let (id, title, linked) = &candidates[idx];
            let is_selected = idx == picker.selected;
            let y = list_start_y + i as u16;
            let row_area = Rect::new(inner.x + 1, y, inner.width - 2, 1);

            let pointer = if is_selected { "\u{25b8} " } else { "  " };
            let pointer_style = if is_selected {
                Style::default()
                    .fg(styles::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let mark = if *linked { "\u{2713} " } else { "  " };
            let mark_style = if *linked {
                Style::default().fg(styles::ACCENT)
            } else {
                Style::default()
            };

            let overhead = 2 + mark.len() + id.len() + 1;
            let title_budget = field_width.saturating_sub(overhead);
            let title_text = styles::truncate(title, title_budget);

            let line = Line::from(vec![
                Span::styled(pointer, pointer_style),
                Span::styled(mark, mark_style),
                Span::styled(id, styles::dim_style()),
                Span::raw(" "),
                Span::styled(title_text, Style::default().fg(styles::TEXT)),
            ]);

            frame.render_widget(Paragraph::new(line), row_area);
        }
    }

    let footer_y = inner.y + inner.height - 1;
    let footer_area = Rect::new(inner.x + 1, footer_y, inner.width - 2, 1);
    let footer_spans = vec![
        Span::styled("Enter", styles::statusbar_key_style()),
        Span::styled(":toggle  ", styles::statusbar_desc_style()),
        Span::styled("\u{2191}\u{2193}", styles::statusbar_key_style()),
        Span::styled(":navigate  ", styles::statusbar_desc_style()),
        Span::styled("Esc", styles::statusbar_key_style()),
        Span::styled(":close", styles::statusbar_desc_style()),
        Span::styled(
            format!("  {}/{}", count.min(picker.selected + 1), count),
            styles::dim_style(),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(footer_spans)), footer_area);
}
