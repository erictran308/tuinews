//! The reader: an article on a page of its own, as a news site lays it out.
//! At the top its picture, and beside it (under it, in a narrow window) its
//! title, source, author and when it was published; then its summary as
//! the lead, and its text read from its page, in a column no wider than
//! `reader_width`. Scrolled down (`j`, Space), the head folds to a line of
//! the title, so the text has the page. A video in the text shows as its
//! poster with a line under it; inside Pepper it plays there, in a box in
//! the corner while it's scrolled out of view.

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui_image::sliced::{SignedPosition, SlicedImage};

use super::{fit, wrap};
use crate::app::{App, Body, Playing, now};
use crate::article::{self, Article, ArticleId};
use crate::html::{Block as Text, Video};
use crate::images::{Key, Subject};
use crate::theme::Colors;

/// Below this width the title goes under the picture, not beside it.
const BESIDE: u16 = 90;
/// Rows the folded head takes: the title, the source, a blank one.
const FOLDED: u16 = 3;

/// The reader in `area`.
pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    // What Enter plays is what this frame shows: none, should it show no
    // text (a window too small for it).
    if let Some(reader) = app.reader.as_mut() {
        reader.videos_shown.clear();
    }
    let c = app.colors.clone();
    let Some(reader) = app.reader.as_ref() else {
        return;
    };
    let article = reader.article.clone();
    let scroll = reader.scroll;
    // Under a popup, sixel and iTerm2 pictures aren't drawn: they'd be
    // drawn over it. They're still asked for, to be ready.
    let covered = app.popup_open() && app.images.paints_over();
    let line = super::border(true, &c);
    let mut title = vec![Span::styled("─ ", line)];
    title.extend(super::logo(app.icons.logo));
    title.push(Span::styled(
        format!(" {} ", fit(&article.source_name(), 40)),
        Style::new().fg(c.text).add_modifier(Modifier::BOLD),
    ));
    let title = Line::from(title);
    let link = Line::from(Span::styled(
        format!(" {} {} ", app.icons.link, fit(&article.host(), 40)),
        Style::new().fg(c.dim),
    ))
    .right_aligned();
    let block = super::bordered()
        .border_style(line)
        .title(title)
        .title(link)
        .style(Style::new().bg(c.panel));
    let inner = block.inner(area).inner(Margin::new(2, 1));
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    // The column the words go in, in the middle.
    let column_width = inner.width.min(app.settings.reader_width());
    let column = Rect {
        x: inner.x + (inner.width - column_width) / 2,
        width: column_width,
        ..inner
    };
    let w = usize::from(column.width);
    let laid = body_layout(app, &c, &reader.body, &article, w, column.height);

    // A video page's own picture is the poster of the video its text opens
    // with: shown there, not twice.
    let picture = !matches!(&laid.pictures.first(),
        Some(Placed { video: Some(Video { poster: Some(poster), .. }), .. })
            if article.image.as_deref().is_some_and(|image| same_picture(image, poster)));
    // At the top: the whole head. Scrolled: the head folded to a line.
    let head_height = if scroll == 0 {
        draw_head(frame, app, &c, &article, column, picture, covered)
    } else {
        draw_folded(frame, app, &c, &article, column)
    };
    let below = Rect {
        y: column.y + head_height,
        height: column.height.saturating_sub(head_height),
        ..column
    };
    // As far as it scrolls: the text's end in view under the folded head
    // (scrolled `n`, it shows from its line `n - 1`); nowhere, when it all
    // fits under the whole head.
    let page = usize::from(column.height.saturating_sub(FOLDED)).max(1);
    let fits = scroll == 0 && laid.lines.len() <= usize::from(below.height);
    let max = if fits {
        0
    } else {
        laid.lines.len().saturating_sub(page) + 1
    };
    if let Some(open) = app.reader.as_mut() {
        open.scroll = open.scroll.min(max);
        open.max_scroll.set(max);
        open.page.set(page);
    }
    let from = scroll.saturating_sub(1);
    let shown: Vec<Line> = laid
        .lines
        .iter()
        .skip(from)
        .take(usize::from(below.height))
        .cloned()
        .collect();
    frame.render_widget(Paragraph::new(shown), below);
    draw_pictures(frame, app, &c, &laid, below, from, covered);
    corner_video(frame, app, &c, &laid, below);
    if max > 0 {
        let mut state = ScrollbarState::new(max).position(scroll.min(max));
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .style(Style::new().fg(c.dim)),
            area.inner(Margin::new(0, 1)),
            &mut state,
        );
    }
}

