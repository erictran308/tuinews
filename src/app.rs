//! The app: its state, the event loop, and what keys and answers do.
//!
//! One `tokio::select!` over terminal events, `AppEvent`s (feeds fetched,
//! articles' pages, finished pictures) and a 1 s tick. Each wake handles
//! what came, then draws; after each frame `Images::fetch` starts what the
//! frame wanted. All state lives in `App`.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use unicode_normalization::char::is_combining_mark;
use unicode_segmentation::UnicodeSegmentation;

use crate::article::{self, Article, ArticleId, FeedId};
use crate::feed::{self, Fetched};
use crate::html::{self, Block};
use crate::icons::Icons;
use crate::images::{ImageEvent, Images};
use crate::settings::Settings;
use crate::store::{self, CategoryId, CategoryRow, FeedRow, Listing, Store};
use crate::text;
use crate::theme::{self, Colors};
use crate::ui::effects::Effects;

/// The longest line typed in a box.
pub const MAX_PROMPT: usize = 500;
/// This many different keys at once with no box open are a paste that came
/// as keystrokes (as pastes do on Windows), not commands.
const PASTED_KEYS: usize = 3;
/// How long a message stays in the status bar.
const STATUS_FOR: Duration = Duration::from_secs(6);
/// How often feeds are checked for being due.
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// How long a site that asked tuinews to slow down isn't asked again.
const SLOW_DOWN: i64 = 3600;
/// Rows the sidebar moves for Ctrl-d and Ctrl-u.
const SIDEBAR_PAGE: usize = 8;
/// Rows a popup scrolls for Ctrl-d and Ctrl-u.
const HELP_PAGE: isize = 8;

/// Now, in Unix seconds.
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// What comes back from work done off the event loop.
#[derive(Debug)]
pub enum AppEvent {
    Image(ImageEvent),
    /// A feed fetched, or not.
    Fetched {
        feed: FeedId,
        result: Result<Fetched>,
    },
    /// An article's text, read from its page.
    Page {
        id: ArticleId,
        result: Result<Vec<Block>>,
    },
    /// A feed you asked to add, found at `url` (or the page you gave named
    /// it), with what it holds; to go `into` the category shown then.
    Found {
        asked: String,
        into: Option<CategoryId>,
        result: Result<(String, feed::Feed)>,
    },
    /// SIGHUP or SIGTERM: quit as `q` does. Unix's only: nothing sends it
    /// on Windows.
    #[cfg_attr(not(unix), allow(dead_code))]
    Quit,
}

/// What the articles' pane shows.
#[derive(Clone, Debug, PartialEq)]
pub enum View {
    Home,
    Saved,
    History,
    /// One of your feeds, with its name.
    Feed(FeedId, String),
    /// One of your categories, with its name: its feeds' articles.
    Category(CategoryId, String),
    /// Your articles holding these words.
    Search(String),
}

impl View {
    pub fn title(&self) -> String {
        match self {
            View::Home => "Home".into(),
            View::Saved => "Saved".into(),
            View::History => "History".into(),
            View::Feed(_, name) | View::Category(_, name) => name.clone(),
            View::Search(q) => format!("Search: {q}"),
        }
    }

    fn listing(&self) -> Listing {
        match self {
            View::Home => Listing::Home,
            View::Saved => Listing::Saved,
            View::History => Listing::History,
            View::Feed(id, _) => Listing::Feed(*id),
            View::Category(id, _) => Listing::Category(*id),
            View::Search(q) => Listing::Search(article::fold(q)),
        }
    }

    /// Lists you keep yourself show everything in them: "only unread" and
    /// blocked words don't hide anything there.
    pub fn keeps_all(&self) -> bool {
        matches!(self, View::Saved | View::History)
    }
}

/// A row of the sidebar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Entry {
    Home,
    Search,
    Saved,
    History,
    /// Your category at this place in `App.categories`.
    Category(usize),
    /// Your feed at this place among those under Feeds (`App::listed`).
    Feed(usize),
}

/// The sidebar's rows above your categories and feeds.
pub const MENU: [Entry; 4] = [Entry::Home, Entry::Search, Entry::Saved, Entry::History];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Grid,
}

/// How the articles are laid out, as the last frame drew them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GridShape {
    pub cols: usize,
    pub rows: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PromptKind {
    Search,
    AddFeed,
}

/// A line being typed: a search, or a feed's address.
#[derive(Clone, Debug, PartialEq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub typed: Typed,
}

impl Prompt {
    pub fn new(kind: PromptKind, typed: Typed) -> Self {
        Self { kind, typed }
    }
}

/// What's typed in a box (a search, a feed's address, a category's name),
/// and where its cursor is: always between two letters as drawn, since only
/// [`Typed::edit`] and [`Typed::paste`] change them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Typed {
    text: String,
    /// Where typing goes: a byte offset in `text`.
    cursor: usize,
}

impl Typed {
    /// `text`, the cursor at its end.
    pub fn new(text: String) -> Self {
        let cursor = text.len();
        Self { text, cursor }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Where typing goes: a byte offset in [`Typed::text`].
    pub fn at(&self) -> usize {
        self.cursor
    }

    /// Edits with `key`, at the cursor (moved as it goes). Backspace, or
    /// Ctrl-h, which some terminals send for it, deletes the letter before
    /// it (an accent written apart first, as an input method that retypes a
    /// letter expects), Delete the letter after it, Ctrl-w or Alt-Backspace
    /// the word before it, and Ctrl-u all before it (what Cmd-Backspace
    /// sends). ← and → move it a letter; with Alt (or Alt-b and Alt-f, what
    /// macOS's Option-← and Option-→ send), a word; Home and End, or Ctrl-a
    /// and Ctrl-e (Cmd-← and Cmd-→), to either end; Ctrl with an arrow is
    /// the panes'. A character that shows goes in at it, typed with AltGr too
    /// (Ctrl and Alt, as Windows says it), while the line holds fewer than
    /// `max`.
    pub fn edit(&mut self, key: &KeyEvent, max: usize) {
        let altgr = key
            .modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL) && !altgr;
        let alt = key.modifiers.contains(KeyModifiers::ALT) && !altgr;
        let at = self.cursor;
        let backspace = key.code == KeyCode::Backspace || (ctrl && key.code == KeyCode::Char('h'));
        // What's deleted, and where the cursor goes.
        let (gone, cursor) = match key.code {
            _ if (backspace && alt) || (ctrl && key.code == KeyCode::Char('w')) => {
                let start = self.word_before();
                (start..at, start)
            }
            _ if backspace => {
                let start = match self.text[..at].chars().next_back() {
                    Some(c) if is_combining_mark(c) => at - c.len_utf8(),
                    _ => self.letter_before(),
                };
                (start..at, start)
            }
            KeyCode::Char('u') if ctrl => (0..at, 0),
            KeyCode::Char('a') if ctrl => (at..at, 0),
            KeyCode::Char('e') if ctrl => (at..at, self.text.len()),
            _ if ctrl => return,
            KeyCode::Delete => (at..self.letter_after(), at),
            KeyCode::Left | KeyCode::Char('b') if alt => (at..at, self.word_before()),
            KeyCode::Right | KeyCode::Char('f') if alt => (at..at, self.word_after()),
            KeyCode::Left => (at..at, self.letter_before()),
            KeyCode::Right => (at..at, self.letter_after()),
            KeyCode::Home => (at..at, 0),
            KeyCode::End => (at..at, self.text.len()),
            KeyCode::Char(c) if !alt && !text::is_hidden(c) && self.text.chars().count() < max => {
                self.text.insert(at, c);
                self.cursor = at + c.len_utf8();
                return;
            }
            _ => return,
        };
        self.text.replace_range(gone, "");
        self.cursor = cursor;
    }

    /// Where the letter before the cursor starts.
    fn letter_before(&self) -> usize {
        let at = self.cursor;
        self.text[..at]
            .grapheme_indices(true)
            .next_back()
            .map_or(at, |(i, _)| i)
    }

    /// Where the letter after the cursor ends.
    fn letter_after(&self) -> usize {
        let at = self.cursor;
        self.text[at..]
            .graphemes(true)
            .next()
            .map_or(at, |g| at + g.len())
    }

    /// Where the word before the cursor starts.
    fn word_before(&self) -> usize {
        self.text[..self.cursor]
            .trim_end()
            .rfind(' ')
            .map_or(0, |i| i + 1)
    }

    /// Where the word after the cursor ends.
    fn word_after(&self) -> usize {
        let rest = &self.text[self.cursor..];
        let start = rest.len() - rest.trim_start().len();
        self.cursor + rest[start..].find(' ').map_or(rest.len(), |i| start + i)
    }

    /// Puts `pasted` (a line, cleaned) in at the cursor, as much as fits in a
    /// line of `max` characters, and the cursor after it. `spaced`: whether
    /// it had a space before it and after it, kept (one) where a word next to
    /// it would run into it otherwise.
    pub fn paste(&mut self, pasted: &str, spaced: (bool, bool), max: usize) {
        if pasted.is_empty() {
            return;
        }
        let (before, after) = self.text.split_at(self.cursor);
        let word = |c: char| !c.is_whitespace();
        let lead = if spaced.0 && before.ends_with(word) {
            " "
        } else {
            ""
        };
        let trail = if spaced.1 && after.starts_with(word) {
            " "
        } else {
            ""
        };
        let line = format!("{lead}{pasted}{trail}");
        let room = max.saturating_sub(self.text.chars().count());
        let cut = text::first_chars(&line, room);
        self.text.insert_str(self.cursor, cut);
        self.cursor += cut.len();
    }
}

/// The tabs of the `?` popup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelpTab {
    Keys,
    Settings,
}

/// The `?` popup: every key, and the settings.
#[derive(Clone, Debug, PartialEq)]
pub struct HelpMenu {
    pub tab: HelpTab,
    /// The Keys tab's scroll.
    pub scroll: usize,
    /// The settings tab's row with the cursor, and its scroll.
    pub selected: usize,
    pub settings_scroll: usize,
}

impl HelpMenu {
    pub const SUMMARIES: usize = 0;
    pub const LIST: usize = 1;
    pub const UNREAD_ONLY: usize = 2;
    pub const FULL_TEXT: usize = 3;
    pub const HISTORY: usize = 4;
    pub const ANIMATIONS: usize = 5;
    pub const THEMES: usize = 6;
    pub const LAST: usize = Self::THEMES + theme::BUILT_IN.len() - 1;

    pub fn new(tab: HelpTab) -> Self {
        Self {
            tab,
            scroll: 0,
            selected: 0,
            settings_scroll: 0,
        }
    }

    /// Rows that pick one of a few (the themes), rather than turn on or
    /// off.
    pub fn is_choice(row: usize) -> bool {
        row >= Self::THEMES
    }
}

/// A list you keep, that an article can be taken off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kept {
    Saved,
    History,
}

