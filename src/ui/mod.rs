//! Drawing: the sidebar of your feeds beside a grid of article cards (or a
//! list), the reader, the status line, and the popups, laid out as
//! tuitube's.

pub mod effects;
mod grid;
mod list;
mod reader;
mod sidebar;
pub mod splash;

use std::num::NonZeroU16;
use std::time::Instant;

use ratatui::Frame;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Confirm, Focus, HelpMenu, HelpTab, Kept, Naming, PromptKind, Typed, View};
use crate::settings::Settings;
use crate::theme::{self, Colors};
use effects::Popup;

/// Below this width the sidebar and the articles take turns.
const NARROW: u16 = 70;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let now = Instant::now();
    // The splash screen leaves once what the frame before asked for is in.
    let ready = app.first_screen_in();
    if let Some(splash) = app.splash.as_mut()
        && !splash.advance(now, ready)
    {
        app.splash = None;
        app.images.hold(false);
    }
    app.images.new_frame();
    let area = frame.area();
    let c = app.colors.clone();
    frame.render_widget(Block::new().style(Style::new().bg(c.bg).fg(c.text)), area);
    let [body, status] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    // The search box is in the articles' border, so they show while it's
    // open; the reader opens over them.
    let narrow = body.width < NARROW;
    let side_width = if narrow {
        if app.focus == Focus::Sidebar && app.prompt.is_none() {
            body.width
        } else {
            0
        }
    } else {
        app.settings.sidebar_width().min(body.width / 2)
    };
    let [side, grid] =
        Layout::horizontal([Constraint::Length(side_width), Constraint::Fill(1)]).areas(body);
    if side.width > 0 {
        sidebar::draw(frame, app, side);
    }
    if grid.width > 0 {
        grid::draw(frame, app, grid);
    }
    status_bar(frame, app, status);
    // While the splash screen is up nothing moves under it: the fade at start
    // waits for its first frame without it.
    if app.splash.is_none() {
        effects::under(app, frame.buffer_mut(), now);
    }
    if app.reader.is_some() {
        let place = reader_place(body, app.settings.reader_width());
        clear_popup(frame, &app.effects, Popup::Reader, place);
        reader::draw(frame, app, place);
    }
    if app.help.is_some() {
        help(frame, app, area);
    }
    if app.category_menu.is_some() {
        category_menu(frame, app, area);
    }
    if app.confirm.is_some() {
        confirm(frame, app, area);
    }
    if let Some(splash) = &app.splash {
        // Over all of it: what the frame drew has asked for its pictures.
        app.effects.popups.get_mut().clear();
        splash::draw(frame.buffer_mut(), area, &c, splash, now);
    } else {
        // The status bar stays readable over the dimming: it says what a
        // popup's keys did.
        effects::over(app, frame.buffer_mut(), Some(status));
    }
    pin_emoji_widths(frame.buffer_mut());
}

/// Where the reader opens: over the articles, in the middle, as wide as
/// its text and its margins (`reader_width`), a row of the screen showing
/// above and below it; all of `body` in a small window.
fn reader_place(body: Rect, reader_width: u16) -> Rect {
    if body.width < NARROW || body.height < 16 {
        return body;
    }
    let width = (reader_width + 12).min(body.width.saturating_sub(8));
    let height = body.height.saturating_sub(2);
    Rect {
        x: body.x + (body.width - width) / 2,
        y: body.y + 1,
        width,
        height,
    }
}

/// Clears where a popup goes, noting it for the animations (`effects`):
/// the screen dims behind it, and it opens out.
fn clear_popup(frame: &mut Frame, effects: &effects::Effects, popup: Popup, place: Rect) {
    frame.render_widget(Clear, place);
    effects.popups.borrow_mut().push((popup, place));
}

/// Marks each emoji with U+FE0F (⚔️, ❤️) as two columns wide when it's
/// sent to the terminal. Otherwise ratatui also sends the column after it,
/// without moving the cursor there first: in a terminal that drew the emoji
/// two columns wide (Ghostty, kitty, WezTerm), that column lands one to the
/// right, and the rest of the row after it.
fn pin_emoji_widths(buffer: &mut Buffer) {
    for cell in &mut buffer.content {
        if cell.diff_option == CellDiffOption::None
            && cell.symbol().contains('\u{FE0F}')
            && let Some(width) = NonZeroU16::new(cell.symbol().width() as u16)
            && width.get() > 1
        {
            cell.diff_option = CellDiffOption::ForcedWidth(width);
        }
    }
}

/// How wide the question's text goes before it takes another line.
const MAX_CONFIRM: usize = 60;