/// The picture (with `picture`), and the title and facts beside it (or
/// under it, when the column's narrow). Rows taken, a blank one after
/// included.
fn draw_head(
    frame: &mut Frame,
    app: &mut App,
    c: &Colors,
    article: &Article,
    column: Rect,
    picture: bool,
    covered: bool,
) -> u16 {
    let font = app.images.font_size();
    let beside = column.width >= BESIDE;
    let wanted_width = if beside {
        column.width * 2 / 5
    } else {
        column.width
    };
    // 16:9 in pixels, at most half the page high (and narrower to keep it
    // 16:9 where it was capped).
    let (fw, fh) = (f32::from(font.width.max(1)), f32::from(font.height.max(1)));
    let shown = picture && article.image.is_some();
    let (picture_width, picture_rows) = if shown && column.height >= 12 {
        let rows = (f32::from(wanted_width) * fw * 9.0 / 16.0 / fh).round() as u16;
        let rows = rows.clamp(3, column.height / 2);
        let cols = (f32::from(rows) * fh * 16.0 / 9.0 / fw).round() as u16;
        (wanted_width.min(cols).max(1), rows)
    } else {
        (0, 0)
    };
    if picture_rows > 0 {
        let at = Rect {
            x: column.x,
            y: column.y,
            width: picture_width,
            height: picture_rows,
        };
        super::grid::picture(frame, app, article, at, column, covered);
    }
    let (text_area, used) = if beside && picture_rows > 0 {
        let gap = 3;
        let area = Rect {
            x: column.x + picture_width + gap,
            width: column.width.saturating_sub(picture_width + gap),
            height: column.height,
            ..column
        };
        (area, picture_rows)
    } else {
        let top = if picture_rows > 0 {
            picture_rows + 1
        } else {
            0
        };
        let area = Rect {
            y: column.y + top,
            height: column.height.saturating_sub(top),
            ..column
        };
        (area, top)
    };
    let lines = title_lines(app, c, article, usize::from(text_area.width));
    let rows = lines.len() as u16;
    frame.render_widget(Paragraph::new(lines), text_area);
    let taken = if beside && picture_rows > 0 {
        used.max(rows)
    } else {
        used + rows
    };
    (taken + 1).min(column.height)
}

