//! The articles, in a box with the search box in its border: what they
//! are, then a grid of cards: the picture, then on a shade of its own the
//! source and age (with a dot while it's unread), the title on up to three
//! lines and the start of the summary. Or a list instead (`list.rs`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui_image::{FontSize, Image};
use unicode_width::UnicodeWidthStr;

use super::{fit, wrap_lines};
use crate::app::{App, Focus, GridShape, View, now};
use crate::article::{self, Article};
use crate::images::{Key, Subject};
use crate::theme::Colors;

/// Columns between the edge of a card's words' shade and what's on it.
const PAD: u16 = 1;
/// Rows a card's title takes.
const TITLE_ROWS: u16 = 3;
/// Rows of the summary under it, when summaries show.
const SUMMARY_ROWS: u16 = 2;

/// How the cards are laid out in an area.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub cols: usize,
    pub rows: usize,
    /// A card's place, ring included.
    pub slot_width: u16,
    pub slot_height: u16,
    /// A card inside its ring.
    pub card_width: u16,
    pub picture_rows: u16,
}

/// As many cards of at least `min_width` columns as fit side by side; the
/// picture 16:9 in pixels, from the terminal's cell size, but short enough
/// that a card's words fit under it in the window.
pub fn geometry(area: Rect, min_width: u16, font: FontSize, summaries: bool) -> Geometry {
    let cols = (area.width / (min_width + 2)).max(1);
    let slot_width = area.width / cols;
    let card_width = slot_width.saturating_sub(2).max(1);
    // A blank row opening the words' shade, the source and age, the title,
    // the summary, a blank row closing the shade.
    let words = 1 + 1 + TITLE_ROWS + if summaries { SUMMARY_ROWS } else { 0 } + 1;
    let (fw, fh) = (f32::from(font.width.max(1)), f32::from(font.height.max(1)));
    let room = f32::from(area.height.saturating_sub(words + 2).max(3));
    let picture_rows = (f32::from(card_width) * fw * 9.0 / 16.0 / fh)
        .round()
        .clamp(3.0, 40.0)
        .min(room) as u16;
    // Ring, picture, words, ring.
    let slot_height = 1 + picture_rows + words + 1;
    let rows = (area.height / slot_height).max(1) as usize;
    Geometry {
        cols: cols as usize,
        rows,
        slot_width,
        slot_height,
        card_width,
        picture_rows,
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let c = app.colors.clone();
    // The search box is in the border: it's lit while you type in it.
    let line = super::border(app.focus == Focus::Grid || app.prompt.is_some(), &c);
    let search = super::search_box(app, area.width, 0, line);
    let block = super::bordered().border_style(line).title(search);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let header = Rect {
        height: inner.height.min(1),
        ..inner
    };
    frame.render_widget(
        Paragraph::new(Line::from(super::header_spans(
            app,
            usize::from(header.width),
        ))),
        header,
    );
    // A column in from the border, so the selected card's ring doesn't
    // touch it.
    let cards = Rect {
        x: inner.x + 1,
        y: inner.y + 1,
        width: inner.width.saturating_sub(2),
        height: inner.height.saturating_sub(1),
    };
    if app.articles.is_empty() {
        empty(frame, app, cards);
        return;
    }
    if app.shows_list() {
        // From the border on: the titles start under the view's name and
        // the search box, the selected one's mark in the column before.
        let rows = Rect {
            x: inner.x,
            width: inner.width,
            ..cards
        };
        return super::list::draw(frame, app, rows);
    }
    let g = geometry(
        cards,
        app.settings.card_width(),
        app.images.font_size(),
        app.settings.summaries,
    );
    app.grid.set(GridShape {
        cols: g.cols,
        rows: g.rows,
    });
    let row = app.selected / g.cols;
    if row < app.scroll {
        app.scroll = row;
    } else if row >= app.scroll + g.rows {
        app.scroll = row + 1 - g.rows;
    }
    // The rows that fit, and the start of the next one in what's left, as
    // a news site's page shows the top of the next row.
    let shown_rows = usize::from(cards.height)
        .div_ceil(usize::from(g.slot_height))
        .max(1);
    let first = app.scroll * g.cols;
    let last = (first + shown_rows * g.cols).min(app.articles.len());
    let visible: Vec<Article> = app.articles[first..last].to_vec();
    for (i, article) in visible.iter().enumerate() {
        let slot = Rect {
            x: cards.x + (i % g.cols) as u16 * g.slot_width,
            y: cards.y + (i / g.cols) as u16 * g.slot_height,
            width: g.slot_width,
            height: g.slot_height,
        };
        let selected = first + i == app.selected;
        card(frame, app, &c, article, slot, cards, &g, selected);
    }
}

/// What a view says when it has no articles.
fn empty(frame: &mut Frame, app: &App, area: Rect) {
    let c = &app.colors;
    let text = |s: &'static str| (s, Style::new().fg(c.subtext));
    let dim = |s: &'static str| (s, Style::new().fg(c.dim));
    let lines: Vec<(&str, Style)> = match &app.view {
        View::Home if app.feeds.is_empty() => vec![
            (
                "No feeds",
                Style::new().fg(c.text).add_modifier(Modifier::BOLD),
            ),
            ("", Style::new()),
            text("Press a and paste a feed's address, or a news site's."),
        ],
        View::Category(id, _)
            if app
                .categories
                .iter()
                .any(|c| c.id == *id && c.feeds.is_empty()) =>
        {
            vec![
                text("No feeds in this category yet."),
                ("", Style::new()),
                dim("a here adds one to it; F on a feed in the sidebar puts that one in."),
            ]
        }
        View::Home | View::Feed(..) | View::Category(..) if app.refreshing.is_some() => {
            vec![text("Fetching the newest articles…")]
        }
        View::Home | View::Feed(..) | View::Category(..) if app.settings.unread_only => vec![
            text("Nothing unread here."),
            ("", Style::new()),
            dim("u shows the articles you've read too."),
        ],
        View::Home => vec![text("No articles yet. R fetches your feeds.")],
        View::Feed(..) => vec![
            text("No articles from this feed. R fetches it again."),
            ("", Style::new()),
            dim("If it never has any, x in the sidebar removes it."),
        ],
        View::Category(..) => vec![
            text("No articles in this category yet. R fetches its feeds."),
            ("", Style::new()),
            dim("F on an article shows its feed's categories."),
        ],
        View::Search(_) => vec![
            text("Nothing found in your articles."),
            ("", Style::new()),
            dim("Search looks in titles, summaries and sources, with or without accents."),
        ],
        View::Saved => vec![text(
            "Nothing saved yet. Press w on an article to keep it here.",
        )],
        View::History if !app.settings.history => {
            vec![text("History is off (O, then Privacy, turns it on).")]
        }
        View::History => vec![text(
            "Articles you open show up here. They're kept only on this computer.",
        )],
    };
    let width = lines.iter().map(|(l, _)| l.width()).max().unwrap_or(0) as u16;
    let height = lines.len() as u16;
    let block = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 3,
        width: width.min(area.width),
        height: height.min(area.height),
    };
    let text: Vec<Line> = lines
        .into_iter()
        .map(|(l, s)| Line::from(Span::styled(fit(l, area.width as usize), s)))
        .collect();
    frame.render_widget(Paragraph::new(text), block);
}