/// The question waiting for `y`, in a popup in the middle: what it does,
/// and the keys.
fn confirm(frame: &mut Frame, app: &App, area: Rect) {
    let c = &app.colors;
    let Some(confirm) = &app.confirm else {
        return;
    };
    let (title, text, hint) = match confirm {
        Confirm::RemoveFeed(_, name) => (
            " Remove this feed? ".to_string(),
            format!(
                "“{name}” and its articles go, from its categories too, but those you saved or opened. a adds a feed again."
            ),
            " `y` remove · any other key keeps it ",
        ),
        Confirm::DeleteCategory(_, name) => (
            " Delete this category? ".to_string(),
            format!(
                "“{name}” goes, with the sites' sections only it had. Your feeds stay, and what you saved or opened."
            ),
            " `y` delete · any other key keeps it ",
        ),
        Confirm::TakeOff(list, _, title) => {
            let from = match list {
                Kept::Saved => "Saved",
                Kept::History => "History",
            };
            (
                format!(" Take it off {from}? "),
                format!("“{title}”"),
                " `y` take it off · any other key keeps it ",
            )
        }
        Confirm::MarkAllRead(ids) => {
            let n = ids.len();
            let articles = if n == 1 { "article" } else { "articles" };
            (
                " Mark all read? ".to_string(),
                format!(
                    "The {n} unread {articles} shown here are marked read. m on one marks it unread again."
                ),
                " `y` mark them read · any other key doesn't ",
            )
        }
    };
    let hint_width = hint.replace('`', "").width();
    let fits = (MAX_CONFIRM.max(hint_width).max(title.width()) + 6).min(usize::from(area.width));
    // A long title goes on more lines, not across the screen.
    let lines = wrap(&text, fits.saturating_sub(4).max(1));
    let widest = lines.iter().map(|l| l.width()).max().unwrap_or(0);
    let width = (widest.max(hint_width).max(title.width()) + 6).min(usize::from(area.width));
    let height = (lines.len() as u16 + 4).min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width as u16) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width as u16,
        height,
    };
    clear_popup(frame, &app.effects, Popup::Question, popup);
    let block = bordered()
        .border_style(border(true, c))
        .title(Span::styled(
            title,
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(hint_spans(hint, c)).right_aligned())
        .style(Style::new().bg(c.panel));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = std::iter::once(Line::default()).chain(lines.iter().map(|line| {
        Line::from(Span::styled(
            fit(&format!(" {line}"), usize::from(inner.width)),
            Style::new().fg(c.text),
        ))
    }));
    frame.render_widget(Paragraph::new(rows.collect::<Vec<_>>()), inner);
}

/// A box with a round border all round, as tuigram's panes.
fn bordered<'a>() -> Block<'a> {
    Block::bordered().border_type(BorderType::Rounded)
}

/// The border of what has the keys is in the accent color.
fn border(focused: bool, c: &Colors) -> Style {
    Style::new().fg(if focused { c.accent } else { c.border })
}

/// The logo: its icon (a feed's) in white on `theme::LOGO`, the ends half a
/// cell wide, so it's two cells wide: about square.
fn logo(icon: &'static str) -> [Span<'static>; 3] {
    let edge = Style::new().fg(theme::LOGO);
    let mark = Style::new()
        .fg(theme::WHITE)
        .bg(theme::LOGO)
        .add_modifier(Modifier::BOLD);
    [
        Span::styled("▐", edge),
        Span::styled(icon, mark),
        Span::styled("▌", edge),
    ]
}

/// Key hints with the keys in backticks, like "`Enter` read · `o` browser",
/// as tuigram writes them: the keys stand out in the accent color, and
/// what they do is dim.
fn hint_spans(hints: &str, c: &Colors) -> Vec<Span<'static>> {
    let key = Style::new().fg(c.accent).add_modifier(Modifier::BOLD);
    let text = Style::new().fg(c.dim);
    hints
        .split('`')
        .enumerate()
        .filter(|(_, part)| !part.is_empty())
        .map(|(i, part)| Span::styled(part.to_string(), if i % 2 == 1 { key } else { text }))
        .collect()
}

/// As many of `hints` (separated by " · ") as fit in `width`, whole, as
/// `hint_spans`; cut with `…` if not even the first fits.
fn hints_fitting(hints: &str, width: usize, c: &Colors) -> Vec<Span<'static>> {
    let shown = |items: &[&str]| items.join(" · ").replace('`', "").width();
    let mut items: Vec<&str> = hints.split(" · ").collect();
    while items.len() > 1 && shown(&items) > width {
        items.pop();
    }
    if shown(&items) > width {
        let plain = items.join(" · ").replace('`', "");
        return vec![Span::styled(fit(&plain, width), Style::new().fg(c.dim))];
    }
    hint_spans(&items.join(" · "), c)
}