/// A question waiting for `y`: any other key says no.
#[derive(Clone, Debug, PartialEq)]
pub enum Confirm {
    RemoveFeed(FeedId, String),
    DeleteCategory(CategoryId, String),
    TakeOff(Kept, ArticleId, String),
    MarkAllRead(Vec<ArticleId>),
}

/// The categories popup (`F`): a feed's categories ticked, to put it in or
/// take it out (`Enter`), make one (`n`), rename (`r`) or delete one (`x`);
/// with no feed, the categories alone.
#[derive(Clone, Debug, PartialEq)]
pub struct CategoryMenu {
    /// The feed whose categories are ticked: its id, name and address.
    pub feed: Option<(FeedId, String, String)>,
    /// The row with the cursor: a category, or "New category" after them.
    pub selected: usize,
    /// A name being typed: a new category's, or the one's being renamed.
    pub naming: Option<Naming>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Naming {
    pub renaming: Option<CategoryId>,
    pub typed: Typed,
}

impl Naming {
    /// The name being typed, `text` so far, the cursor at its end.
    fn new(renaming: Option<CategoryId>, text: String) -> Self {
        Self {
            renaming,
            typed: Typed::new(text),
        }
    }

    /// A new category's name, nothing typed yet.
    fn blank() -> Self {
        Self::new(None, String::new())
    }
}

/// What the reader shows under an article's title.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    /// The feed's summary alone (`full_text` off, or a page not over https).
    Summary,
    Loading,
    Text(Vec<Block>),
    /// Why its text couldn't be read: the summary shows, and `o` opens it.
    Failed(String),
}

/// An article open in the reader.
#[derive(Clone, Debug)]
pub struct Reader {
    pub article: Article,
    pub body: Body,
    /// The first line shown.
    pub scroll: usize,
    /// As far as it scrolls, and a page's rows, as the last frame drew it.
    pub max_scroll: Cell<usize>,
    pub page: Cell<usize>,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

pub struct App {
    pub store: Store,
    pub settings: Settings,
    settings_path: Option<PathBuf>,
    pub colors: Colors,
    pub icons: Icons,
    http: reqwest::Client,
    tx: UnboundedSender<AppEvent>,
    pub images: Images,
    pub effects: Effects,
    /// The splash screen, while it's up over the first frames
    /// (`ui::splash`, `keep_splash`).
    pub splash: Option<crate::ui::splash::Splash>,
    pub quit: bool,

    /// Every feed, those only in categories too (`App::listed`: those under
    /// Feeds).
    pub feeds: Vec<FeedRow>,
    pub categories: Vec<CategoryRow>,
    /// How many of Home's articles you haven't read.
    pub home_unread: usize,
    pub view: View,
    back: Vec<(View, Option<ArticleId>)>,
    /// What the view shows, in order.
    pub articles: Vec<Article>,
    pub selected: usize,
    /// The first row of cards shown.
    pub scroll: usize,
    pub grid: Cell<GridShape>,
    pub focus: Focus,
    pub sidebar_selected: usize,
    pub sidebar_scroll: Cell<usize>,
    pub read: HashSet<ArticleId>,
    pub saved: HashSet<ArticleId>,
    /// Read since the view was opened: they stay in it with "only unread"
    /// on, so the cards don't move under you as you read.
    read_here: HashSet<ArticleId>,

    pub prompt: Option<Prompt>,
    /// What was typed in the search box when it was left, and where its
    /// cursor was.
    pub search_typed: Option<Typed>,
    pub help: Option<HelpMenu>,
    pub confirm: Option<Confirm>,
    pub category_menu: Option<CategoryMenu>,
    pub reader: Option<Reader>,
    pub status: Option<Status>,

    /// Feeds fetched so far this round, and of how many.
    pub refreshing: Option<(usize, usize)>,
    /// When the last round ended.
    pub feeds_updated: Option<i64>,
    fetching: HashSet<FeedId>,
    fetch_limit: Arc<Semaphore>,
    /// Feeds whose site asked to slow down, and until when.
    slowed: HashMap<FeedId, i64>,
    last_check: Instant,
    /// A feed being looked for, to add it: what you typed.
    pub adding: Option<String>,
    pending_g: bool,
    needs_reload: bool,
    /// The system clipboard, kept for the whole run: on Linux, copied text
    /// lasts only while it's held.
    clipboard: Option<arboard::Clipboard>,
}

/// Ctrl and an arrow or `h j k l`: the pane that way.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Toward {
    Left,
    Down,
    Up,
    Right,
}

fn toward(key: &KeyEvent) -> Option<Toward> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    Some(match key.code {
        KeyCode::Char('h') | KeyCode::Left => Toward::Left,
        KeyCode::Char('j') | KeyCode::Down => Toward::Down,
        KeyCode::Char('k') | KeyCode::Up => Toward::Up,
        KeyCode::Char('l') | KeyCode::Right => Toward::Right,
        _ => return None,
    })
}

impl App {
    pub fn new(
        store: Store,
        settings: Settings,
        settings_path: Option<PathBuf>,
        http: reqwest::Client,
        tx: UnboundedSender<AppEvent>,
        images: Images,
    ) -> Self {
        let colors = Colors::named(&settings.theme);
        let icons = Icons::pick(settings.icon_mode(), |name| std::env::var(name).ok());
        let mut app = Self {
            store,
            settings,
            settings_path,
            colors,
            icons,
            http,
            tx,
            images,
            effects: Effects::default(),
            splash: None,
            quit: false,
            feeds: Vec::new(),
            categories: Vec::new(),
            home_unread: 0,
            view: View::Home,
            back: Vec::new(),
            articles: Vec::new(),
            selected: 0,
            scroll: 0,
            grid: Cell::new(GridShape { cols: 1, rows: 1 }),
            focus: Focus::Sidebar,
            sidebar_selected: 0,
            sidebar_scroll: Cell::new(0),
            read: HashSet::new(),
            saved: HashSet::new(),
            read_here: HashSet::new(),
            prompt: None,
            search_typed: None,
            help: None,
            confirm: None,
            category_menu: None,
            reader: None,
            status: None,
            refreshing: None,
            feeds_updated: None,
            fetching: HashSet::new(),
            fetch_limit: Arc::new(Semaphore::new(feed::PARALLEL)),
            slowed: HashMap::new(),
            last_check: Instant::now(),
            adding: None,
            pending_g: false,
            needs_reload: false,
            clipboard: None,
        };
        app.reload_marks();
        app.reload();
        app
    }

