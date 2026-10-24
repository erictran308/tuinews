//! The articles as a list, without pictures: for terminals that draw them
//! badly, or to see more at once (`v`). Each takes two rows and a blank one
//! above: its title (bold while unread, a dot before it), then its source,
//! age and the start of its summary.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::fit;
use crate::app::{App, Focus, GridShape, now};
use crate::article;

/// Rows an article takes: a blank one, its title, and what's under it.
const ROWS: u16 = 3;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let c = app.colors.clone();
    let focused = app.focus == Focus::Grid && app.prompt.is_none();
    // A page is the articles that fit whole.
    let fit_rows = usize::from(area.height / ROWS).max(1);
    app.grid.set(GridShape {
        cols: 1,
        rows: fit_rows,
    });
    if app.selected < app.scroll {
        app.scroll = app.selected;
    } else if app.selected >= app.scroll + fit_rows {
        app.scroll = app.selected + 1 - fit_rows;
    }
    let shown = usize::from(area.height).div_ceil(usize::from(ROWS));
    let first = app.scroll;
    let last = (first + shown).min(app.articles.len());
    // The words start two columns in, under the view's name; the selected
    // one's mark goes in the column before.
    let x = area.x + 2;
    let width = usize::from(area.width.saturating_sub(4));
    for (i, article) in app.articles[first..last].iter().enumerate() {
        let top = area.y + i as u16 * ROWS + 1;
        let rows = Rect {
            x: area.x + 1,
            y: top,
            width: area.width.saturating_sub(2),
            height: 2,
        }
        .intersection(area);
        if rows.is_empty() {
            continue;
        }
        let selected = first + i == app.selected;
        if selected {
            frame.render_widget(Block::new().style(Style::new().bg(c.selected)), rows);
            if focused {
                frame.render_widget(
                    Paragraph::new(vec![Line::from("▌"), Line::from("▌")])
                        .style(Style::new().fg(c.accent)),
                    Rect { width: 1, ..rows },
                );
            }
        }
        let read = app.is_read(&article.id);
        let saved = app.saved.contains(&article.id);
        let title_style = if selected && focused {
            Style::new().fg(c.accent).add_modifier(Modifier::BOLD)
        } else if read {
            Style::new().fg(c.subtext)
        } else {
            Style::new().fg(c.text).add_modifier(Modifier::BOLD)
        };
        let dot = if read {
            Span::raw("  ")
        } else {
            Span::styled(format!("{} ", app.icons.dot), Style::new().fg(c.red))
        };
        let age = article::short_age(article.published, now());
        let mark = if saved {
            format!("{}  ", app.icons.saved)
        } else {
            String::new()
        };
        let end = format!("{mark}{age:>3}");
        let room = width.saturating_sub(2 + end.width() + 2);
        let title = fit(&article.title, room);
        let pad = width.saturating_sub(2 + title.width() + end.width());
        let first_row = Line::from(vec![
            dot,
            Span::styled(title, title_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(mark, Style::new().fg(c.accent)),
            Span::styled(format!("{age:>3}"), Style::new().fg(c.dim)),
        ]);
        // The source and age, then as much of the summary as fits.
        let source = article.source_name();
        let lead = format!("{source} · {}", article::age(article.published, now()));
        let lead = fit(&lead, width.saturating_sub(2));
        let room = width.saturating_sub(2 + lead.width() + 3);
        let mut second = vec![
            Span::raw("  "),
            Span::styled(lead, Style::new().fg(c.subtext)),
        ];
        if room > 10 && app.settings.summaries && !article.summary.is_empty() {
            second.push(Span::styled(" · ", Style::new().fg(c.dim)));
            second.push(Span::styled(
                fit(&article::one_line(&article.summary, room * 2), room),
                Style::new().fg(c.dim),
            ));
        }
        let text = Rect {
            x,
            width: area.width.saturating_sub(4),
            ..rows
        }
        .intersection(area);
        frame.render_widget(Paragraph::new(vec![first_row, Line::from(second)]), text);
    }
}