/// `text` cut to `width` columns, with `…` if it was longer. Measured, as
/// everywhere, a character as the terminal draws it (a grapheme, as ratatui
/// does): ⚔️ is two columns, though ⚔ alone is one. None is ever cut in two.
pub fn fit(text: &str, width: usize) -> String {
    if cells(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", head(text, width - 1))
}

/// The columns `text` takes, a grapheme at a time, as [`fit`] measures.
fn cells(text: &str) -> usize {
    text.graphemes(true).map(UnicodeWidthStr::width).sum()
}

/// The end of `text` that fits in `width` columns, found in one pass from
/// the end.
fn tail(text: &str, width: usize) -> &str {
    let mut used = 0;
    let mut start = text.len();
    for (i, grapheme) in text.grapheme_indices(true).rev() {
        let w = grapheme.width();
        if used + w > width {
            break;
        }
        used += w;
        start = i;
    }
    &text[start..]
}

/// The start of `text` that fits in `width` columns.
fn head(text: &str, width: usize) -> &str {
    let mut used = 0;
    for (i, grapheme) in text.grapheme_indices(true) {
        used += grapheme.width();
        if used > width {
            return &text[..i];
        }
    }
    text
}

/// What's typed in a box, in `room` columns and one more, the cursor a
/// block on the letter it's before (on a space at the end). The cursor
/// stays in view, with what's after it as far as there's room (half of it,
/// when what's before needs more).
fn with_cursor(typed: &Typed, room: usize, c: &Colors) -> Vec<Span<'static>> {
    let (before, after) = typed.text().split_at(typed.at());
    let (under, after) = match after.graphemes(true).next() {
        Some(letter) => (letter, &after[letter.len()..]),
        None => (" ", ""),
    };
    let room = (room + 1).saturating_sub(cells(under));
    let before = tail(before, room.saturating_sub(cells(after).min(room / 2)));
    let after = head(after, room.saturating_sub(cells(before)));
    vec![
        Span::styled(before.to_string(), Style::new().fg(c.text)),
        Span::styled(under.to_string(), Style::new().fg(c.panel).bg(c.accent)),
        Span::styled(after.to_string(), Style::new().fg(c.text)),
    ]
}

/// `text` in rows at most `width` columns wide, broken between words (a
/// word wider than a row is broken where it must be).
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in text.split(' ') {
        let gap = usize::from(!row.is_empty());
        if row.width() + gap + word.width() <= width {
            if gap == 1 {
                row.push(' ');
            }
            row.push_str(word);
            continue;
        }
        if !row.is_empty() {
            rows.push(std::mem::take(&mut row));
        }
        for grapheme in word.graphemes(true) {
            // A character wider than a row takes one of its own.
            if !row.is_empty() && row.width() + grapheme.width() > width {
                rows.push(std::mem::take(&mut row));
            }
            row.push_str(grapheme);
        }
    }
    rows.push(row);
    rows
}

/// `text` on at most `lines` rows of `width`, the last cut with `…` if
/// there's more.
pub(super) fn wrap_lines(text: &str, width: usize, lines: usize) -> Vec<String> {
    if width == 0 || lines == 0 {
        return Vec::new();
    }
    let wrapped = wrap(text, width);
    let mut rows: Vec<String> = wrapped.iter().take(lines).cloned().collect();
    if wrapped.len() > lines
        && let Some(last) = rows.last_mut()
    {
        let rest: Vec<&str> = wrapped[lines - 1..].iter().map(String::as_str).collect();
        *last = fit(&format!("{}…", rest.join(" ")), width);
        if !last.ends_with('…') {
            *last = fit(&format!("{last}…"), width);
        }
    }
    rows
}

/// Narrower than this, the search box shows only its icon and a word.
const MIN_SEARCH: usize = 12;