    pub async fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        mut rx: UnboundedReceiver<AppEvent>,
    ) -> Result<()> {
        let mut keys = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        #[cfg(unix)]
        quit_on_hangup(self.tx.clone())?;
        self.refresh_feeds(false);
        let mut result = Ok(());
        loop {
            if self.quit {
                break;
            }
            // The frames of an animation come at a steady pace, counted from
            // when each began, not from when the one before ended.
            let frame_began = tokio::time::Instant::now();
            // A frame that can't be drawn ends tuinews; when the tab was
            // closed (the hangup not read yet), it's a quit.
            if let Err(err) = terminal.draw(|frame| crate::ui::draw(frame, self)) {
                if !crate::screen::gone() {
                    result = Err(err.into());
                }
                break;
            }
            self.images.fetch();
            tokio::select! {
                event = keys.next() => match event {
                    Some(Ok(event)) => {
                        // Whatever else is already waiting is read with it:
                        // a paste that comes as keystrokes arrives at once.
                        let mut events = vec![event];
                        while events.len() < MAX_PROMPT
                            && crossterm::event::poll(Duration::ZERO).unwrap_or(false)
                        {
                            match crossterm::event::read() {
                                Ok(event) => events.push(event),
                                Err(_) => break,
                            }
                        }
                        self.on_terminal_events(events);
                    }
                    Some(Err(_)) | None => self.quit = true,
                },
                Some(event) = rx.recv() => {
                    self.on_event(event);
                    while let Ok(event) = rx.try_recv() {
                        self.on_event(event);
                    }
                }
                _ = tick.tick() => self.on_tick(),
                // The animations move on their own frames, only while one moves.
                _ = tokio::time::sleep_until(frame_began + self.effects.every()),
                    if self.effects.moving() => {}
                // So does the splash screen, while it's up.
                _ = tokio::time::sleep_until(
                    frame_began + self.splash.as_ref().map_or(Duration::ZERO, |s| s.every()),
                ), if self.splash.is_some() => {}
            }
            if std::mem::take(&mut self.needs_reload) {
                self.reload();
            }
        }
        result
    }

    // Messages.

    pub fn info(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            error: false,
            at: Instant::now(),
        });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            error: true,
            at: Instant::now(),
        });
    }

    fn on_tick(&mut self) {
        if self
            .status
            .as_ref()
            .is_some_and(|s| s.at.elapsed() > STATUS_FOR)
        {
            self.status = None;
        }
        if self.last_check.elapsed() >= CHECK_EVERY {
            self.last_check = Instant::now();
            self.refresh_feeds(false);
        }
    }

    fn on_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Image(event) => self.images.on_built(event),
            AppEvent::Fetched { feed, result } => self.on_fetched(feed, result),
            AppEvent::Page { id, result } => self.on_page(&id, result),
            AppEvent::Found {
                asked,
                into,
                result,
            } => self.on_found(&asked, into, result),
            AppEvent::Quit => self.quit = true,
        }
    }

    // What's shown.

    /// The read and saved marks, from the store.
    fn reload_marks(&mut self) {
        match (self.store.read(), self.store.saved()) {
            (Ok(read), Ok(saved)) => {
                self.read = read;
                self.saved = saved;
            }
            (Err(e), _) | (_, Err(e)) => self.error(format!("Couldn't read your marks: {e:#}")),
        }
    }

    /// The feeds and the view's articles again, from the store, keeping the
    /// selection on the same article (and the sidebar on the same row).
    pub fn reload(&mut self) {
        // The sidebar's row stays on what it was on, as rows above it come
        // and go.
        let on = match self.entry(self.sidebar_selected) {
            Entry::Category(i) => self
                .categories
                .get(i)
                .map(|c| View::Category(c.id, c.name.clone())),
            Entry::Feed(i) => self.listed_nth(i).map(|f| View::Feed(f.id, f.name())),
            _ => None,
        };
        self.refresh_counts();
        if let Some(row) = on.and_then(|view| self.row_of(&view)) {
            self.sidebar_selected = row;
        }
        self.sidebar_selected = self.sidebar_selected.min(self.sidebar_len() - 1);
        // A feed or category shown that's gone: Home. One renamed: its name.
        match &self.view {
            View::Feed(id, _) => match self.feeds.iter().find(|f| f.id == *id) {
                Some(feed) => self.view = View::Feed(feed.id, feed.name()),
                None => self.view = View::Home,
            },
            View::Category(id, _) => match self.categories.iter().find(|c| c.id == *id) {
                Some(category) => self.view = View::Category(category.id, category.name.clone()),
                None => self.view = View::Home,
            },
            _ => {}
        }
        let keep = self.selected_article().map(|a| a.id.clone());
        let mut articles = match self.store.articles(&self.view.listing()) {
            Ok(articles) => articles,
            Err(e) => {
                self.error(format!("Couldn't read the articles: {e:#}"));
                Vec::new()
            }
        };
        if !self.view.keeps_all() {
            let blocks = self.settings.blocks();
            let unread_only = self.settings.unread_only;
            articles.retain(|a| {
                !blocks.hold(a)
                    && (!unread_only
                        || !self.read.contains(&a.id)
                        || self.read_here.contains(&a.id))
            });
        }
        self.articles = articles;
        // At the top, the newest stays selected as newer ones come in;
        // further down, the same article does.
        let at_top = self.selected == 0 && self.reader.is_none();
        self.selected = keep
            .filter(|_| !at_top)
            .and_then(|id| self.articles.iter().position(|a| a.id == id))
            .unwrap_or(self.selected)
            .min(self.articles.len().saturating_sub(1));
    }

    /// Shows `view`, remembering the one before for Backspace.
    pub fn show(&mut self, view: View) {
        if view == self.view {
            return self.reload();
        }
        let here = self.selected_article().map(|a| a.id.clone());
        self.back
            .push((std::mem::replace(&mut self.view, view), here));
        if self.back.len() > 50 {
            self.back.remove(0);
        }
        self.selected = 0;
        self.scroll = 0;
        self.read_here.clear();
        self.reload();
    }

    /// Backspace: the view before, with what was selected in it.
    fn go_back(&mut self) {
        let Some((view, selected)) = self.back.pop() else {
            return;
        };
        self.view = view;
        self.read_here.clear();
        self.scroll = 0;
        self.selected = 0;
        self.reload();
        if let Some(id) = selected
            && let Some(i) = self.articles.iter().position(|a| a.id == id)
        {
            self.selected = i;
        }
        self.sync_sidebar();
    }

    /// The sidebar's row for the view shown.
    fn sync_sidebar(&mut self) {
        if let Some(row) = self.row_of(&self.view.clone()) {
            self.sidebar_selected = row;
        }
    }

    /// The sidebar's row that shows `view`, if one does.
    pub fn row_of(&self, view: &View) -> Option<usize> {
        let categories = MENU.len();
        let feeds = categories + self.categories.len();
        match view {
            View::Home => Some(0),
            View::Search(_) => Some(1),
            View::Saved => Some(2),
            View::History => Some(3),
            View::Category(id, _) => self
                .categories
                .iter()
                .position(|c| c.id == *id)
                .map(|i| categories + i),
            View::Feed(id, _) => self.listed().position(|f| f.id == *id).map(|i| feeds + i),
        }
    }

    /// Your feeds under Feeds, in the sidebar's order: not the sites'
    /// sections only in categories.
    pub fn listed(&self) -> impl Iterator<Item = &FeedRow> {
        self.feeds.iter().filter(|f| f.listed)
    }

    fn listed_nth(&self, i: usize) -> Option<&FeedRow> {
        self.listed().nth(i)
    }

    pub fn sidebar_len(&self) -> usize {
        MENU.len() + self.categories.len() + self.listed().count()
    }

    pub fn entry(&self, row: usize) -> Entry {
        if let Some(entry) = MENU.get(row) {
            return *entry;
        }
        let row = row - MENU.len();
        if row < self.categories.len() {
            Entry::Category(row)
        } else {
            Entry::Feed(row - self.categories.len())
        }
    }

    /// The view a sidebar row shows, if it shows one (Search opens the
    /// search box).
    fn entry_view(&self, entry: Entry) -> Option<View> {
        Some(match entry {
            Entry::Home => View::Home,
            Entry::Saved => View::Saved,
            Entry::History => View::History,
            Entry::Search => match &self.view {
                View::Search(_) => self.view.clone(),
                _ => return None,
            },
            Entry::Category(i) => {
                let category = self.categories.get(i)?;
                View::Category(category.id, category.name.clone())
            }
            Entry::Feed(i) => {
                let feed = self.listed_nth(i)?;
                View::Feed(feed.id, feed.name())
            }
        })
    }

    pub fn selected_article(&self) -> Option<&Article> {
        if let Some(reader) = &self.reader {
            return Some(&reader.article);
        }
        self.articles.get(self.selected)
    }

    pub fn is_read(&self, id: &ArticleId) -> bool {
        self.read.contains(id)
    }

    /// The feeds, categories and how many of each you haven't read, again
    /// from the store.
    fn refresh_counts(&mut self) {
        match (
            self.store.feeds(),
            self.store.categories(),
            self.store.home_unread(),
        ) {
            (Ok(feeds), Ok(categories), Ok(unread)) => {
                self.feeds = feeds;
                self.categories = categories;
                self.home_unread = unread;
            }
            (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => {
                self.error(format!("Couldn't read your feeds: {e:#}"));
            }
        }
    }

    /// Whether a popup is open over the screen (over the reader too, if
    /// it's open).
    pub fn popup_open(&self) -> bool {
        self.help.is_some() || self.confirm.is_some() || self.category_menu.is_some()
    }

    /// Whether anything is open over the articles: a popup, or the reader.
    pub fn covers_articles(&self) -> bool {
        self.popup_open() || self.reader.is_some()
    }

    /// Keeps the splash screen drawn since `since` up over the first frames
    /// while what they show loads, pictures held (`ui::splash`).
    pub fn keep_splash(&mut self, since: Instant) {
        self.splash = Some(crate::ui::splash::Splash::new(since));
        self.images.hold(true);
    }

    /// The first screen is all there, for the splash screen to leave: the
    /// pictures the frame before asked for, and articles to show (on a
    /// first start, once a feed has brought some, or every feed came).
    pub fn first_screen_in(&self) -> bool {
        self.images.missing() == Some(0) && (!self.articles.is_empty() || self.refreshing.is_none())
    }

    /// Whether the list shows instead of cards.
    pub fn shows_list(&self) -> bool {
        self.settings.list
    }

    // Feeds.

    /// Fetches every feed that's due (all of them, with `force`), at most
    /// `feed::PARALLEL` at once.
    pub fn refresh_feeds(&mut self, force: bool) {
        let at = now();
        let every = self.settings.refresh_every();
        let due: Vec<FeedRow> = self
            .feeds
            .iter()
            .filter(|f| !self.fetching.contains(&f.id))
            .filter(|f| self.slowed.get(&f.id).is_none_or(|until| *until <= at))
            .filter(|f| force || f.fetched_at.is_none_or(|t| at.saturating_sub(t) >= every))
            .cloned()
            .collect();
        if due.is_empty() {
            return;
        }
        let (done, total) = self.refreshing.unwrap_or((0, 0));
        self.refreshing = Some((done, total + due.len()));
        for row in due {
            self.fetching.insert(row.id);
            let (client, tx, limit) =
                (self.http.clone(), self.tx.clone(), self.fetch_limit.clone());
            tokio::spawn(async move {
                let Ok(_permit) = limit.acquire_owned().await else {
                    return;
                };
                let result = feed::fetch(
                    &client,
                    &row.url,
                    row.etag.as_deref(),
                    row.last_modified.as_deref(),
                )
                .await;
                let _ = tx.send(AppEvent::Fetched {
                    feed: row.id,
                    result,
                });
            });
        }
    }

    fn on_fetched(&mut self, id: FeedId, result: Result<Fetched>) {
        self.fetching.remove(&id);
        if let Some((done, total)) = self.refreshing.as_mut() {
            *done += 1;
            if *done >= *total {
                self.refreshing = None;
                self.feeds_updated = Some(now());
            }
        }
        // Removed while it was fetched.
        let Some(row) = self.feeds.iter().find(|f| f.id == id).cloned() else {
            return;
        };
        let at = now();
        let saved = match result {
            Ok(Fetched::Unchanged) => self.store.feed_fetched(id, at, None, None),
            Ok(Fetched::New {
                feed,
                etag,
                last_modified,
            }) => {
                let source = if row.title.is_empty() {
                    feed.title.clone()
                } else {
                    row.title.clone()
                };
                self.store
                    .name_feed_if_unnamed(id, &feed.title)
                    .and_then(|()| self.store.save_articles(id, &source, &feed.items, at))
                    .and_then(|_| {
                        self.store
                            .feed_fetched(id, at, etag.as_deref(), last_modified.as_deref())
                    })
            }
            Err(e) => {
                if e.is::<feed::TooManyRequests>() {
                    self.slowed.insert(id, at + SLOW_DOWN);
                }
                self.store.feed_failed(id, at, &format!("{e:#}"))
            }
        };
        if let Err(e) = saved {
            self.error(format!("Couldn't keep {}'s articles: {e:#}", row.name()));
        }
        self.needs_reload = true;
    }

    /// R: every feed now, and the pictures that failed again.
    fn refresh_now(&mut self) {
        self.images.retry_failed();
        self.refresh_feeds(true);
        if self.refreshing.is_some() {
            self.info("Fetching your feeds…");
        }
    }

    /// `a`: the box to add a feed in.
    fn open_add_feed(&mut self, text: String) {
        self.prompt = Some(Prompt::new(PromptKind::AddFeed, Typed::new(text)));
    }

    /// Looks for the feed at what you typed (a feed's address, or a site's
    /// page that names its feed), to add it once it's found: into the
    /// category shown, if one is (and then only there), else under Feeds.
    fn add_feed(&mut self, typed: &str) {
        let typed = typed.trim();
        let with_scheme = if typed.contains("://") {
            typed.to_string()
        } else {
            format!("https://{typed}")
        };
        let Some(url) = article::checked_link(&with_scheme, None) else {
            return self.error("That isn't a web address tuinews can fetch");
        };
        let into = match &self.view {
            View::Category(id, _) => Some(*id),
            _ => None,
        };
        if let Some(row) = self.feeds.iter().find(|f| f.url == url).cloned() {
            return self.put_known_feed(&row, into);
        }
        self.adding = Some(url.clone());
        self.info(format!("Looking for a feed at {url}…"));
        let (client, tx) = (self.http.clone(), self.tx.clone());
        tokio::spawn(async move {
            let result = feed::discover(&client, &url).await;
            let _ = tx.send(AppEvent::Found {
                asked: url,
                into,
                result,
            });
        });
    }

    /// A feed you asked to add that's yours already: into the category, if
    /// it was asked for in one; else under Feeds, if it was only a section
    /// in categories (not when the category asked for is gone: it's in
    /// others still).
    fn put_known_feed(&mut self, row: &FeedRow, into: Option<CategoryId>) {
        let (feed, name) = (row.id, row.name());
        let category = into.and_then(|id| self.categories.iter().find(|c| c.id == id).cloned());
        let Some(category) = category else {
            if row.listed || into.is_some() {
                return self.info("That feed is yours already");
            }
            return match self.store.list_feed(feed) {
                Ok(()) => {
                    self.reload();
                    self.info(format!("{name} is under Feeds now"))
                }
                Err(e) => self.error(format!("Couldn't put it under Feeds: {e:#}")),
            };
        };
        match self.store.set_in_category(category.id, feed, true) {
            Ok(_) => {
                self.reload();
                self.info(format!("{name} is in {} now", category.name));
            }
            Err(e) => self.error(format!("Couldn't put it in {}: {e:#}", category.name)),
        }
    }

    fn on_found(
        &mut self,
        asked: &str,
        into: Option<CategoryId>,
        result: Result<(String, feed::Feed)>,
    ) {
        if self.adding.as_deref() == Some(asked) {
            self.adding = None;
        }
        let (url, found) = match result {
            Ok(found) => found,
            Err(e) => return self.error(format!("No feed at {asked}: {e:#}")),
        };
        if let Some(row) = self.feeds.iter().find(|f| f.url == url).cloned() {
            return self.put_known_feed(&row, into);
        }
        // Into a category that's still there: only in it.
        let into = into.and_then(|id| self.categories.iter().find(|c| c.id == id).cloned());
        let at = now();
        let added = self
            .store
            .add_feed(&url, &found.title, into.is_none())
            .and_then(|id| {
                if let Some(category) = &into {
                    self.store.set_in_category(category.id, id, true)?;
                }
                self.store
                    .save_articles(id, &found.title, &found.items, at)
                    .and_then(|_| self.store.feed_fetched(id, at, None, None))
                    .map(|()| id)
            });
        match added {
            Ok(id) => {
                self.reload();
                let name = self
                    .feeds
                    .iter()
                    .find(|f| f.id == id)
                    .map(FeedRow::name)
                    .unwrap_or_default();
                let n = found.items.len();
                match into {
                    Some(category) => {
                        self.info(format!("Added {name} to {}: {n} articles", category.name));
                    }
                    None => {
                        self.info(format!("Added {name}: {n} articles"));
                        self.show(View::Feed(id, name));
                        self.sync_sidebar();
                    }
                }
                self.focus = Focus::Grid;
            }
            Err(e) => self.error(format!("Couldn't add it: {e:#}")),
        }
    }

    fn remove_feed(&mut self, id: FeedId, name: &str) {
        match self.store.remove_feed(id) {
            Ok(()) => {
                self.back
                    .retain(|(v, _)| !matches!(v, View::Feed(f, _) if *f == id));
                self.reload_marks();
                self.reload();
                self.info(format!("Removed {name}. a adds a feed again"));
            }
            Err(e) => self.error(format!("Couldn't remove it: {e:#}")),
        }
    }

    // Categories.

    /// `F`: the categories popup, for the feed of the sidebar's row, else
    /// the selected article's (in a category, its feed there), else for
    /// none: the categories to rename or delete.
    fn open_categories(&mut self) {
        let row = self.entry(self.sidebar_selected);
        let on_sidebar = self.focus == Focus::Sidebar && self.reader.is_none();
        let feed = if on_sidebar {
            match row {
                Entry::Feed(i) => self.listed_nth(i).map(|f| f.id),
                _ => None,
            }
        } else {
            self.selected_article()
                .cloned()
                .and_then(|article| self.feed_of(&article))
        };
        let feed = feed
            .and_then(|id| self.feeds.iter().find(|f| f.id == id))
            .map(|f| (f.id, f.name(), f.url.clone()));
        // On the category of the sidebar's row, or the one shown.
        let shown = match &self.view {
            View::Category(id, _) => self.categories.iter().position(|c| c.id == *id),
            _ => None,
        };
        let selected = match row {
            Entry::Category(i) if on_sidebar => i,
            _ => shown.unwrap_or(0),
        };
        // With none yet, it opens on naming the first.
        let naming = self.categories.is_empty().then(Naming::blank);
        self.category_menu = Some(CategoryMenu {
            feed,
            selected,
            naming,
        });
    }

    /// The feed an article came from; in a category shown, its feed there.
    fn feed_of(&self, article: &Article) -> Option<FeedId> {
        if let View::Category(id, _) = &self.view
            && let Some(category) = self.categories.iter().find(|c| c.id == *id)
            && let Ok(feeds) = self.store.feeds_of(&article.id)
            && let Some(feed) = feeds.into_iter().find(|f| category.feeds.contains(f))
        {
            return Some(feed);
        }
        article.feed
    }

    fn on_category_key(&mut self, key: KeyEvent) {
        let Some(menu) = self.category_menu.as_mut() else {
            return;
        };
        if let Some(naming) = menu.naming.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    menu.naming = None;
                    if self.categories.is_empty() {
                        self.category_menu = None;
                    }
                }
                KeyCode::Enter => {
                    if let Some(naming) = menu.naming.take() {
                        self.name_category(naming);
                    }
                }
                _ => {
                    naming.typed.edit(&key, store::MAX_CATEGORY_NAME);
                }
            }
            return;
        }
        // The categories, then "New category".
        let last = self.categories.len();
        menu.selected = menu.selected.min(last);
        let on = (menu.selected < last).then_some(menu.selected);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'F') => self.category_menu = None,
            KeyCode::Char('j') | KeyCode::Down => menu.selected = (menu.selected + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => menu.selected = menu.selected.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => menu.selected = 0,
            KeyCode::Char('G') | KeyCode::End => menu.selected = last,
            KeyCode::Char('n') => {
                menu.selected = last;
                menu.naming = Some(Naming::blank());
            }
            KeyCode::Enter | KeyCode::Char(' ') => match on {
                None => {
                    menu.naming = Some(Naming::blank());
                }
                Some(i) if menu.feed.is_some() => self.toggle_in_category(i),
                // No feed to put in: Enter opens the category.
                Some(i) => {
                    let category = &self.categories[i];
                    let view = View::Category(category.id, category.name.clone());
                    self.category_menu = None;
                    self.show(view);
                    self.sync_sidebar();
                    self.focus = Focus::Grid;
                }
            },
            KeyCode::Char('r') => {
                if let Some(i) = on {
                    let category = &self.categories[i];
                    menu.naming = Some(Naming::new(Some(category.id), category.name.clone()));
                }
            }
            KeyCode::Char('x' | 'd') | KeyCode::Delete => {
                if let Some(i) = on {
                    let category = &self.categories[i];
                    self.confirm =
                        Some(Confirm::DeleteCategory(category.id, category.name.clone()));
                }
            }
            _ => {}
        }
    }

    /// Enter on a category in the popup: the feed in it, or out of it.
    fn toggle_in_category(&mut self, i: usize) {
        let feed = self.category_menu.as_ref().and_then(|m| m.feed.clone());
        let (Some((feed, name, _)), Some(category)) = (feed, self.categories.get(i).cloned())
        else {
            return;
        };
        let on = !category.feeds.contains(&feed);
        match self.store.set_in_category(category.id, feed, on) {
            Ok(gone) => {
                self.reload_marks();
                self.reload();
                match gone {
                    Some(gone) => {
                        self.category_menu = None;
                        self.info(format!(
                            "Removed {gone}: it was in no other category, nor under Feeds"
                        ));
                    }
                    None if on => self.info(format!("{name} is in {} now", category.name)),
                    None => self.info(format!("{name} is out of {}", category.name)),
                }
            }
            Err(e) => self.error(format!("Couldn't change {}: {e:#}", category.name)),
        }
    }

    /// Enter on a name typed in the popup: a new category (with the feed in
    /// it, if the popup is a feed's), or one renamed. A name that won't do
    /// stays to be fixed.
    fn name_category(&mut self, naming: Naming) {
        let feed = self
            .category_menu
            .as_ref()
            .and_then(|m| m.feed.as_ref().map(|f| f.0));
        let done = match naming.renaming {
            Some(id) => self
                .store
                .rename_category(id, naming.typed.text())
                .map(|()| format!("Renamed it {}", naming.typed.text().trim())),
            None => self.store.add_category(naming.typed.text()).and_then(|id| {
                if let Some(feed) = feed {
                    self.store.set_in_category(id, feed, true)?;
                }
                Ok(format!("Made {}", naming.typed.text().trim()))
            }),
        };
        match done {
            Ok(said) => {
                self.reload();
                if let Some(menu) = self.category_menu.as_mut() {
                    let name = article::one_line(naming.typed.text(), store::MAX_CATEGORY_NAME);
                    if let Some(i) = self.categories.iter().position(|c| c.name == name) {
                        menu.selected = i;
                    }
                }
                self.info(said);
            }
            Err(e) => {
                self.error(format!("{e:#}"));
                if let Some(menu) = self.category_menu.as_mut() {
                    menu.naming = Some(naming);
                }
            }
        }
    }

    fn delete_category(&mut self, id: CategoryId, name: &str) {
        match self.store.delete_category(id) {
            Ok(gone) => {
                self.back
                    .retain(|(v, _)| !matches!(v, View::Category(c, _) if *c == id));
                self.reload_marks();
                self.reload();
                if gone.is_empty() {
                    self.info(format!("Deleted {name}"));
                } else {
                    self.info(format!(
                        "Deleted {name}, and its {} feeds only it had",
                        gone.len()
                    ));
                }
            }
            Err(e) => self.error(format!("Couldn't delete it: {e:#}")),
        }
    }

    // Reading.

    /// Enter: the selected article in the reader, marked read (and put in
    /// History), with its text read from its page.
    fn open_selected(&mut self) {
        let Some(article) = self.articles.get(self.selected).cloned() else {
            return;
        };
        self.open(article);
    }

    fn open(&mut self, article: Article) {
        self.mark_read(std::slice::from_ref(&article.id));
        if self.settings.history
            && let Err(e) = self.store.opened(&article.id, now())
        {
            self.error(format!("Couldn't keep it in History: {e:#}"));
        }
        let body = self.body_of(&article);
        self.reader = Some(Reader {
            article,
            body,
            scroll: 0,
            max_scroll: Cell::new(0),
            page: Cell::new(10),
        });
    }

    /// The article's text if it's kept, else it's read from its page (only
    /// over https, as everything tuinews fetches).
    fn body_of(&mut self, article: &Article) -> Body {
        if !self.settings.full_text {
            return Body::Summary;
        }
        // A copy kept before pictures were is read from the page again.
        match self.store.body(&article.id) {
            Ok(Some(stored)) => {
                if let Some(blocks) = Block::decode(&stored) {
                    return Body::Text(blocks);
                }
            }
            Ok(None) => {}
            Err(e) => self.error(format!("Couldn't read the kept text: {e:#}")),
        }
        if !article.link.starts_with("https://") {
            return Body::Failed("its page isn't on https, so tuinews doesn't fetch it".into());
        }
        let (client, tx) = (self.http.clone(), self.tx.clone());
        let (id, link) = (article.id.clone(), article.link.clone());
        tokio::spawn(async move {
            let result = async {
                let page = feed::page(&client, &link).await?;
                let base = link.clone();
                let blocks =
                    tokio::task::spawn_blocking(move || html::article_text(&page, &base)).await?;
                anyhow::Ok(blocks)
            }
            .await;
            let _ = tx.send(AppEvent::Page { id, result });
        });
        Body::Loading
    }

    fn on_page(&mut self, id: &ArticleId, result: Result<Vec<Block>>) {
        let body = match result {
            Ok(blocks) if !blocks.is_empty() => {
                if let Err(e) = self.store.save_body(id, &Block::encode(&blocks), now()) {
                    self.error(format!("Couldn't keep the text: {e:#}"));
                }
                Body::Text(blocks)
            }
            Ok(_) => Body::Failed("no text was found on its page".into()),
            Err(e) => Body::Failed(format!("{e:#}")),
        };
        if let Some(reader) = self.reader.as_mut()
            && reader.article.id == *id
        {
            reader.body = body;
        }
    }

    /// The reader closes.
    fn close_reader(&mut self) {
        self.reader = None;
        self.needs_reload = true;
    }

    /// `]` / `[` in the reader: the next or the one before in the list.
    fn read_next(&mut self, forward: bool) {
        let Some(reader) = &self.reader else {
            return;
        };
        let at = self
            .articles
            .iter()
            .position(|a| a.id == reader.article.id)
            .unwrap_or(self.selected);
        let next = if forward {
            (at + 1 < self.articles.len()).then_some(at + 1)
        } else {
            at.checked_sub(1)
        };
        match next.and_then(|i| self.articles.get(i).cloned()) {
            Some(article) => {
                self.selected = next.unwrap_or(at);
                self.open(article);
            }
            None => self.info(if forward {
                "That was the last one"
            } else {
                "That was the first one"
            }),
        }
    }

    fn mark_read(&mut self, ids: &[ArticleId]) {
        let new: Vec<ArticleId> = ids
            .iter()
            .filter(|id| !self.read.contains(id))
            .cloned()
            .collect();
        if new.is_empty() {
            return;
        }
        match self.store.mark_read(&new, now()) {
            Ok(()) => {
                for id in new {
                    self.read_here.insert(id.clone());
                    self.read.insert(id);
                }
                // The sidebar's counts.
                self.refresh_counts();
            }
            Err(e) => self.error(format!("Couldn't mark it read: {e:#}")),
        }
    }

    /// `m`: read or unread again.
    fn toggle_read(&mut self) {
        let Some(id) = self.selected_article().map(|a| a.id.clone()) else {
            return;
        };
        if !self.read.contains(&id) {
            self.mark_read(&[id]);
            return self.info("Marked read");
        }
        match self.store.mark_unread(&id) {
            Ok(()) => {
                self.read.remove(&id);
                self.read_here.remove(&id);
                self.refresh_counts();
                self.info("Marked unread");
            }
            Err(e) => self.error(format!("Couldn't mark it unread: {e:#}")),
        }
    }

    /// `M`: asks to mark every article shown read.
    fn ask_mark_all_read(&mut self) {
        let unread: Vec<ArticleId> = self
            .articles
            .iter()
            .filter(|a| !self.read.contains(&a.id))
            .map(|a| a.id.clone())
            .collect();
        if unread.is_empty() {
            return self.info("Nothing unread here");
        }
        self.confirm = Some(Confirm::MarkAllRead(unread));
    }

    /// `w`: saves the article for later, or (asking first) unsaves it.
    fn toggle_saved(&mut self) {
        let Some(article) = self.selected_article().cloned() else {
            return;
        };
        if self.saved.contains(&article.id) {
            self.confirm = Some(Confirm::TakeOff(Kept::Saved, article.id, article.title));
            return;
        }
        match self.store.save(&article.id, now()) {
            Ok(()) => {
                self.saved.insert(article.id);
                self.info("Saved: it's in Saved, on the sidebar, for as long as you keep it");
            }
            Err(e) => self.error(format!("Couldn't save it: {e:#}")),
        }
    }

    /// `x`: takes the article off Saved or History (asking first); in the
    /// sidebar, removes the feed (asking first).
    fn ask_to_remove(&mut self) {
        let Some(article) = self.articles.get(self.selected).cloned() else {
            return;
        };
        let list = match self.view {
            View::Saved => Kept::Saved,
            View::History => Kept::History,
            _ => return self.info("x takes an article off Saved or History"),
        };
        self.confirm = Some(Confirm::TakeOff(list, article.id, article.title));
    }

    fn take_off(&mut self, list: Kept, id: &ArticleId) {
        let done = match list {
            Kept::Saved => self.store.unsave(id).map(|()| {
                self.saved.remove(id);
                "Taken off Saved"
            }),
            Kept::History => self
                .store
                .take_off_history(id)
                .map(|()| "Taken off History"),
        };
        match done {
            Ok(said) => {
                self.info(said);
                self.reload();
            }
            Err(e) => self.error(format!("Couldn't take it off: {e:#}")),
        }
    }

    /// `o`: the article in your browser, marked read.
    fn open_in_browser(&mut self) {
        let Some(article) = self.selected_article().cloned() else {
            return;
        };
        match open_url(&article.link) {
            Ok(()) => {
                self.info(format!("Opened {}", article.host()));
                self.mark_read(&[article.id]);
            }
            Err(e) => self.error(format!("Couldn't open your browser: {e}")),
        }
    }

    /// `y`: copies the article's link.
    fn copy_link(&mut self) {
        let Some(url) = self.selected_article().map(|a| a.link.clone()) else {
            return;
        };
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let copied = self
            .clipboard
            .as_mut()
            .is_some_and(|clipboard| clipboard.set_text(url.clone()).is_ok());
        // No system clipboard (over SSH, say): the terminal's own copy
        // (OSC 52), which most terminals take.
        if copied {
            self.info(format!("Copied {url}"));
        } else if copy_through_terminal(&url).is_ok() {
            self.info(format!("Copied {url} (through the terminal)"));
        } else {
            self.error("Couldn't copy: there's no clipboard here");
        }
    }

    // Settings.

    /// Saves one setting in settings.toml (as the file is now) and takes
    /// the file's settings, so what was written there by hand meanwhile
    /// applies too.
    fn save_setting(&mut self, key: &str, value: toml::Value) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        match Settings::save_one(&path, key, value) {
            Ok(settings) => {
                self.settings = settings;
                self.colors = Colors::named(&self.settings.theme);
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn toggle_list(&mut self) {
        self.settings.list = !self.settings.list;
        self.save_setting("list", self.settings.list.into());
        self.scroll = 0;
    }

    fn toggle_unread_only(&mut self) {
        self.settings.unread_only = !self.settings.unread_only;
        self.save_setting("unread_only", self.settings.unread_only.into());
        self.read_here.clear();
        self.reload();
        self.info(if self.settings.unread_only {
            "Only unread articles: u shows them all again"
        } else {
            "All articles, read and unread"
        });
    }

    /// Enter or Space on a row of the settings tab.
    fn change_setting(&mut self, row: usize) {
        match row {
            HelpMenu::SUMMARIES => {
                self.settings.summaries = !self.settings.summaries;
                self.save_setting("summaries", self.settings.summaries.into());
            }
            HelpMenu::LIST => self.toggle_list(),
            HelpMenu::UNREAD_ONLY => self.toggle_unread_only(),
            HelpMenu::FULL_TEXT => {
                self.settings.full_text = !self.settings.full_text;
                self.save_setting("full_text", self.settings.full_text.into());
            }
            HelpMenu::HISTORY => {
                self.settings.history = !self.settings.history;
                self.save_setting("history", self.settings.history.into());
            }
            HelpMenu::ANIMATIONS => {
                self.settings.animations = !self.settings.animations;
                self.save_setting("animations", self.settings.animations.into());
            }
            row if row >= HelpMenu::THEMES => {
                if let Some((id, _)) = theme::BUILT_IN.get(row - HelpMenu::THEMES) {
                    self.settings.theme = (*id).to_string();
                    self.colors = Colors::named(id);
                    self.save_setting("theme", (*id).into());
                }
            }
            _ => {}
        }
    }

    // Keys.

    fn on_terminal_events(&mut self, events: Vec<Event>) {
        let presses: Vec<KeyEvent> = events
            .iter()
            .filter_map(|event| match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => Some(*key),
                _ => None,
            })
            .collect();
        let repeated = presses.windows(2).all(|w| w[0].code == w[1].code);
        if presses.len() < 2 || repeated {
            return events.into_iter().for_each(|event| self.on_terminal(event));
        }
        for event in events.into_iter().filter(|e| !matches!(e, Event::Key(_))) {
            self.on_terminal(event);
        }
        let typed = |key: &KeyEvent| {
            matches!(key.code, KeyCode::Char(_))
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        };
        let text: String = presses
            .iter()
            .filter_map(|key| match key.code {
                KeyCode::Char(c) if typed(key) => Some(c),
                _ => None,
            })
            .collect();
        if !self.typing() {
            // Keys that only move (arrows, Enter, Backspace) are no paste's
            // when they come alone: they came at once over a slow link. With
            // anything else it's a paste, Ctrl-J (a pasted line's end in raw
            // mode) and Tab too.
            let moves = |key: &KeyEvent| {
                matches!(
                    key.code,
                    KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::PageUp
                        | KeyCode::PageDown
                        | KeyCode::Home
                        | KeyCode::End
                        | KeyCode::Enter
                        | KeyCode::Esc
                        | KeyCode::Backspace
                        | KeyCode::BackTab
                )
            };
            if presses.len() >= PASTED_KEYS && !presses.iter().all(moves) {
                return self.on_terminal(Event::Paste(text));
            }
            return presses.into_iter().for_each(|key| self.on_key(key));
        }
        // A key that edits (Backspace, an arrow, Esc) is no paste's: it's
        // typing that came all at once (a slow link), so the keys run as
        // typed, and nothing once the box closes.
        let edits = |key: &KeyEvent| !typed(key) && key.code != KeyCode::Enter;
        if presses.iter().any(edits) {
            for key in presses {
                if !self.typing() {
                    break;
                }
                self.on_key(key);
            }
            return;
        }
        let (last, before) = presses.split_last().expect("two or more");
        let submit = last.code == KeyCode::Enter && before.iter().all(typed);
        self.on_terminal(Event::Paste(text));
        if submit {
            self.on_key(*last);
        }
    }

    /// Whether a line is being typed: in the search or add box, or a
    /// category's name.
    fn typing(&self) -> bool {
        self.prompt.is_some()
            || self
                .category_menu
                .as_ref()
                .is_some_and(|m| m.naming.is_some())
    }

    fn on_terminal(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.on_key(key),
            Event::Paste(text) => {
                // Cut before cleaning, so a huge paste costs nothing.
                let cut = text::first_chars(&text, 2 * MAX_PROMPT);
                let line = article::one_line(cut, MAX_PROMPT);
                // In a box, a space it had at either end is kept where a word
                // next to it would run into it otherwise.
                let spaced = (
                    cut.starts_with(char::is_whitespace),
                    cut.ends_with(char::is_whitespace),
                );
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.typed.paste(&line, spaced, MAX_PROMPT);
                } else if let Some(naming) =
                    self.category_menu.as_mut().and_then(|m| m.naming.as_mut())
                {
                    naming.typed.paste(&line, spaced, store::MAX_CATEGORY_NAME);
                } else if !self.popup_open() {
                    // A link pasted with no box open: the box to add it.
                    let pasted = line;
                    if pasted.contains('.') && !pasted.contains(' ') {
                        self.open_add_feed(pasted);
                    } else {
                        self.info(
                            "Pasted text goes in the search box (/) or the box to add a feed (a)",
                        );
                    }
                }
            }
            _ => {}
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.prompt.is_some() {
            return self.on_prompt_key(key);
        }
        // `y` answers yes; any other key, no.
        if let Some(confirm) = self.confirm.take() {
            if key.code == KeyCode::Char('y') && !ctrl {
                match confirm {
                    Confirm::RemoveFeed(id, name) => self.remove_feed(id, &name),
                    Confirm::DeleteCategory(id, name) => self.delete_category(id, &name),
                    Confirm::TakeOff(list, id, _) => self.take_off(list, &id),
                    Confirm::MarkAllRead(ids) => {
                        let n = ids.len();
                        self.mark_read(&ids);
                        self.info(format!("Marked {n} read"));
                    }
                }
            }
            return;
        }
        if self.help.is_some() {
            return self.on_help_key(key, ctrl);
        }
        if self.category_menu.is_some() {
            return self.on_category_key(key);
        }
        if key.code == KeyCode::Char('F') && !ctrl {
            return self.open_categories();
        }
        if self.reader.is_some() {
            return self.on_reader_key(key, ctrl);
        }
        if std::mem::take(&mut self.pending_g) && key.code == KeyCode::Char('g') {
            return self.jump_to(0);
        }
        if let Some(way) = toward(&key) {
            return self.go_toward(way);
        }
        // Letters act here only without Ctrl: Ctrl-u and Ctrl-d move half a
        // page in the cards and the sidebar.
        match key.code {
            KeyCode::Char('q') if !ctrl => self.quit = true,
            KeyCode::Char('?') if !ctrl => self.help = Some(HelpMenu::new(HelpTab::Keys)),
            KeyCode::Char('O') if !ctrl => self.help = Some(HelpMenu::new(HelpTab::Settings)),
            KeyCode::Char('/' | 's') if !ctrl => self.open_search(),
            KeyCode::Char('a') if !ctrl => self.open_add_feed(String::new()),
            KeyCode::Char('R') if !ctrl => self.refresh_now(),
            KeyCode::Char('v') if !ctrl => self.toggle_list(),
            KeyCode::Char('u') if !ctrl => self.toggle_unread_only(),
            KeyCode::Char('M') if !ctrl => self.ask_mark_all_read(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Grid,
                    Focus::Grid => Focus::Sidebar,
                };
            }
            KeyCode::Backspace => self.go_back(),
            KeyCode::Char('o') if ctrl => self.go_back(),
            _ => match self.focus {
                Focus::Sidebar => self.on_sidebar_key(key),
                Focus::Grid => self.on_grid_key(key),
            },
        }
    }

    fn on_prompt_key(&mut self, key: KeyEvent) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                let prompt = self.prompt.take().expect("open");
                if prompt.kind == PromptKind::Search {
                    self.search_typed = Some(prompt.typed);
                }
            }
            // To the articles, keeping what was typed.
            KeyCode::Char('j') | KeyCode::Down if ctrl => {
                let prompt = self.prompt.take().expect("open");
                if prompt.kind == PromptKind::Search {
                    self.search_typed = Some(prompt.typed);
                }
                self.focus = Focus::Grid;
            }
            KeyCode::Enter => {
                let prompt = self.prompt.take().expect("open");
                let typed = prompt.typed.text().trim().to_string();
                if typed.is_empty() {
                    return;
                }
                match prompt.kind {
                    PromptKind::Search => {
                        self.search_typed = None;
                        self.show(View::Search(typed));
                        self.sync_sidebar();
                        self.focus = Focus::Grid;
                    }
                    PromptKind::AddFeed => self.add_feed(&typed),
                }
            }
            _ => {
                prompt.typed.edit(&key, MAX_PROMPT);
            }
        }
    }

    fn on_help_key(&mut self, key: KeyEvent, ctrl: bool) {
        let Some(menu) = self.help.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | '?') => self.help = None,
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Char('h' | 'l')
            | KeyCode::Left
            | KeyCode::Right => {
                menu.tab = match menu.tab {
                    HelpTab::Keys => HelpTab::Settings,
                    HelpTab::Settings => HelpTab::Keys,
                };
            }
            KeyCode::Char('O') => menu.tab = HelpTab::Settings,
            KeyCode::Enter | KeyCode::Char(' ') if menu.tab == HelpTab::Settings => {
                let row = menu.selected;
                self.change_setting(row);
            }
            _ => {
                let delta: isize = match key.code {
                    KeyCode::Char('j') | KeyCode::Down => 1,
                    KeyCode::Char('k') | KeyCode::Up => -1,
                    KeyCode::Char('d') if ctrl => HELP_PAGE,
                    KeyCode::Char('u') if ctrl => -HELP_PAGE,
                    KeyCode::PageDown => 2 * HELP_PAGE,
                    KeyCode::PageUp => -2 * HELP_PAGE,
                    KeyCode::Char('g') | KeyCode::Home => isize::MIN / 2,
                    KeyCode::Char('G') | KeyCode::End => isize::MAX / 2,
                    _ => return,
                };
                match menu.tab {
                    // Drawing stops it at the end.
                    HelpTab::Keys => menu.scroll = menu.scroll.saturating_add_signed(delta),
                    HelpTab::Settings => {
                        menu.selected = menu
                            .selected
                            .saturating_add_signed(delta)
                            .min(HelpMenu::LAST);
                    }
                }
            }
        }
    }

    fn on_reader_key(&mut self, key: KeyEvent, ctrl: bool) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let page = reader.page.get().max(1);
        let max = reader.max_scroll.get();
        let mut scroll_by = |delta: isize| {
            reader.scroll = reader.scroll.saturating_add_signed(delta).min(max);
        };
        match key.code {
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('q' | 'h') | KeyCode::Left => {
                self.close_reader();
            }
            KeyCode::Char('o') if ctrl => self.close_reader(),
            KeyCode::Char('j') | KeyCode::Down => scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => scroll_by(-1),
            KeyCode::Char('d') if ctrl => scroll_by(page as isize / 2),
            KeyCode::Char('u') if ctrl => scroll_by(-(page as isize / 2)),
            KeyCode::Char(' ') | KeyCode::PageDown | KeyCode::Char('f') if !ctrl => {
                scroll_by(page.saturating_sub(2).max(1) as isize);
            }
            KeyCode::Char('b') | KeyCode::PageUp => {
                scroll_by(-(page.saturating_sub(2).max(1) as isize))
            }
            KeyCode::Char('g') | KeyCode::Home => reader.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => reader.scroll = max,
            KeyCode::Char(']' | 'n' | 'J') => self.read_next(true),
            KeyCode::Char('[' | 'p' | 'K') => self.read_next(false),
            KeyCode::Char('o') | KeyCode::Enter => self.open_in_browser(),
            KeyCode::Char('w') => self.toggle_saved(),
            KeyCode::Char('m') => self.toggle_read(),
            KeyCode::Char('y') => self.copy_link(),
            KeyCode::Char('?') => self.help = Some(HelpMenu::new(HelpTab::Keys)),
            KeyCode::Char('O') => self.help = Some(HelpMenu::new(HelpTab::Settings)),
            _ => {}
        }
    }

    /// Ctrl and an arrow or `h j k l`: the pane that way.
    fn go_toward(&mut self, way: Toward) {
        match (self.focus, way) {
            (Focus::Sidebar, Toward::Right) => self.focus = Focus::Grid,
            (Focus::Grid, Toward::Left) => self.focus = Focus::Sidebar,
            (_, Toward::Up) => self.open_search(),
            _ => {}
        }
    }

    /// The search box, with what was typed in it when it was left, else
    /// the search shown.
    fn open_search(&mut self) {
        let typed = self.search_typed.take().unwrap_or_else(|| {
            Typed::new(match &self.view {
                View::Search(q) => q.clone(),
                _ => String::new(),
            })
        });
        self.prompt = Some(Prompt::new(PromptKind::Search, typed));
    }

    /// Opens a sidebar row: its articles, or the search box.
    fn open_entry(&mut self, entry: Entry) {
        if entry == Entry::Search && !matches!(self.view, View::Search(_)) {
            return self.open_search();
        }
        if let Some(view) = self.entry_view(entry) {
            self.show(view);
        }
        self.focus = Focus::Grid;
    }

    fn on_sidebar_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.sidebar_len().saturating_sub(1);
        let moved = match key.code {
            KeyCode::Char('d') if ctrl => Some((self.sidebar_selected + SIDEBAR_PAGE).min(last)),
            KeyCode::Char('u') if ctrl => Some(self.sidebar_selected.saturating_sub(SIDEBAR_PAGE)),
            KeyCode::PageDown => Some((self.sidebar_selected + 2 * SIDEBAR_PAGE).min(last)),
            KeyCode::PageUp => Some(self.sidebar_selected.saturating_sub(2 * SIDEBAR_PAGE)),
            KeyCode::Char('j') | KeyCode::Down => Some((self.sidebar_selected + 1).min(last)),
            KeyCode::Char('k') | KeyCode::Up => Some(self.sidebar_selected.saturating_sub(1)),
            KeyCode::Char('G') | KeyCode::End => Some(last),
            KeyCode::Home => Some(0),
            KeyCode::Char('g') => {
                self.pending_g = true;
                None
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                self.open_entry(self.entry(self.sidebar_selected));
                None
            }
            KeyCode::Char('x' | 'd') | KeyCode::Delete if !ctrl => {
                match self.entry(self.sidebar_selected) {
                    Entry::Feed(i) => {
                        if let Some(feed) = self.listed_nth(i) {
                            self.confirm = Some(Confirm::RemoveFeed(feed.id, feed.name()));
                        }
                    }
                    Entry::Category(i) => {
                        if let Some(category) = self.categories.get(i) {
                            self.confirm =
                                Some(Confirm::DeleteCategory(category.id, category.name.clone()));
                        }
                    }
                    _ => {}
                }
                None
            }
            _ => None,
        };
        if let Some(i) = moved
            && i != self.sidebar_selected
        {
            self.sidebar_selected = i;
            // Moving shows what's kept at once.
            if let Some(view) = self.entry_view(self.entry(i)) {
                self.show(view);
            }
        }
    }

    fn jump_to(&mut self, i: usize) {
        match self.focus {
            Focus::Sidebar => {
                self.sidebar_selected = i.min(self.sidebar_len().saturating_sub(1));
                if let Some(view) = self.entry_view(self.entry(self.sidebar_selected)) {
                    self.show(view);
                }
            }
            Focus::Grid => self.select(i),
        }
    }

    fn select(&mut self, i: usize) {
        self.selected = i.min(self.articles.len().saturating_sub(1));
    }

    fn on_grid_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let GridShape { cols, rows } = self.grid.get();
        let cols = cols.max(1);
        let page = rows.max(1) * cols;
        let last = self.articles.len().saturating_sub(1);
        match key.code {
            // Left from a row's first card goes to the row before's last,
            // right from its last to the next row's first.
            KeyCode::Char('h') | KeyCode::Left if cols > 1 => {
                self.select(self.selected.saturating_sub(1));
            }
            KeyCode::Char('h') | KeyCode::Left => self.focus = Focus::Sidebar,
            KeyCode::Char('l') | KeyCode::Right => self.select((self.selected + 1).min(last)),
            KeyCode::Char('j') | KeyCode::Down => {
                // Into a shorter last row: its last card.
                if self.selected / cols < last / cols {
                    self.select((self.selected + cols).min(last));
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.selected >= cols {
                    self.select(self.selected - cols);
                }
            }
            KeyCode::Char('d') if ctrl => self.select((self.selected + page / 2).min(last)),
            KeyCode::Char('u') if ctrl => self.select(self.selected.saturating_sub(page / 2)),
            KeyCode::PageDown => self.select((self.selected + page).min(last)),
            KeyCode::PageUp => self.select(self.selected.saturating_sub(page)),
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Home => self.select(0),
            KeyCode::Char('G') | KeyCode::End => self.select(last),
            KeyCode::Enter => self.open_selected(),
            KeyCode::Char('o') => self.open_in_browser(),
            KeyCode::Char('y') => self.copy_link(),
            KeyCode::Char('w') => self.toggle_saved(),
            KeyCode::Char('m') => self.toggle_read(),
            KeyCode::Char('x') | KeyCode::Delete => self.ask_to_remove(),
            KeyCode::Esc => self.focus = Focus::Sidebar,
            _ => {}
        }
    }
}