/// Whether all of `r` is within `bounds`.
fn inside(r: Rect, bounds: Rect) -> bool {
    r.intersection(bounds) == r
}

/// An article's picture in `area`, of which only what's within `bounds` is
/// drawn, or a placeholder until it's ready: the source's name on a shade.
/// Whether the picture was drawn.
/// `covered`: something is open over it, which a sixel or iTerm2 picture
/// would be drawn over, so it isn't.
pub(super) fn picture(
    frame: &mut Frame,
    app: &mut App,
    article: &Article,
    area: Rect,
    bounds: Rect,
    covered: bool,
) -> bool {
    let c = app.colors.clone();
    let shown = area.intersection(bounds);
    if shown.is_empty() {
        return false;
    }
    let whole = shown == area;
    // Kitty's and block pictures can be cut; sixel and iTerm2 ones can't.
    let cuttable = !app.images.paints_over();
    let key = Key {
        subject: Subject::Article(article.id.clone()),
        cols: area.width,
        rows: area.height,
    };
    // Asked for even when it can't be drawn now (covered), so it's ready
    // when it can.
    let drawn = article
        .image
        .as_deref()
        .is_some_and(|url| match app.images.get(&key, url) {
            Some(_) if covered || !(whole || cuttable) => false,
            Some(image) if whole => {
                frame.render_widget(Image::new(image), area);
                true
            }
            Some(image) => {
                frame.render_widget(Image::new(image).allow_clipping(true), shown);
                true
            }
            None => false,
        });
    if drawn {
        app.images.place(&key, area, shown, frame.buffer_mut());
        return true;
    }
    frame.render_widget(Block::new().style(Style::new().bg(c.selected)), shown);
    // The source's name in the middle, in its color; a mark while the
    // picture comes, or if it's broken.
    let name = article.source_name();
    let mark = if article.image.is_none() {
        name.clone()
    } else if app.images.is_broken(&key) {
        format!("{}  {name}", app.icons.picture)
    } else {
        name.clone()
    };
    let mark = fit(&mark, usize::from(area.width.saturating_sub(2)));
    let w = mark.width() as u16;
    let middle = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height / 2,
        width: w,
        height: 1,
    };
    if inside(middle, bounds) {
        frame.render_widget(
            Paragraph::new(Span::styled(
                mark,
                Style::new()
                    .fg(c.source(&name))
                    .add_modifier(Modifier::BOLD),
            )),
            middle,
        );
    }
    false
}

