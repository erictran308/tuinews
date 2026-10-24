//! The sidebar, in a box with the logo in its border: Home, Search, Saved,
//! History, then under a rule your categories, and under another your
//! feeds by name, each with how many of its articles you haven't read (a
//! feed marked if its last fetch failed).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::fit;
use crate::app::{App, Entry, Focus, MENU};
use crate::icons::Icons;

fn label(entry: Entry, icons: &Icons) -> (&'static str, &'static str) {
    match entry {
        Entry::Home => (icons.home, "Home"),
        Entry::Search => (icons.search, "Search"),
        Entry::Saved => (icons.saved, "Saved"),
        Entry::History => (icons.history, "History"),
        Entry::Category(_) | Entry::Feed(_) => ("", ""),
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let c = app.colors.clone();
    let focused = app.focus == Focus::Sidebar && app.prompt.is_none();
    let line = super::border(focused, &c);
    // The logo, then the name.
    let mut logo = vec![Span::styled("─ ", line)];
    logo.extend(super::logo(app.icons.logo));
    logo.push(Span::styled(
        " tuinews ",
        Style::new().fg(c.text).add_modifier(Modifier::BOLD),
    ));
    let logo = Line::from(logo);
    let block = super::bordered().border_style(line).title(logo);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;

    // A name with a count after it, dim, at the right edge.
    let counted = |name: &str, count: String, style: Style| {
        let name = fit(name, width.saturating_sub(4 + count.width()));
        let pad = width.saturating_sub(2 + name.width() + count.width() + 1);
        Line::from(vec![
            Span::styled(format!("  {name}"), style),
            Span::raw(" ".repeat(pad)),
            Span::styled(count, Style::new().fg(c.dim)),
        ])
    };

    // Every row, with the sidebar entry it selects, if any.
    let mut rows: Vec<(Option<usize>, Line)> = Vec::new();
    for (i, entry) in MENU.iter().enumerate() {
        let (icon, name) = label(*entry, &app.icons);
        let count = match entry {
            Entry::Home => app.home_unread,
            Entry::Saved => app.saved.len(),
            _ => 0,
        };
        let count = if count > 0 {
            count.to_string()
        } else {
            String::new()
        };
        rows.push((
            Some(i),
            counted(&format!("{icon}  {name}"), count, Style::new()),
        ));
    }
    // Across the box, meeting its border on both sides, as tuigram's.
    let mut rules = Vec::new();
    let mut rule = |rows: &mut Vec<(Option<usize>, Line)>, title: String| {
        rules.push(rows.len());
        let heading = fit(&title, usize::from(area.width).saturating_sub(4));
        let fill = usize::from(area.width).saturating_sub(3 + heading.width());
        rows.push((
            None,
            Line::from(vec![
                Span::styled("├─", line),
                Span::styled(heading, Style::new().fg(c.dim)),
                Span::styled(format!("{}┤", "─".repeat(fill)), line),
            ]),
        ));
    };
    let unread = |n: usize| if n > 0 { n.to_string() } else { String::new() };
    let categories_from = MENU.len();
    if !app.categories.is_empty() {
        rule(
            &mut rows,
            format!(" Categories ({}) ", app.categories.len()),
        );
        for (i, category) in app.categories.iter().enumerate() {
            rows.push((
                Some(categories_from + i),
                counted(&category.name, unread(category.unread), Style::new()),
            ));
        }
    }
    let feeds_from = categories_from + app.categories.len();
    let listed: Vec<_> = app.listed().cloned().collect();
    rule(&mut rows, format!(" Feeds ({}) ", listed.len()));
    if listed.is_empty() {
        for hint in ["  None yet: press a", "  to add a feed"] {
            rows.push((None, Line::from(Span::styled(hint, Style::new().fg(c.dim)))));
        }
    }
    for (i, feed) in listed.iter().enumerate() {
        let (count, style) = match &feed.error {
            // Its last fetch failed: marked, with its count if it has one.
            Some(_) if feed.unread > 0 => (
                format!("{} {}", app.icons.failed, feed.unread),
                Style::new().fg(c.subtext),
            ),
            Some(_) => (app.icons.failed.to_string(), Style::new().fg(c.subtext)),
            None if feed.unread > 0 => (unread(feed.unread), Style::new()),
            None => (String::new(), Style::new().fg(c.subtext)),
        };
        rows.push((Some(feeds_from + i), counted(&feed.name(), count, style)));
    }

    // Keep the selected row on screen.
    let height = inner.height as usize;
    let selected_row = rows
        .iter()
        .position(|(entry, _)| *entry == Some(app.sidebar_selected))
        .unwrap_or(0);
    let mut scroll = app.sidebar_scroll.get();
    if selected_row < scroll {
        scroll = selected_row.saturating_sub(1);
    } else if selected_row >= scroll + height {
        scroll = selected_row + 1 - height;
    }
    app.sidebar_scroll.set(scroll);

    // The row of the view shown, while the sidebar's cursor is elsewhere.
    let shown_row = app.row_of(&app.view);
    for (y, (i, (entry, line))) in rows
        .into_iter()
        .enumerate()
        .skip(scroll)
        .take(height)
        .enumerate()
    {
        let row = Rect {
            y: inner.y + y as u16,
            height: 1,
            ..inner
        };
        if rules.contains(&i) {
            frame.render_widget(
                Paragraph::new(line),
                Rect {
                    x: area.x,
                    width: area.width,
                    ..row
                },
            );
            continue;
        }
        let selected = entry == Some(app.sidebar_selected);
        let style = if selected && focused {
            Style::new()
                .bg(c.selected)
                .fg(c.accent)
                .add_modifier(Modifier::BOLD)
        } else if selected || (entry.is_some() && entry == shown_row) {
            Style::new().bg(c.selected).fg(c.text)
        } else {
            Style::new().fg(c.text)
        };
        frame.render_widget(Paragraph::new(line).style(style), row);
        if selected && focused {
            frame.render_widget(
                Paragraph::new(Span::styled("▌", Style::new().fg(c.accent))),
                Rect { width: 1, ..row },
            );
        }
    }
}