/// Opens `url` (a link `article::checked_link` passed) in the browser.
/// The opener gets it on its command line: browsers take URLs no other way.
fn open_url(url: &str) -> std::io::Result<()> {
    if cfg!(any(target_os = "macos", windows)) {
        return open::that_detached(url);
    }
    let opener = crate::tools::on_path("xdg-open").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "xdg-open isn't installed")
    })?;
    // xdg-open runs more programs by name (xprop, gio, x-www-browser…). They
    // come from PATH's absolute folders only (main keeps no other), and it
    // runs in the home folder, not the one tuinews was started from. The
    // rest of the environment stays: it decides which browser opens.
    let mut command = std::process::Command::new(opener);
    command
        .arg(url)
        .current_dir(dirs::home_dir().unwrap_or_else(|| "/".into()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Its own process group, so the terminal's signals don't reach the
    // browser it starts.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = {
        let _starting = crate::tools::starting();
        command.spawn()?
    };
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Asks the terminal to put `text` on the clipboard (OSC 52). Only ever a
/// checked link.
fn copy_through_terminal(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

/// Standard base64, with padding.
fn base64(data: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(CHARS[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Turns SIGHUP (the terminal window closed) and SIGTERM into a normal quit,
/// which closes the database.
#[cfg(unix)]
fn quit_on_hangup(tx: UnboundedSender<AppEvent>) -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut hangup = signal(SignalKind::hangup())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::spawn(async move {
        tokio::select! {
            _ = hangup.recv() => {}
            _ = terminate.recv() => {}
        }
        let _ = tx.send(AppEvent::Quit);
    });
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use ratatui_image::picker::Picker;

    /// An app on an in-memory store with the default feeds, three articles
    /// in the first, and nothing fetched.
    pub fn app() -> App {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(rx);
        let mut store = Store::in_memory();
        store.seed_defaults().unwrap();
        let feeds = store.feeds().unwrap();
        let items: Vec<feed::Item> = (1..=3)
            .map(|n| feed::Item {
                title: format!("Tin số {n}: một tiêu đề khá dài để xuống dòng"),
                link: format!("https://vnexpress.net/tin-{n}.html"),
                summary: format!("Tóm tắt của tin số {n}."),
                image: Some(format!("https://i1-vnexpress.vnecdn.net/{n}.jpg")),
                published: Some(1_000 * i64::from(n)),
                ..feed::Item::default()
            })
            .collect();
        store
            .save_articles(feeds[0].id, "VnExpress", &items, 10_000)
            .unwrap();
        let mut images = Images::new(Picker::halfblocks(), tx.clone(), feed::client(), None);
        images.go_offline();
        App::new(store, Settings::default(), None, feed::client(), tx, images)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    /// Presses `j` `n` times.
    fn down(app: &mut App, n: usize) {
        for _ in 0..n {
            app.on_key(key(KeyCode::Char('j')));
        }
    }

    /// Items of a section feed, about cars.
    fn cars_items() -> Vec<feed::Item> {
        (1..=2)
            .map(|n| feed::Item {
                title: format!("Xe số {n}"),
                link: format!("https://vnexpress.net/xe-{n}.html"),
                published: Some(5_000 + i64::from(n)),
                ..feed::Item::default()
            })
            .collect()
    }

    #[test]
    fn home_lists_the_newest_first_and_the_sidebar_shows_feeds_as_you_move() {
        let mut app = app();
        assert_eq!(app.view, View::Home);
        assert!(app.articles[0].title.starts_with("Tin số 3"));
        let categories = crate::defaults::CATEGORIES.len();
        assert_eq!(
            app.sidebar_len(),
            MENU.len() + categories + crate::defaults::FEEDS.len()
        );
        // Down past Search, Saved and History to the first category.
        down(&mut app, 4);
        assert!(matches!(&app.view, View::Category(_, name) if name == "Tech"));
        assert!(app.articles.is_empty(), "its sections not fetched");
        // Past the categories, the first feed.
        down(&mut app, categories);
        assert_eq!(app.view, View::Feed(app.feeds[0].id, "VnExpress".into()));
        assert_eq!(app.articles.len(), 3);
        down(&mut app, 1);
        assert!(app.articles.is_empty(), "Tuổi Trẻ, not fetched");
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.articles.len(), 3, "back");
    }

    #[test]
    fn a_category_shows_its_sections_articles_and_home_doesnt() {
        let mut app = app();
        let section = app
            .feeds
            .iter()
            .find(|f| f.url == "https://vnexpress.net/rss/oto-xe-may.rss")
            .unwrap()
            .clone();
        assert!(!section.listed);
        app.store
            .save_articles(section.id, "VnExpress", &cars_items(), 10_000)
            .unwrap();
        app.reload();
        assert_eq!(app.articles.len(), 3, "Home: not the section's");
        assert_eq!(app.home_unread, 3);
        down(&mut app, 5);
        assert!(matches!(&app.view, View::Category(_, name) if name == "Cars"));
        let titles: Vec<&str> = app.articles.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["Xe số 2", "Xe số 1"]);
        assert_eq!(app.categories[1].unread, 2);
        app.focus = Focus::Grid;
        app.settings.full_text = false;
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.categories[1].unread, 1, "read in it");
    }

    #[test]
    fn f_puts_a_feed_in_categories_and_makes_renames_and_deletes_them() {
        let mut app = app();
        let vnexpress = app.feeds[0].id;
        down(&mut app, 4 + crate::defaults::CATEGORIES.len());
        press(&mut app, "F");
        let menu = app.category_menu.as_ref().expect("open");
        assert_eq!(menu.feed.as_ref().map(|f| f.0), Some(vnexpress));
        // Enter on Tech: VnExpress in it, then out again.
        app.on_key(key(KeyCode::Enter));
        assert!(app.categories[0].feeds.contains(&vnexpress));
        app.on_key(key(KeyCode::Enter));
        assert!(!app.categories[0].feeds.contains(&vnexpress));
        // A new one, with the feed in it.
        press(&mut app, "n");
        press(&mut app, "Xe điện");
        app.on_key(key(KeyCode::Enter));
        let made = app.categories.last().unwrap().clone();
        assert_eq!(made.name, "Xe điện");
        assert_eq!(made.feeds, [vnexpress]);
        assert_eq!(
            app.category_menu.as_ref().unwrap().selected,
            app.categories.len() - 1,
            "on the one made"
        );
        // A name in use stays to be changed.
        press(&mut app, "n");
        press(&mut app, "tech");
        app.on_key(key(KeyCode::Enter));
        assert!(app.status.as_ref().unwrap().error);
        assert!(app.category_menu.as_ref().unwrap().naming.is_some());
        app.on_key(key(KeyCode::Esc));
        // Renamed, then deleted: the feed stays, under Feeds.
        press(&mut app, "kr");
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        press(&mut app, "EV");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.categories.last().unwrap().name, "EV");
        press(&mut app, "x");
        assert!(matches!(&app.confirm, Some(Confirm::DeleteCategory(_, n)) if n == "EV"));
        press(&mut app, "y");
        assert_eq!(app.categories.len(), crate::defaults::CATEGORIES.len());
        assert!(app.feeds.iter().any(|f| f.id == vnexpress));
        app.on_key(key(KeyCode::Esc));
        assert!(app.category_menu.is_none());
    }

    #[test]
    fn a_feed_added_in_a_category_goes_only_there() {
        let mut app = app();
        down(&mut app, 5);
        let View::Category(cars, _) = app.view.clone() else {
            panic!("Cars")
        };
        let found = feed::Feed {
            title: "Xe 360".into(),
            items: cars_items(),
        };
        app.on_found(
            "https://xe360.vn/rss",
            Some(cars),
            Ok(("https://xe360.vn/rss".into(), found)),
        );
        let added = app
            .feeds
            .iter()
            .find(|f| f.url == "https://xe360.vn/rss")
            .unwrap();
        assert!(!added.listed, "not under Feeds");
        assert!(app.categories[1].feeds.contains(&added.id));
        assert_eq!(app.articles.len(), 2, "shown in Cars");
        assert_eq!(app.home_unread, 3, "not in Home");
        // Deleting Cars from the sidebar removes it, the section with it.
        app.focus = Focus::Sidebar;
        press(&mut app, "xy");
        assert!(app.feeds.iter().all(|f| f.url != "https://xe360.vn/rss"));
        assert_eq!(app.view, View::Home);
    }

    #[test]
    fn at_the_top_the_newest_stays_selected_as_feeds_come_in() {
        let mut app = app();
        let feed = app.feeds[1].id;
        let newer = feed::Item {
            title: "Tin mới hơn".into(),
            link: "https://tuoitre.vn/moi.html".into(),
            published: Some(9_000),
            ..feed::Item::default()
        };
        app.store
            .save_articles(feed, "Tuổi Trẻ", &[newer], 10_000)
            .unwrap();
        app.reload();
        assert_eq!(app.selected, 0);
        assert_eq!(app.articles[0].title, "Tin mới hơn");
        // Further down, the article selected stays so.
        app.selected = 2;
        let id = app.articles[2].id.clone();
        let newest = feed::Item {
            title: "Mới nhất".into(),
            link: "https://tuoitre.vn/moi-nhat.html".into(),
            published: Some(9_500),
            ..feed::Item::default()
        };
        app.store
            .save_articles(feed, "Tuổi Trẻ", &[newest], 10_000)
            .unwrap();
        app.reload();
        assert_eq!(app.articles[app.selected].id, id);
    }

    #[test]
    fn opening_an_article_reads_it_and_keeps_it_in_history() {
        let mut app = app();
        app.settings.full_text = false;
        app.focus = Focus::Grid;
        app.on_key(key(KeyCode::Enter));
        let reader = app.reader.as_ref().expect("open");
        assert_eq!(reader.body, Body::Summary);
        let id = reader.article.id.clone();
        assert!(app.is_read(&id));
        assert_eq!(app.feeds[0].unread, 2);
        press(&mut app, "]");
        assert!(
            app.reader
                .as_ref()
                .unwrap()
                .article
                .title
                .starts_with("Tin số 2")
        );
        app.on_key(key(KeyCode::Esc));
        assert!(app.reader.is_none());
        app.reload();
        app.show(View::History);
        assert_eq!(app.articles.len(), 2);
        assert!(
            app.articles[0].title.starts_with("Tin số 2"),
            "the last opened first"
        );
    }

    #[test]
    fn only_unread_keeps_what_you_read_here_until_you_leave() {
        let mut app = app();
        app.settings.full_text = false;
        app.focus = Focus::Grid;
        press(&mut app, "u");
        assert_eq!(app.articles.len(), 3);
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Esc));
        app.reload();
        assert_eq!(app.articles.len(), 3, "read here: still shown");
        app.show(View::Saved);
        app.go_back();
        assert_eq!(app.articles.len(), 2, "gone once you've left");
    }

    #[test]
    fn saving_asks_before_unsaving_and_x_takes_off_saved() {
        let mut app = app();
        app.focus = Focus::Grid;
        press(&mut app, "w");
        let id = app.articles[0].id.clone();
        assert!(app.saved.contains(&id));
        press(&mut app, "w");
        assert!(matches!(
            app.confirm,
            Some(Confirm::TakeOff(Kept::Saved, ..))
        ));
        press(&mut app, "n");
        assert!(app.saved.contains(&id), "kept: any key but y");
        app.show(View::Saved);
        assert_eq!(app.articles.len(), 1);
        press(&mut app, "xy");
        assert!(app.articles.is_empty());
        assert!(!app.saved.contains(&id));
    }

    #[test]
    fn marking_all_read_asks_first() {
        let mut app = app();
        app.focus = Focus::Grid;
        press(&mut app, "M");
        assert!(matches!(app.confirm, Some(Confirm::MarkAllRead(ref ids)) if ids.len() == 3));
        press(&mut app, "y");
        assert_eq!(app.home_unread, 0);
        press(&mut app, "m");
        assert_eq!(app.home_unread, 1, "m: unread again");
    }

    #[test]
    fn removing_a_feed_asks_and_shows_home_if_it_was_shown() {
        let mut app = app();
        down(&mut app, 4 + crate::defaults::CATEGORIES.len());
        press(&mut app, "x");
        assert!(matches!(app.confirm, Some(Confirm::RemoveFeed(_, ref n)) if n == "VnExpress"));
        press(&mut app, "y");
        assert_eq!(app.view, View::Home);
        assert_eq!(app.listed().count(), crate::defaults::FEEDS.len() - 1);
        assert!(app.articles.is_empty());
    }

    #[test]
    fn the_search_box_edits_where_its_cursor_is() {
        let mut app = app();
        press(&mut app, "/ha noi");
        let text = |app: &App| app.prompt.as_ref().unwrap().typed.text().to_string();
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Left));
        press(&mut app, "x");
        assert_eq!(text(&app), "ha nxoi");
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Delete));
        assert_eq!(text(&app), "ha ni");
        // A paste goes in at the cursor too.
        app.on_key(key(KeyCode::Home));
        app.on_terminal(Event::Paste("tin".into()));
        press(&mut app, " ");
        assert_eq!(text(&app), "tin ha ni");
        app.on_key(key(KeyCode::End));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(text(&app), "tin ha ");
        // Out to the articles and back: the cursor is where it was.
        app.on_key(key(KeyCode::Home));
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert!(app.prompt.is_none());
        press(&mut app, "/x");
        assert_eq!(text(&app), "xtin ha ");
        // A paste between words keeps its spaces.
        app.on_key(key(KeyCode::Home));
        app.on_key(key(KeyCode::Right));
        app.on_terminal(Event::Paste(" mới ".into()));
        assert_eq!(text(&app), "x mới tin ha ");
        // ← steps over a letter written as two characters (e and its accent),
        // and Delete takes it whole; Backspace takes the accent first, as an
        // input method that retypes a letter expects.
        let mut line = Typed::new("cafe\u{301}!".into());
        for _ in 0..2 {
            line.edit(&key(KeyCode::Left), 50);
        }
        assert_eq!(&line.text()[..line.at()], "caf");
        line.edit(&key(KeyCode::Delete), 50);
        assert_eq!(line.text(), "caf!");
        let mut line = Typed::new("cafe\u{301}".into());
        line.edit(&key(KeyCode::Backspace), 50);
        assert_eq!(line.text(), "cafe");
        // Option-← (Alt-b, or Alt with ←) goes back a word, Option-Backspace
        // deletes one; an Alt letter isn't typed, nor does Ctrl-← move.
        let alt = |code| KeyEvent::new(code, KeyModifiers::ALT);
        let mut line = Typed::new("tin ha noi".into());
        line.edit(&alt(KeyCode::Char('b')), 50);
        assert_eq!(&line.text()[line.at()..], "noi");
        line.edit(&alt(KeyCode::Left), 50);
        assert_eq!(&line.text()[line.at()..], "ha noi");
        line.edit(&alt(KeyCode::Char('f')), 50);
        assert_eq!(&line.text()[line.at()..], " noi");
        // An Alt letter isn't typed, nor does Ctrl-← move; AltGr (Ctrl and
        // Alt) types its letter, Cmd-← and Cmd-→ (Ctrl-a, Ctrl-e) go to
        // either end, and Cmd-Backspace (Ctrl-u) deletes all before.
        line.edit(&alt(KeyCode::Char('x')), 50);
        line.edit(&KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL), 50);
        assert_eq!(&line.text()[line.at()..], " noi");
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        line.edit(&KeyEvent::new(KeyCode::Char('ą'), altgr), 50);
        assert_eq!(line.text(), "tin haą noi");
        line.edit(
            &KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            50,
        );
        assert_eq!(line.at(), 0);
        line.edit(
            &KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            50,
        );
        assert_eq!(line.at(), line.text().len());
        line.edit(&alt(KeyCode::Char('b')), 50);
        line.edit(
            &KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            50,
        );
        assert_eq!(line.text(), "noi");
        // Backspace takes a flag whole: it's one letter.
        let mut flags = Typed::new("🇻🇳🇺🇸".into());
        flags.edit(&key(KeyCode::Left), 50);
        flags.edit(&key(KeyCode::Backspace), 50);
        assert_eq!(flags.text(), "🇺🇸");
        assert_eq!(flags.at(), 0);
        let mut line = Typed::new("tin ha noi".into());
        line.edit(&alt(KeyCode::Char('b')), 50);
        line.edit(&alt(KeyCode::Left), 50);
        line.edit(&alt(KeyCode::Char('f')), 50);
        line.edit(&alt(KeyCode::Backspace), 50);
        assert_eq!(line.text(), "tin  noi");
    }

    #[test]
    fn searching_finds_articles_without_their_accents() {
        let mut app = app();
        press(&mut app, "/");
        assert!(app.prompt.is_some());
        press(&mut app, "tin so 2");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.view, View::Search("tin so 2".into()));
        assert_eq!(app.articles.len(), 1);
        assert_eq!(app.focus, Focus::Grid);
    }

    #[test]
    fn a_paste_with_no_box_open_is_never_taken_for_keys() {
        let mut app = app();
        // A tab in it (a line copied from a table) is still a paste's, and
        // so is a line's end (Ctrl-J in raw mode).
        let pasted: Vec<Event> = "qx\ty\nMR"
            .chars()
            .map(|c| match c {
                '\t' => Event::Key(key(KeyCode::Tab)),
                '\n' => Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
                c => Event::Key(key(KeyCode::Char(c))),
            })
            .collect();
        app.on_terminal_events(pasted);
        assert!(!app.quit, "q in a paste doesn't quit");
        assert!(app.confirm.is_none());
        let link: Vec<Event> = "genk.vn/rss/home.rss"
            .chars()
            .map(|c| Event::Key(key(KeyCode::Char(c))))
            .collect();
        app.on_terminal_events(link);
        assert_eq!(
            app.prompt,
            Some(Prompt::new(
                PromptKind::AddFeed,
                Typed::new("genk.vn/rss/home.rss".into())
            ))
        );
    }

    #[test]
    fn keys_that_come_at_once_run_as_keys() {
        let mut app = app();
        app.focus = Focus::Grid;
        app.settings.full_text = false;
        let keys = [KeyCode::Down, KeyCode::Up, KeyCode::Enter];
        app.on_terminal_events(keys.into_iter().map(|k| Event::Key(key(k))).collect());
        assert!(app.reader.is_some(), "Enter opened the article");
    }

    #[test]
    fn adding_a_section_you_have_puts_it_under_feeds() {
        let mut app = app();
        let url = "https://vnexpress.net/rss/oto-xe-may.rss";
        assert!(app.listed().all(|f| f.url != url));
        app.add_feed(url);
        assert_eq!(
            app.listed().last().map(|f| f.url.as_str()),
            Some(url),
            "last"
        );
        assert!(app.status.as_ref().is_some_and(|s| !s.error));
    }

    #[test]
    fn base64_pads_as_the_terminal_wants() {
        assert_eq!(base64(b"https://a.vn"), "aHR0cHM6Ly9hLnZu");
        assert_eq!(base64(b"ab"), "YWI=");
    }
}