/// The head, folded: the title on a line, the source and age under it.
fn draw_folded(frame: &mut Frame, app: &App, c: &Colors, article: &Article, column: Rect) -> u16 {
    let w = usize::from(column.width);
    let source = article.source_name();
    let lines = vec![
        Line::from(Span::styled(
            fit(&article.title, w),
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled(
                fit(&source, w / 2),
                Style::new()
                    .fg(c.source(&source))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {}", article::age(article.published, now())),
                Style::new().fg(c.subtext),
            ),
            Span::styled(
                if app.saved.contains(&article.id) {
                    format!("  {} Saved", app.icons.saved)
                } else {
                    String::new()
                },
                Style::new().fg(c.accent),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), column);
    FOLDED.min(column.height)
}

/// The title, big and bold, then the source, category, date and author.
fn title_lines(app: &App, c: &Colors, article: &Article, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = wrap(&article.title, width)
        .into_iter()
        .map(|l| {
            Line::from(Span::styled(
                l,
                Style::new().fg(c.text).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    lines.push(Line::default());
    let source = article.source_name();
    let mut facts = vec![Span::styled(
        fit(&source, width),
        Style::new()
            .fg(c.source(&source))
            .add_modifier(Modifier::BOLD),
    )];
    if !article.category.is_empty() {
        facts.push(Span::styled(
            fit(
                &format!(" · {}", article.category),
                width.saturating_sub(source.len()),
            ),
            Style::new().fg(c.subtext),
        ));
    }
    lines.push(Line::from(facts));
    let when = format!(
        "{} · {}",
        article::when(article.published),
        article::age(article.published, now())
    );
    lines.push(Line::from(Span::styled(
        fit(&when, width),
        Style::new().fg(c.subtext),
    )));
    if !article.author.is_empty() {
        lines.push(Line::from(Span::styled(
            fit(&article.author, width),
            Style::new().fg(c.subtext),
        )));
    }
    if app.saved.contains(&article.id) {
        lines.push(Line::from(Span::styled(
            format!("{} Saved", app.icons.saved),
            Style::new().fg(c.accent),
        )));
    }
    lines
}

/// A picture of the article's text, laid out: its first row among the
/// text's, how big it's drawn, and where it comes from (a video's poster:
/// none, when it has no poster); and the video it's the poster of.
struct Placed {
    row: usize,
    cols: u16,
    rows: u16,
    subject: Subject,
    url: String,
    video: Option<Video>,
}

/// The reader's text, laid out a row at a time: the lines, and the
/// pictures over the blank rows kept for them.
struct Laid {
    width: usize,
    lines: Vec<Line<'static>>,
    pictures: Vec<Placed>,
}

impl Laid {
    fn paragraph(&mut self, text: &str, style: Style) {
        for row in wrap(text, self.width) {
            self.lines.push(Line::from(Span::styled(row, style)));
        }
        self.lines.push(Line::default());
    }

    /// A picture across the column at its own shape (16:9 until it's known),
    /// never wider than it is, nor taller than `most` rows.
    fn picture(&mut self, app: &App, url: &str, most: u16) {
        self.place(app, url, most, None);
        self.lines.push(Line::default());
    }

    /// A video: its poster laid out as a picture (16:9 with none) but as
    /// wide as the column lets it, however small the poster, as the video is
    /// played there; and a line under it saying what Enter does, or how it
    /// plays.
    fn video(&mut self, app: &App, c: &Colors, video: &Video, most: u16) {
        let poster = video.poster.as_deref().unwrap_or_default();
        self.place(app, poster, most, Some(video.clone()));
        let playing = app.playing.as_ref().filter(|p| p.video == *video);
        let line = video_line(app, c, playing);
        self.lines.push(line.centered());
        self.lines.push(Line::default());
    }

    fn place(&mut self, app: &App, url: &str, most: u16, video: Option<Video>) {
        let subject = Subject::Inline(ArticleId::of(url));
        let font = app.images.font_size();
        let (fw, fh) = (f32::from(font.width.max(1)), f32::from(font.height.max(1)));
        let shape = app.images.shape(&subject);
        let (w, h) = shape.unwrap_or((16, 9));
        let aspect = w.max(1) as f32 / h.max(1) as f32;
        let mut cols = self.width as f32;
        // A picture no bigger than it is, but a video's box its own size.
        if shape.is_some() && video.is_none() {
            cols = cols.min(w as f32 * MAX_ENLARGED / fw);
        }
        let mut rows = cols * fw / aspect / fh;
        let most = f32::from(most.max(MIN_PICTURE_ROWS));
        if rows > most {
            rows = most;
            cols = rows * fh * aspect / fw;
        }
        let (cols, rows) = (
            // Four columns at least, but never wider than the column, even
            // one narrower than that.
            (cols.round() as u16).max(4).min(self.width as u16),
            (rows.round() as u16).max(MIN_PICTURE_ROWS),
        );
        self.pictures.push(Placed {
            row: self.lines.len(),
            cols,
            rows,
            subject,
            url: url.to_string(),
            video,
        });
        self.lines
            .extend(std::iter::repeat_n(Line::default(), usize::from(rows)));
    }
}

/// What the line under a video says: what Enter does; or, for the one
/// `playing`, where and how far it plays, and its keys.
fn video_line(app: &App, c: &Colors, playing: Option<&Playing>) -> Line<'static> {
    let icons = &app.icons;
    let Some(playing) = playing else {
        return Line::from(vec![
            Span::styled(format!("{} Video", icons.play), Style::new().fg(c.accent)),
            Span::styled(" · Enter plays it", Style::new().fg(c.dim)),
        ]);
    };
    if playing.inside() && !playing.started {
        return Line::from(Span::styled(
            format!("{} Loading the video…", icons.loading),
            Style::new().fg(c.accent),
        ));
    }
    let mark = if playing.paused {
        icons.pause
    } else {
        icons.play
    };
    let mut spans = vec![Span::styled(
        format!("{mark} {}", super::played(playing)),
        Style::new().fg(c.accent).add_modifier(Modifier::BOLD),
    )];
    let keys = match (playing.inside(), playing.player.controllable()) {
        (true, _) => " · Enter pauses · f fills the tab · x stops",
        (false, true) => " in mpv's window · Enter pauses · f full screen · x stops",
        (false, false) => " in mpv's window · x stops",
    };
    spans.push(Span::styled(keys, Style::new().fg(c.dim)));
    Line::from(spans)
}

/// How much bigger than its own size a picture is drawn, at most.
const MAX_ENLARGED: f32 = 1.5;
/// The fewest rows a picture takes.
const MIN_PICTURE_ROWS: u16 = 3;

/// The address of a picture without what follows its path: the same
/// picture at another size (`?w=680`, `?w=1200`) is the same.
fn same_picture(a: &str, b: &str) -> bool {
    let path = |url: &str| url.split(['?', '#']).next().unwrap_or_default().to_string();
    path(a) == path(b)
}

/// The lead (the feed's summary), then the article's text and pictures, or
/// why they aren't there, each paragraph wrapped to `width` with a blank
/// row after. Pictures are at most `page` rows high.
fn body_layout(
    app: &App,
    c: &Colors,
    body: &Body,
    article: &Article,
    width: usize,
    page: u16,
) -> Laid {
    let mut out = Laid {
        width,
        lines: Vec::new(),
        pictures: Vec::new(),
    };
    let blocks = match body {
        Body::Text(blocks) => blocks.as_slice(),
        _ => &[],
    };
    // The lead, unless the page's text starts with it.
    let summary = article.summary.trim();
    let first = blocks
        .iter()
        .find(|b| !matches!(b, Text::Picture(_) | Text::Video(_)))
        .map(Text::text)
        .unwrap_or_default();
    let opening: String = summary.chars().take(40).collect();
    if !summary.is_empty() && !first.starts_with(&opening) {
        let lead = Style::new().fg(c.text).add_modifier(Modifier::BOLD);
        for line in summary.lines().filter(|l| !l.is_empty()) {
            out.paragraph(line, lead);
        }
    }
    match body {
        Body::Summary => out.paragraph(
            "o opens the whole article in your browser.",
            Style::new().fg(c.dim),
        ),
        Body::Loading => out.paragraph(
            &format!(
                "{} Reading the article from {}…",
                app.icons.loading,
                article.host()
            ),
            Style::new().fg(c.accent),
        ),
        Body::Failed(why) => out.paragraph(
            &format!("Its text couldn't be read here: {why}. o opens it in your browser."),
            Style::new().fg(c.warning),
        ),
        Body::Text(blocks) => {
            for block in blocks {
                match block {
                    Text::Paragraph(t) => out.paragraph(t, Style::new().fg(c.text)),
                    Text::Heading(t) => {
                        out.paragraph(t, Style::new().fg(c.accent).add_modifier(Modifier::BOLD));
                    }
                    Text::Aside(t) => {
                        out.paragraph(t, Style::new().fg(c.subtext).add_modifier(Modifier::ITALIC))
                    }
                    // The article's own picture is at the top already.
                    Text::Picture(url)
                        if article
                            .image
                            .as_deref()
                            .is_some_and(|i| same_picture(i, url)) => {}
                    Text::Picture(url) => out.picture(app, url, page * 4 / 5),
                    Text::Video(video) => out.video(app, c, video, page * 4 / 5),
                }
            }
            out.paragraph(
                &format!("— {} · o opens it in your browser", article.host()),
                Style::new().fg(c.dim),
            );
        }
    }
    out
}

/// The pictures of the text that show in `area`, whose first row is the
/// text's row `from`: each where it falls, cut where the area cuts it (a
/// row at a time), or a shade until it's ready. A video's poster too: the
/// one playing inside Pepper plays over it while it's shown whole, and the
/// videos shown are noted, for Enter.
fn draw_pictures(
    frame: &mut Frame,
    app: &mut App,
    c: &Colors,
    laid: &Laid,
    area: Rect,
    from: usize,
    covered: bool,
) {
    let mut videos: Vec<(Video, bool)> = Vec::new();
    // Nothing to draw into (a window too short for the text under the head).
    if area.height == 0 {
        return;
    }
    for placed in &laid.pictures {
        let top = placed.row as i32 - from as i32;
        let bottom = top + i32::from(placed.rows);
        if bottom <= 0 || top >= i32::from(area.height) {
            continue;
        }
        let x = (area.width.saturating_sub(placed.cols)) / 2;
        let whole = Rect {
            x: area.x + x,
            y: (i32::from(area.y) + top).max(0) as u16,
            width: placed.cols,
            height: placed.rows,
        };
        let shown = Rect {
            y: (i32::from(area.y) + top.max(0)) as u16,
            height: (bottom.min(i32::from(area.height)) - top.max(0)) as u16,
            ..whole
        }
        .intersection(area);
        if let Some(video) = &placed.video {
            let in_view = top >= 0 && bottom <= i32::from(area.height);
            let plays_here = app
                .playing
                .as_ref()
                .is_some_and(|p| p.video == *video && p.inside() && !p.fullscreen);
            if plays_here && in_view {
                app.video_box.set(Some(whole));
            }
            videos.push((video.clone(), in_view));
        }
        let key = Key {
            subject: placed.subject.clone(),
            cols: placed.cols,
            rows: placed.rows,
        };
        let image = if placed.url.is_empty() {
            None
        } else {
            app.images.get_sliced(&key, &placed.url)
        };
        if !covered && let Some(image) = image {
            let at = SignedPosition {
                x: x as i16,
                y: top as i16,
            };
            frame.render_widget(SlicedImage::new(image, at), area);
            app.images.place(&placed.subject, whole, shown);
            continue;
        }
        frame.render_widget(Block::new().style(Style::new().bg(c.selected)), shown);
        let mark = if placed.video.is_some() {
            app.icons.play
        } else if app.images.is_broken(&key) {
            app.icons.failed
        } else {
            app.icons.picture
        };
        let middle = Rect {
            x: whole.x + whole.width / 2,
            y: area.y + ((top + bottom) / 2).clamp(0, i32::from(area.height) - 1) as u16,
            width: 1,
            height: 1,
        };
        if shown.contains(middle.as_position()) {
            frame.render_widget(
                Paragraph::new(Span::styled(mark, Style::new().fg(c.dim))),
                middle,
            );
        }
    }
    // Enter plays one shown whole before one cut by the page's edge.
    videos.sort_by_key(|(_, whole)| !whole);
    if let Some(reader) = app.reader.as_mut() {
        reader.videos_shown = videos.into_iter().map(|(video, _)| video).collect();
    }
}

/// The video playing inside Pepper, while its place in the text isn't all
/// in `area` (scrolled out, or partly): in a box in the corner, black with
/// what it waits for until its first frame.
fn corner_video(frame: &mut Frame, app: &mut App, c: &Colors, laid: &Laid, area: Rect) {
    let Some(playing) = app.playing.as_ref() else {
        return;
    };
    let here = laid
        .pictures
        .iter()
        .any(|p| p.video.as_ref() == Some(&playing.video));
    // Not where Pepper took the picture away (an older one, under a popup):
    // the black box would hide the text for nothing.
    if !playing.inside()
        || playing.fullscreen
        || !here
        || app.video_box.get().is_some()
        || !app.picture_shown()
    {
        return;
    }
    let font = app.images.font_size();
    let Some(corner) = crate::pepper::pip_area(area, (font.width, font.height)) else {
        return;
    };
    frame.render_widget(
        Block::new().style(Style::new().bg(crate::theme::BLACK)),
        corner,
    );
    if !playing.started {
        let line = video_line(app, c, Some(playing));
        let middle = Rect {
            y: corner.y + corner.height / 2,
            height: 1,
            ..corner
        };
        frame.render_widget(Paragraph::new(line.centered()), middle);
    }
    app.video_box.set(Some(corner));
}