/// The search box, as the title of the articles' block (`width` columns
/// wide, `right` of them taken by its title on the right): a field from
/// the pictures' left edge, with what's typed or a hint and `/` at its end.
/// `line` is the border's style, which leads into the field.
pub(super) fn search_box(app: &App, width: u16, right: u16, line: Style) -> Line<'static> {
    let c = &app.colors;
    let lead = "─ ";
    // The corners, the lead, a space after the field and one of border
    // before the title on the right.
    let taken = 2 + lead.width() + 1 + if right > 0 { usize::from(right) + 1 } else { 0 };
    let box_width = usize::from(width).saturating_sub(taken).min(70);
    let bg = if app.prompt.is_some() {
        c.selected
    } else {
        c.panel
    };
    let inner = box_width.saturating_sub(4);
    let mut spans = match &app.prompt {
        Some(prompt) => {
            let (icon, label) = match prompt.kind {
                PromptKind::Search => (app.icons.search, ""),
                PromptKind::AddFeed => (app.icons.link, "Add a feed: "),
            };
            // The cursor stays in view.
            let room = inner.saturating_sub(label.width() + 1);
            let mut spans = vec![
                Span::styled(format!(" {icon} "), Style::new().fg(c.accent)),
                Span::styled(label, Style::new().fg(c.dim)),
            ];
            spans.extend(with_cursor(&prompt.typed, room, c));
            spans
        }
        // What was left typed in it, else what it's for.
        None => {
            let typed = app
                .search_typed
                .as_ref()
                .map(Typed::text)
                .filter(|t| !t.is_empty());
            let room = inner.saturating_sub(4);
            let shown = typed.map_or_else(
                || fit("Search your articles", room),
                |t| tail(t, room).to_string(),
            );
            vec![
                Span::styled(format!(" {} ", app.icons.search), Style::new().fg(c.dim)),
                Span::styled(shown, Style::new().fg(c.dim)),
            ]
        }
    };
    let hint = app.prompt.is_none() && box_width > MIN_SEARCH;
    let used: usize = spans.iter().map(|s| cells(&s.content)).sum();
    let pad = box_width.saturating_sub(used + if hint { 3 } else { 0 });
    spans.push(Span::raw(" ".repeat(pad)));
    // The box's colour, under all but the cursor's own.
    let mut spans: Vec<Span> = spans
        .into_iter()
        .map(|s| match s.style.bg {
            Some(_) => s,
            None => s.patch_style(Style::new().bg(bg)),
        })
        .collect();
    if hint {
        spans.push(Span::styled(" / ", Style::new().fg(c.dim).bg(c.selected)));
    }
    spans.insert(0, Span::styled(lead, line));
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// A message, or the keys for where you are; your feeds and what's unread
/// on the right.
fn status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let c = &app.colors;
    let width = area.width as usize;
    let feeds = app.listed().count();
    let right = format!(
        " {feeds} feed{} · {} unread ",
        if feeds == 1 { "" } else { "s" },
        app.home_unread
    );
    let room = width.saturating_sub(right.width());
    let left = match (&app.status, &app.prompt) {
        (_, Some(prompt)) => {
            let keys = match prompt.kind {
                PromptKind::Search => {
                    " `Enter` search · `Ctrl-j` articles · `Esc` cancel · `Ctrl-u` clear · without accents finds them too · `←→` move"
                }
                PromptKind::AddFeed => {
                    " Type or paste a feed's address, or a site's · `Enter` add · `Esc` cancel · `Ctrl-u` clear · `←→` move"
                }
            };
            hints_fitting(keys, room, c)
        }
        (Some(status), None) => {
            let color = if status.error { c.red } else { c.ok };
            vec![Span::styled(
                fit(&format!(" {}", status.text), room),
                Style::new().fg(color),
            )]
        }
        (None, None) if app.reader.is_some() => hints_fitting(
            " `Esc` back · `j k` scroll · `Space` page · `]` `[` next, before · `o` browser · `w` save · `m` unread · `y` copy link · `?` help",
            room,
            c,
        ),
        (None, None) => {
            let keys = match app.focus {
                Focus::Grid if app.view.keeps_all() => {
                    " `Enter` read · `o` browser · `x` take off · `w` save · `y` copy link · `F` categories · `v` list or cards · `/` search · `?` help"
                }
                Focus::Grid => {
                    " `Enter` read · `o` browser · `w` save · `m` read or unread · `M` all read · `u` only unread · `F` categories · `v` list or cards · `/` search · `?` help"
                }
                Focus::Sidebar => {
                    " `↑↓` move · `Enter` open · `Ctrl-l` articles · `F` categories · `a` add a feed · `x` remove · `R` refresh · `/` search · `?` help"
                }
            };
            hints_fitting(keys, room, c)
        }
    };
    let used: usize = left.iter().map(Span::width).sum();
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(room.saturating_sub(used))));
    spans.push(Span::styled(right, Style::new().fg(c.dim)));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(c.bg)),
        area,
    );
}

const HELP: [(&str, &str); 33] = [
    ("←↓↑→  h j k l", "move, in the pane that has the keys"),
    (
        "Ctrl + ←↓↑→  h j k l",
        "to the pane that way: sidebar, search box, articles",
    ),
    ("gg  G  Home  End", "first, last"),
    ("PgUp PgDn  Ctrl-u Ctrl-d", "a page, half a page"),
    ("Tab", "sidebar ↔ articles"),
    ("Enter", "read the article here, its text from its page"),
    ("o", "open it in your browser"),
    ("y", "copy its link"),
    ("w", "save it for later (again to take it off Saved)"),
    ("m  /  M", "read or unread  /  mark all shown read"),
    ("u", "only unread articles, or all"),
    (
        "x",
        "take off Saved or History; in the sidebar, remove the feed or category",
    ),
    (
        "F",
        "categories: put the feed in some or take it out, make, rename or delete one",
    ),
    ("v", "a list or cards"),
    (
        "/  s",
        "search your articles (accents optional: ha noi finds Hà Nội)",
    ),
    (
        "a",
        "add a feed: its address, or a site's; in a category, into it",
    ),
    ("R", "fetch every feed now"),
    ("Backspace  Ctrl-o", "back"),
    ("", ""),
    ("In the reader", ""),
    ("j k  Space  b", "scroll a line, a page down, a page up"),
    ("]  [", "the next article, the one before"),
    ("o  Enter", "open it in your browser"),
    ("w  m  y", "save, unread, copy its link"),
    ("Esc  q  Backspace", "back to the articles"),
    ("", ""),
    ("In a box you type in", ""),
    ("←→  Option ←→", "move a letter, a word"),
    ("Home End  Cmd ←→", "to either end"),
    ("Backspace  Delete", "delete the letter before, after"),
    ("Ctrl-w  Ctrl-u", "delete the word before, all before"),
    ("", ""),
    (
        "?  /  O  /  q",
        "these keys  /  settings: theme, list, unread only…  /  quit",
    ),
];