/// The line over a card's title: a dot while it's unread, its source and
/// age, and a mark if it's saved.
pub(super) fn byline(app: &App, c: &Colors, article: &Article, width: usize) -> Line<'static> {
    let unread = !app.is_read(&article.id);
    let mut spans = vec![if unread {
        Span::styled(format!("{} ", app.icons.dot), Style::new().fg(c.red))
    } else {
        Span::raw("")
    }];
    let saved = app.saved.contains(&article.id);
    let age = article::age(article.published, now());
    let source = article.source_name();
    let room = width
        .saturating_sub(spans.iter().map(Span::width).sum::<usize>())
        .saturating_sub(if saved { 2 } else { 0 });
    let both = format!("{source} · {age}");
    let text = if both.width() <= room {
        both
    } else {
        let short = format!(" · {}", article::short_age(article.published, now()));
        format!(
            "{}{short}",
            fit(&source, room.saturating_sub(short.width()))
        )
    };
    spans.push(Span::styled(fit(&text, room), Style::new().fg(c.subtext)));
    if saved {
        spans.push(Span::styled(
            format!(" {}", app.icons.saved),
            Style::new().fg(c.accent),
        ));
    }
    Line::from(spans)
}

/// A card in `slot`, of which only what's within `bounds` is drawn: the
/// last row may be cut off by the window's bottom.
#[allow(clippy::too_many_arguments)]
fn card(
    frame: &mut Frame,
    app: &mut App,
    c: &Colors,
    article: &Article,
    slot: Rect,
    bounds: Rect,
    g: &Geometry,
    selected: bool,
) {
    let focused = app.focus == Focus::Grid && app.prompt.is_none();
    if selected && inside(slot, bounds) {
        let ring = if focused { c.accent } else { c.border };
        frame.render_widget(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(ring)),
            slot,
        );
    }
    let inner = Rect {
        x: slot.x + 1,
        y: slot.y + 1,
        width: g.card_width,
        height: slot.height.saturating_sub(2),
    };
    let area = Rect {
        height: g.picture_rows,
        ..inner
    };
    if area.intersection(bounds).is_empty() {
        return;
    }
    let covered = app.covers_articles() && app.images.paints_over();
    picture(frame, app, article, area, bounds, covered);

    // Under the picture, the card's words on a shade of their own, so they
    // stand apart from the background.
    let words = Rect {
        y: area.y + area.height,
        height: (inner.y + inner.height).saturating_sub(area.y + area.height),
        ..inner
    }
    .intersection(bounds);
    if !words.is_empty() {
        frame.render_widget(Block::new().style(Style::new().bg(c.selected)), words);
    }
    let text_x = inner.x + PAD;
    let text_width = inner.width.saturating_sub(2 * PAD);
    let w = usize::from(text_width);
    let read = app.is_read(&article.id);
    let title_style = if selected && focused {
        Style::new().fg(c.accent).add_modifier(Modifier::BOLD)
    } else if read {
        Style::new().fg(c.subtext)
    } else {
        Style::new().fg(c.text).add_modifier(Modifier::BOLD)
    };
    let mut lines = vec![byline(app, c, article, w)];
    let mut title = wrap_lines(&article.title, w, usize::from(TITLE_ROWS));
    title.resize(usize::from(TITLE_ROWS), String::new());
    lines.extend(
        title
            .into_iter()
            .map(|l| Line::from(Span::styled(l, title_style))),
    );
    if app.settings.summaries {
        let summary = article::one_line(&article.summary, w * 3);
        lines.extend(
            wrap_lines(&summary, w, usize::from(SUMMARY_ROWS))
                .into_iter()
                .map(|l| Line::from(Span::styled(l, Style::new().fg(c.dim)))),
        );
    }
    let text_area = Rect {
        x: text_x,
        y: area.y + area.height + 1,
        width: text_width,
        height: (inner.y + inner.height).saturating_sub(area.y + area.height + 1),
    }
    .intersection(bounds);
    if !text_area.is_empty() {
        frame.render_widget(Paragraph::new(lines), text_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards_are_as_many_as_fit_and_pictures_are_16_by_9() {
        let font = FontSize {
            width: 8,
            height: 16,
        };
        let g = geometry(Rect::new(0, 0, 150, 60), 32, font, true);
        assert_eq!(g.cols, 4);
        assert_eq!(g.card_width, 35);
        // 35 columns of 8 px = 280 px wide, so 157.5 px tall: 10 rows of 16.
        assert_eq!(g.picture_rows, 10);
        assert_eq!(g.slot_height, 1 + 10 + 8 + 1);
        assert_eq!(g.rows, 3);
        let tiny = geometry(Rect::new(0, 0, 10, 5), 32, font, false);
        assert_eq!((tiny.cols, tiny.rows), (1, 1));
    }
}