/// The `?` popup, centered: a tab with every key, and one with the
/// settings. Both are as big as the bigger one, so the tabs don't move.
fn help(frame: &mut Frame, app: &mut App, area: Rect) {
    let c = app.colors.clone();
    let Some(menu) = app.help.as_mut() else {
        return;
    };
    let keys = key_lines(&c);
    let (settings, cursor) = settings_lines(&app.settings, menu.selected, &c);
    let widest = |lines: &[Line]| lines.iter().map(Line::width).max().unwrap_or(0);
    let width = (widest(&keys).max(widest(&settings)) + 3).min(area.width as usize) as u16;
    let height = (keys.len().max(settings.len()) as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let tab = |label: &'static str, active: bool| {
        if active {
            Span::styled(
                label,
                Style::new()
                    .fg(c.bg)
                    .bg(c.accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(label, Style::new().fg(c.dim))
        }
    };
    let tabs = Line::from(vec![
        Span::raw(" "),
        tab(" Keys ", menu.tab == HelpTab::Keys),
        Span::raw(" "),
        tab(" Settings ", menu.tab == HelpTab::Settings),
        Span::raw(" "),
    ]);
    let hint = match menu.tab {
        HelpTab::Keys => " `j k` scroll · `Tab` settings · `Esc` close ",
        HelpTab::Settings if HelpMenu::is_choice(menu.selected) => {
            " `Enter` use · `Tab` keys · `Esc` close "
        }
        HelpTab::Settings => " `Enter` on/off · `Tab` keys · `Esc` close ",
    };
    clear_popup(frame, &app.effects, Popup::Keys, popup);
    let block = bordered()
        .border_style(border(true, &c))
        .title(tabs)
        .title_bottom(Line::from(hint_spans(hint, &c)).right_aligned())
        .style(Style::new().bg(c.panel));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = usize::from(inner.height);
    let (lines, scroll) = match menu.tab {
        HelpTab::Keys => (keys, &mut menu.scroll),
        HelpTab::Settings => {
            // Scrolled as little as keeps the cursor in view, with the line
            // above it: the heading, for the first row of a group. On the
            // last theme, the end shows.
            let scroll = &mut menu.settings_scroll;
            *scroll = (*scroll).min(cursor.saturating_sub(1));
            let cursor_end = if menu.selected == HelpMenu::LAST {
                settings.len() - 1
            } else {
                cursor
            };
            if cursor_end >= *scroll + rows {
                *scroll = cursor_end + 1 - rows;
            }
            (settings, scroll)
        }
    };
    let max = lines.len().saturating_sub(rows);
    *scroll = (*scroll).min(max);
    frame.render_widget(Paragraph::new(lines).scroll((*scroll as u16, 0)), inner);
    if max > 0 {
        let mut state = ScrollbarState::new(max).position(*scroll);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .style(Style::new().fg(c.dim)),
            popup.inner(Margin::new(0, 1)),
            &mut state,
        );
    }
}

fn key_lines(c: &Colors) -> Vec<Line<'static>> {
    let key_width = HELP.iter().map(|(k, _)| k.width()).max().unwrap_or(0);
    HELP.iter()
        .map(|(key, what)| {
            if what.is_empty() {
                return Line::from(Span::styled(
                    format!(" {key}"),
                    Style::new().fg(c.dim).add_modifier(Modifier::BOLD),
                ));
            }
            Line::from(vec![
                Span::styled(format!(" {key:<key_width$}  "), Style::new().fg(c.accent)),
                Span::styled(*what, Style::new().fg(c.text)),
            ])
        })
        .collect()
}

/// The settings tab, and the line of the row `selected`.
fn settings_lines(settings: &Settings, selected: usize, c: &Colors) -> (Vec<Line<'static>>, usize) {
    let mut lines = Vec::new();
    let mut cursor = 0;
    let mut add = |lines: &mut Vec<Line<'static>>, row: usize, mark: &'static str, label: &str| {
        let chosen = row == selected;
        let bar = if chosen {
            Span::styled("▌", Style::new().fg(c.accent))
        } else {
            Span::raw(" ")
        };
        let line = Line::from(vec![
            bar,
            Span::styled(mark, Style::new().fg(c.accent)),
            Span::styled(label.to_string(), Style::new().fg(c.text)),
        ]);
        if chosen {
            cursor = lines.len();
            lines.push(line.style(Style::new().bg(c.selected)));
        } else {
            lines.push(line);
        }
    };
    let heading = |text: &'static str| {
        Line::from(Span::styled(
            text,
            Style::new().fg(c.dim).add_modifier(Modifier::BOLD),
        ))
    };
    let check = |on: bool| if on { " [✓] " } else { " [ ] " };

    lines.push(heading(" Articles"));
    add(
        &mut lines,
        HelpMenu::SUMMARIES,
        check(settings.summaries),
        "Show the start of each summary on its card",
    );
    add(
        &mut lines,
        HelpMenu::LIST,
        check(settings.list),
        "Show articles as a list, without pictures",
    );
    add(
        &mut lines,
        HelpMenu::UNREAD_ONLY,
        check(settings.unread_only),
        "Show only the articles you haven't read",
    );
    add(
        &mut lines,
        HelpMenu::FULL_TEXT,
        check(settings.full_text),
        "Read the whole article here, from its page (else its summary)",
    );
    // Not a row: blocked words are written in settings.toml.
    let blocked = match settings.blocked() {
        0 => "No words blocked".to_string(),
        1 => "Blocked: 1 word".to_string(),
        n => format!("Blocked: {n} words"),
    };
    lines.push(Line::styled(
        format!("     {blocked} (blocked_words in settings.toml)"),
        Style::new().fg(c.dim),
    ));
    lines.push(Line::default());
    lines.push(heading(" Privacy"));
    add(
        &mut lines,
        HelpMenu::HISTORY,
        check(settings.history),
        "Keep a history of the articles you open, here only",
    );
    lines.push(Line::default());
    lines.push(heading(" Look"));
    add(
        &mut lines,
        HelpMenu::ANIMATIONS,
        check(settings.animations),
        effects::ABOUT,
    );
    lines.push(Line::default());
    lines.push(heading(" Theme"));
    let dot = |on: bool| if on { " ● " } else { " ○ " };
    for (i, (id, name)) in theme::names().iter().enumerate() {
        add(
            &mut lines,
            HelpMenu::THEMES + i,
            dot(*id == settings.theme),
            name,
        );
    }
    (lines, cursor)
}

/// What a view says above its articles: its name, how many, how many
/// unread, and how fresh the feeds are.
fn header_spans(app: &App, width: usize) -> Vec<Span<'static>> {
    let c = &app.colors;
    let count = match app.articles.len() {
        0 => String::new(),
        1 => "  1 article".into(),
        n => format!("  {n} articles"),
    };
    let mut spans = vec![
        Span::styled(
            // Two columns in: over the pictures' left edge, and in line
            // with the search box.
            format!("  {}", fit(&app.view.title(), width / 2)),
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(count, Style::new().fg(c.dim)),
    ];
    if !app.view.keeps_all() {
        let unread = app.articles.iter().filter(|a| !app.is_read(&a.id)).count();
        if unread > 0 && !app.settings.unread_only {
            spans.push(Span::styled(
                format!(" · {unread} unread"),
                Style::new().fg(c.dim),
            ));
        }
        if app.settings.unread_only {
            spans.push(Span::styled(
                "  ·  only unread (u)",
                Style::new().fg(c.accent),
            ));
        }
    }
    if let View::Category(id, _) = &app.view
        && let Some(category) = app.categories.iter().find(|c| c.id == *id)
    {
        let n = category.feeds.len();
        spans.push(Span::styled(
            format!("  ·  from {n} feed{}", if n == 1 { "" } else { "s" }),
            Style::new().fg(c.dim),
        ));
    }
    if matches!(app.view, View::Home | View::Feed(..) | View::Category(..)) {
        let freshness = match (app.refreshing, app.feeds_updated) {
            (Some((done, total)), _) => Some((format!("  ·  updating {done}/{total}"), c.accent)),
            (None, Some(at)) => Some((
                format!(
                    "  ·  updated {}",
                    crate::article::age(at, crate::app::now())
                ),
                c.dim,
            )),
            (None, None) => None,
        };
        if let Some((text, color)) = freshness {
            spans.push(Span::styled(text, Style::new().fg(color)));
        }
    }
    if let View::Feed(id, _) = &app.view
        && let Some(error) = app
            .feeds
            .iter()
            .find(|f| f.id == *id)
            .and_then(|f| f.error.clone())
    {
        spans.push(Span::styled(
            format!("  ·  couldn't fetch: {error}"),
            Style::new().fg(c.warning),
        ));
    }
    if let Some(adding) = &app.adding {
        spans.push(Span::styled(
            format!("  ·  {} looking for a feed at {adding}", app.icons.loading),
            Style::new().fg(c.accent),
        ));
    }
    spans
}

/// The longest a category's row goes before its name is cut.
const CATEGORY_WIDTH: usize = 48;

/// The categories popup (`F`): the feed's name and address, then each
/// category (ticked when the feed is in it, with how many feeds it has),
/// then "New category"; a name being typed in its row.
fn category_menu(frame: &mut Frame, app: &mut App, area: Rect) {
    let c = app.colors.clone();
    let Some(menu) = app.category_menu.clone() else {
        return;
    };
    let mut lines: Vec<Line> = Vec::new();
    match &menu.feed {
        Some((_, name, url)) => {
            lines.push(Line::from(Span::styled(
                format!("  {}", fit(name, CATEGORY_WIDTH)),
                Style::new().fg(c.text).add_modifier(Modifier::BOLD),
            )));
            let shown = url
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            lines.push(Line::from(Span::styled(
                format!("  {}", fit(shown, CATEGORY_WIDTH)),
                Style::new().fg(c.dim),
            )));
        }
        None => lines.push(Line::from(Span::styled(
            "  Your categories",
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        ))),
    }
    lines.push(Line::default());
    let first_row = lines.len();
    let field = |naming: &Naming| with_cursor(&naming.typed, CATEGORY_WIDTH, &c);
    let selected = menu.selected.min(app.categories.len());
    // The counts in a column after the longest name.
    let names = app
        .categories
        .iter()
        .map(|c| fit(&c.name, CATEGORY_WIDTH - 12).width())
        .max()
        .unwrap_or(0);
    let row = |lines: &mut Vec<Line<'static>>, i: usize, mut spans: Vec<Span<'static>>| {
        let chosen = i == selected;
        spans.insert(
            0,
            if chosen {
                Span::styled("▌", Style::new().fg(c.accent))
            } else {
                Span::raw(" ")
            },
        );
        let line = Line::from(spans);
        lines.push(if chosen {
            line.style(Style::new().bg(c.selected))
        } else {
            line
        });
    };
    for (i, category) in app.categories.iter().enumerate() {
        let mark = match &menu.feed {
            Some((feed, ..)) if category.feeds.contains(feed) => " [✓] ",
            Some(_) => " [ ] ",
            None => "  •  ",
        };
        let mut spans = vec![Span::styled(mark, Style::new().fg(c.accent))];
        let renaming = menu
            .naming
            .as_ref()
            .filter(|n| n.renaming == Some(category.id));
        match renaming {
            Some(naming) => spans.extend(field(naming)),
            None => {
                let n = category.feeds.len();
                let name = fit(&category.name, CATEGORY_WIDTH - 12);
                let pad = " ".repeat(names.saturating_sub(name.width()));
                spans.push(Span::styled(
                    format!("{name}{pad}"),
                    Style::new().fg(c.text),
                ));
                spans.push(Span::styled(
                    format!("  {n} feed{}", if n == 1 { "" } else { "s" }),
                    Style::new().fg(c.dim),
                ));
            }
        }
        row(&mut lines, i, spans);
    }
    let mut new = vec![Span::styled(" +   ", Style::new().fg(c.accent))];
    match menu.naming.as_ref().filter(|n| n.renaming.is_none()) {
        Some(naming) => new.extend(field(naming)),
        None => new.push(Span::styled("New category", Style::new().fg(c.subtext))),
    }
    row(&mut lines, app.categories.len(), new);

    let hint = match (&menu.naming, &menu.feed) {
        (Some(_), _) => " `Enter` save · `Esc` cancel · `←→` move ",
        (None, Some(_)) => " `Enter` in or out · `n` new · `r` rename · `x` delete · `Esc` close ",
        (None, None) => " `Enter` open · `n` new · `r` rename · `x` delete · `Esc` close ",
    };
    let hint_width = hint.replace('`', "").width();
    let widest = lines.iter().map(Line::width).max().unwrap_or(0);
    let width =
        (widest.max(hint_width).max(CATEGORY_WIDTH) + 4).min(usize::from(area.width)) as u16;
    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    clear_popup(frame, &app.effects, Popup::Categories, popup);
    let block = bordered()
        .border_style(border(true, &c))
        .title(Span::styled(
            " Categories ",
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(hint_spans(hint, &c)).right_aligned())
        .style(Style::new().bg(c.panel));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    // The cursor's row stays in view.
    let rows = usize::from(inner.height);
    let cursor = first_row + selected;
    let scroll = (cursor + 1).saturating_sub(rows);
    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::app;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_sidebar_lists_the_feeds_beside_the_cards() {
        let mut app = app();
        let shown = screen(&mut app, 140, 40);
        let logo = format!("─ ▐{}▌ tuinews ─", app.icons.logo);
        assert!(shown.contains(&logo), "the logo: {shown}");
        assert!(shown.contains("Home"));
        assert!(shown.contains("VnExpress"));
        assert!(shown.contains("Tuổi Trẻ"));
        assert!(shown.contains("Feeds (15)"));
        assert!(shown.contains("Categories (8)"), "{shown}");
        assert!(shown.contains("Tech"));
        assert!(shown.contains("Cars"));
        assert!(shown.contains("Tin số 3"), "the newest card: {shown}");
        assert!(shown.contains("3 articles"));
        assert!(shown.contains("15 feeds · 3 unread"));
    }

    #[test]
    fn a_list_shows_every_article_on_its_rows() {
        let mut app = app();
        app.settings.list = true;
        let shown = screen(&mut app, 120, 30);
        for n in 1..=3 {
            assert!(shown.contains(&format!("Tin số {n}")), "{shown}");
        }
        assert!(shown.contains("VnExpress ·"), "{shown}");
    }

    #[test]
    fn the_reader_shows_the_article_and_its_keys() {
        let mut app = app();
        app.settings.full_text = false;
        app.focus = Focus::Grid;
        app.on_key(key(KeyCode::Enter));
        let shown = screen(&mut app, 120, 30);
        assert!(shown.contains("Tin số 3"), "{shown}");
        assert!(shown.contains("Tóm tắt của tin số 3."));
        assert!(shown.contains("vnexpress.net"));
        assert!(shown.contains("Esc back"));
    }

    #[test]
    fn the_reader_opens_over_the_articles_with_its_pictures_in_place() {
        let mut app = app();
        app.settings.full_text = false;
        app.focus = Focus::Grid;
        app.on_key(key(KeyCode::Enter));
        let reader = app.reader.as_mut().unwrap();
        reader.body = crate::app::Body::Text(vec![
            crate::html::Block::Paragraph("Đoạn đầu.".into()),
            crate::html::Block::Picture("https://i1-vnexpress.vnecdn.net/trong-bai.jpg".into()),
            crate::html::Block::Aside("Chú thích ảnh".into()),
        ]);
        let shown = screen(&mut app, 150, 70);
        // The sidebar shows beside it: it's a popup, not the whole screen.
        assert!(shown.contains("tuinews"), "{shown}");
        assert!(shown.contains("Đoạn đầu."));
        let lines: Vec<&str> = shown.lines().collect();
        let at = |text: &str| lines.iter().position(|l| l.contains(text)).unwrap();
        // Rows kept for the picture (a shade until it comes) between the
        // paragraph and its caption.
        assert!(at("Chú thích ảnh") > at("Đoạn đầu.") + 4, "{shown}");
        assert!(shown.contains(app.icons.picture), "its mark while it loads");
    }

    #[test]
    fn a_narrow_window_takes_turns_and_nothing_breaks_when_tiny() {
        let mut app = app();
        let side = screen(&mut app, 60, 20);
        assert!(side.contains("VnExpress"));
        assert!(!side.contains("Tin số 3"));
        app.focus = Focus::Grid;
        let cards = screen(&mut app, 60, 20);
        assert!(cards.contains("Tin số 3"), "{cards}");
        for (w, h) in [(1, 1), (5, 3), (20, 6), (30, 4)] {
            screen(&mut app, w, h);
        }
        app.help = Some(HelpMenu::new(HelpTab::Settings));
        screen(&mut app, 10, 5);
        let help = screen(&mut app, 120, 40);
        assert!(help.contains("Catppuccin Mocha"));
    }

    #[test]
    fn long_titles_end_with_an_ellipsis_within_their_rows() {
        let rows = wrap_lines("một hai ba bốn năm sáu bảy tám chín mười", 10, 2);
        assert_eq!(rows.len(), 2);
        assert!(rows[1].ends_with('…'), "{rows:?}");
        assert!(rows.iter().all(|r| r.width() <= 10));
        assert_eq!(wrap_lines("ngắn", 10, 3), ["ngắn"]);
    }

    #[test]
    fn the_search_box_keeps_its_cursor_in_view() {
        let mut app = app();
        app.on_key(key(KeyCode::Char('/')));
        let cursor_on = |app: &App| {
            let line = search_box(app, 140, 0, Style::new());
            let block = line
                .spans
                .iter()
                .find(|s| s.style.bg == Some(app.colors.accent));
            block.map(|s| s.content.to_string())
        };
        let typed = &mut app.prompt.as_mut().unwrap().typed;
        *typed = Typed::new("hà nội mưa lớn".into());
        typed.edit(&key(KeyCode::Home), 100);
        for _ in 0..3 {
            typed.edit(&key(KeyCode::Right), 100);
        }
        let shown = screen(&mut app, 140, 40);
        assert!(shown.contains("hà nội mưa lớn"), "{shown}");
        assert_eq!(cursor_on(&app).as_deref(), Some("n"), "a block on the n");
        // At the end, a block after the last letter.
        app.prompt
            .as_mut()
            .unwrap()
            .typed
            .edit(&key(KeyCode::End), 100);
        assert_eq!(cursor_on(&app).as_deref(), Some(" "));
        // Longer than the box, the cursor at its start: it's in view.
        let typed = &mut app.prompt.as_mut().unwrap().typed;
        *typed = Typed::new("tin ".repeat(40));
        typed.edit(&key(KeyCode::Home), 100);
        let shown = screen(&mut app, 140, 40);
        assert!(shown.contains("tin tin"), "{shown}");
        assert_eq!(cursor_on(&app).as_deref(), Some("t"));
    }
}
