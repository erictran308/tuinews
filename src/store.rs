//! What tuinews keeps, in SQLite (`tuinews.db` in the data folder): your
//! feeds and categories, the articles they brought (each linked to every
//! feed that brought it), which you've read, saved or opened, and articles'
//! text once read from their pages.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::article::{self, Article, ArticleId, FeedId};
use crate::html::Block;
use crate::settings::Blocks;
use crate::{config, defaults, feed, opml, text};

/// The newest articles Home shows, of all your feeds.
pub const HOME_SIZE: usize = 600;
/// A feed's newest articles shown.
pub const FEED_SIZE: usize = 300;
/// Articles searched, newest first.
pub const SEARCHED: usize = 5_000;
/// History's articles kept, newest first (up to [`HISTORY_SLACK`] more, till
/// it's cut back).
pub const HISTORY_SIZE: usize = 1_000;
/// History is cut back to [`HISTORY_SIZE`] once it's this many over, so the
/// checkpoint after comes once in so many articles opened, not with each.
const HISTORY_SLACK: usize = 50;
/// Feeds you may have, at most, those only in categories included.
pub const MAX_FEEDS: usize = 500;
/// Categories you may have, at most.
pub const MAX_CATEGORIES: usize = 100;
/// The longest name of a category, in characters.
pub const MAX_CATEGORY_NAME: usize = 40;
/// What the schema is at: 2 added categories, and articles linked to every
/// feed that brought them (`article_feeds`); 3 the article's date beside
/// each link, so a feed's newest are read off an index ([`newest`]).
const VERSION: i64 = 3;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS feeds (
    id INTEGER PRIMARY KEY,
    url TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL DEFAULT '',
    place INTEGER NOT NULL,
    fetched_at INTEGER,
    etag TEXT,
    last_modified TEXT,
    error TEXT,
    listed INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS categories (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    place INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS category_feeds (
    category_id INTEGER NOT NULL,
    feed_id INTEGER NOT NULL,
    PRIMARY KEY (category_id, feed_id)
);
CREATE INDEX IF NOT EXISTS category_feeds_by_feed ON category_feeds (feed_id);
CREATE TABLE IF NOT EXISTS article_feeds (
    article_id TEXT NOT NULL,
    feed_id INTEGER NOT NULL,
    -- The article's, as in articles.
    published INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (article_id, feed_id)
);
-- Its index by feed and date is made by migrate, once an older table has the
-- column.
CREATE TABLE IF NOT EXISTS articles (
    id TEXT PRIMARY KEY,
    feed_id INTEGER,
    source TEXT NOT NULL DEFAULT '',
    link TEXT NOT NULL,
    title TEXT NOT NULL,
    summary TEXT NOT NULL DEFAULT '',
    image TEXT,
    author TEXT NOT NULL DEFAULT '',
    category TEXT NOT NULL DEFAULT '',
    published INTEGER NOT NULL,
    seen_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS articles_by_feed ON articles (feed_id, published DESC);
CREATE INDEX IF NOT EXISTS articles_by_date ON articles (published DESC);
CREATE TABLE IF NOT EXISTS read (
    article_id TEXT PRIMARY KEY,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS saved (
    article_id TEXT PRIMARY KEY,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS history (
    article_id TEXT PRIMARY KEY,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS bodies (
    article_id TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
);
";

/// A feed of yours, with what the sidebar says of it.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedRow {
    pub id: FeedId,
    pub url: String,
    pub title: String,
    pub fetched_at: Option<i64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Why the last fetch failed, if it did.
    pub error: Option<String>,
    pub unread: usize,
    /// Shown under Feeds, and in Home; else it's a site's section that only
    /// shows in its categories.
    pub listed: bool,
}

pub type CategoryId = i64;

/// A category of yours: its feeds, and how many of their articles you
/// haven't read.
#[derive(Clone, Debug, PartialEq)]
pub struct CategoryRow {
    pub id: CategoryId,
    pub name: String,
    pub feeds: Vec<FeedId>,
    pub unread: usize,
}

/// What importing an OPML file did.
#[derive(Debug, Default, PartialEq)]
pub struct Imported {
    pub added: usize,
    /// Its feeds that were yours already.
    pub known: usize,
    /// Categories made for its folders.
    pub categories: usize,
    /// Feeds left out: over [`MAX_FEEDS`].
    pub over: usize,
    /// Folders no category was made for: over [`MAX_CATEGORIES`]. Their
    /// feeds go under Feeds.
    pub folders_over: usize,
}

impl FeedRow {
    /// Its name, or its address's host while it has none.
    pub fn name(&self) -> String {
        if !self.title.is_empty() {
            return self.title.clone();
        }
        host_name(&self.url).unwrap_or_else(|| self.url.clone())
    }
}

/// The host of `url`, without `www.`: what stands for a feed's name while
/// it has none.
pub fn host_name(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(|h| h.trim_start_matches("www.").to_string())
}

/// Which articles a view lists.
#[derive(Clone, Debug, PartialEq)]
pub enum Listing {
    /// Your feeds', those under Feeds: not the sections only in categories.
    Home,
    Feed(FeedId),
    Category(CategoryId),
    Saved,
    History,
    /// Words to look for, [`article::fold`]ed.
    Search(String),
}

pub struct Store {
    db: Connection,
    /// Articles whose title holds one of these aren't counted unread: the
    /// views leave them out ([`Store::hide`]). Those of them kept are in
    /// the connection's own `temp.hidden`, so counting needs no titles.
    hidden: Blocks,
}

const COLUMNS: &str = "a.id, a.feed_id, f.id IS NOT NULL, a.source, a.link, a.title, a.summary, \
                       a.image, a.author, a.category, a.published";

/// An article as stored, cleaned again (`text::clean`) as it's read: the
/// file may have been changed by something else. A row whose id or link
/// doesn't check out is skipped.
fn article_row(row: &rusqlite::Row) -> rusqlite::Result<Option<Article>> {
    let id: String = row.get(0)?;
    let link: String = row.get(4)?;
    let (Some(id), Some(link)) = (ArticleId::parse(&id), article::checked_link(&link, None)) else {
        return Ok(None);
    };
    let feed: Option<FeedId> = row.get(1)?;
    let feed_present: bool = row.get(2)?;
    let image: Option<String> = row.get(7)?;
    Ok(Some(Article {
        id,
        feed: feed.filter(|_| feed_present),
        source: article::one_line(&row.get::<_, String>(3)?, article::MAX_SOURCE),
        link,
        title: article::one_line(&row.get::<_, String>(5)?, article::MAX_TITLE),
        summary: article::lines(&row.get::<_, String>(6)?, article::MAX_SUMMARY),
        image: image.and_then(|i| article::checked_link(&i, None)),
        author: article::one_line(&row.get::<_, String>(8)?, article::MAX_BYLINE),
        category: article::one_line(&row.get::<_, String>(9)?, article::MAX_BYLINE),
        published: row.get(10)?,
    }))
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let shown = config::shown(path);
        let db = Connection::open(path).with_context(|| format!("cannot open {shown}"))?;
        // Deleted rows (an article taken off History) are overwritten, not
        // left readable in the file's free pages; under WAL that holds once
        // they're checkpointed, which removals do (`scrub`).
        db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA secure_delete = ON;")?;
        db.execute_batch(SCHEMA)
            .with_context(|| format!("cannot set up {shown}"))?;
        migrate(&db).with_context(|| format!("cannot update {shown}"))?;
        date_links(&db).with_context(|| format!("cannot update {shown}"))?;
        // The data folder is already yours alone; this is for a copy of the
        // database moved out of it.
        protect(path);
        Self::with(db).with_context(|| format!("cannot set up {shown}"))
    }

    /// The store over `db`, set up: what this connection keeps for itself.
    fn with(db: Connection) -> Result<Self> {
        db.execute_batch("CREATE TEMP TABLE IF NOT EXISTS hidden (article_id TEXT PRIMARY KEY);")?;
        Ok(Self {
            db,
            hidden: Blocks::default(),
        })
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        let db = Connection::open_in_memory().expect("SQLite opens in memory");
        db.execute_batch(SCHEMA).expect("the schema is valid");
        migrate(&db).expect("a new database is at the latest version");
        Self::with(db).expect("a temporary table can be made")
    }

    /// Puts the default feeds in, and the default categories with their
    /// sections, each the first time only: what you remove doesn't come
    /// back. Whether the feeds went in. On a first start, once you say so
    /// (`App.welcome`); after, for a database from before categories.
    pub fn seed_defaults(&mut self) -> Result<bool> {
        let tx = self.db.transaction()?;
        let done = |key: &str| -> Result<bool> {
            let value: Option<String> = tx
                .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
                .optional()?;
            Ok(value.is_some())
        };
        let (feeds_seeded, categories_seeded) = (done("seeded")?, done("categories_seeded")?);
        if !feeds_seeded {
            for (place, feed) in defaults::FEEDS.iter().enumerate() {
                tx.execute(
                    "INSERT OR IGNORE INTO feeds (url, title, place) VALUES (?1, ?2, ?3)",
                    params![feed.url, feed.name, place as i64],
                )?;
            }
            tx.execute("INSERT INTO meta (key, value) VALUES ('seeded', '1')", [])?;
        }
        if !categories_seeded {
            seed_categories(&tx)?;
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('categories_seeded', '1')",
                [],
            )?;
        }
        tx.commit()?;
        Ok(!feeds_seeded)
    }

    /// Whether nothing was ever put in: a new database, or one whose first
    /// start ended before you said what to start with (`App.welcome`).
    pub fn is_new(&self) -> Result<bool> {
        let seeded = self
            .db
            .prepare("SELECT 1 FROM meta WHERE key = 'seeded'")?
            .exists([])?;
        Ok(!seeded)
    }

    /// Starts with no feeds and no categories: the defaults never go in.
    pub fn start_empty(&mut self) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO meta (key, value)
             VALUES ('seeded', '1'), ('categories_seeded', '1')",
            [],
        )?;
        Ok(())
    }

    /// Articles whose title holds one of `blocks` aren't counted unread:
    /// those kept are found now, those to come as they come.
    pub fn hide(&mut self, blocks: Blocks) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM temp.hidden", [])?;
        if !blocks.is_empty() {
            let mut titles = tx.prepare("SELECT id, title FROM articles")?;
            let mut hide = tx.prepare("INSERT OR IGNORE INTO temp.hidden VALUES (?1)")?;
            let rows =
                titles.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, title) = row?;
                if blocks.hides(&article::one_line(&title, article::MAX_TITLE)) {
                    hide.execute([id])?;
                }
            }
        }
        tx.commit()?;
        // Only once they're all marked: articles to come are by these words.
        self.hidden = blocks;
        Ok(())
    }

    /// Every feed, those only in categories too, in the sidebar's order,
    /// with how many of the articles it shows you haven't read if it's
    /// under Feeds (a section's count shows nowhere).
    pub fn feeds(&self) -> Result<Vec<FeedRow>> {
        let mut statement = self.db.prepare(
            "SELECT id, url, title, fetched_at, etag, last_modified, error, listed
             FROM feeds ORDER BY place, id",
        )?;
        let rows = statement.query_map([], |row| {
            let url: String = row.get(1)?;
            let error: Option<String> = row.get(6)?;
            Ok(FeedRow {
                id: row.get(0)?,
                url,
                title: article::one_line(&row.get::<_, String>(2)?, article::MAX_SOURCE),
                fetched_at: row.get(3)?,
                etag: row.get(4)?,
                last_modified: row.get(5)?,
                error: error.map(|e| article::one_line(&e, 200)),
                unread: 0,
                listed: row.get(7)?,
            })
        })?;
        // A feed whose address no longer checks out is never fetched.
        let mut feeds: Vec<FeedRow> = rows
            .filter_map(Result::ok)
            .filter(|f| article::checked_link(&f.url, None).is_some())
            .collect();
        for feed in feeds.iter_mut().filter(|f| f.listed) {
            feed.unread = self.unread(&Listing::Feed(feed.id))?;
        }
        Ok(feeds)
    }

    /// Your categories, in the sidebar's order: their feeds, and how many of
    /// the articles each shows you haven't read (each once, if two of its
    /// feeds brought it).
    pub fn categories(&self) -> Result<Vec<CategoryRow>> {
        let mut feeds = self.db.prepare(
            "SELECT feed_id FROM category_feeds WHERE category_id = ?1 ORDER BY feed_id",
        )?;
        self.category_names()?
            .into_iter()
            .map(|(id, name)| {
                let ids = feeds
                    .query_map([id], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<FeedId>>>()?;
                Ok(CategoryRow {
                    id,
                    name,
                    feeds: ids,
                    unread: self.unread(&Listing::Category(id))?,
                })
            })
            .collect()
    }

    /// Your categories' names, in the sidebar's order, without counting
    /// what's in them.
    fn category_names(&self) -> Result<Vec<(CategoryId, String)>> {
        let mut statement = self
            .db
            .prepare("SELECT id, name FROM categories ORDER BY place, id")?;
        let rows = statement.query_map([], |row| {
            let name: String = row.get(1)?;
            Ok((row.get(0)?, article::one_line(&name, MAX_CATEGORY_NAME)))
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// How many of Home's articles you haven't read.
    pub fn home_unread(&self) -> Result<usize> {
        self.unread(&Listing::Home)
    }

    /// How many of the articles `listing` shows you haven't read, but those
    /// a blocked word hides, as the view leaves them out.
    fn unread(&self, listing: &Listing) -> Result<usize> {
        let Some((newest, param)) = newest(listing) else {
            return Ok(0);
        };
        let n: i64 = self
            .db
            .prepare_cached(&format!(
                "SELECT COUNT(*) FROM ({newest}) w
                 WHERE NOT EXISTS (SELECT 1 FROM read r WHERE r.article_id = w.article_id)
                   AND NOT EXISTS (SELECT 1 FROM temp.hidden h WHERE h.article_id = w.article_id)"
            ))?
            .query_row(rusqlite::params_from_iter(param), |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }

    /// The feeds that brought an article.
    pub fn feeds_of(&self, id: &ArticleId) -> Result<Vec<FeedId>> {
        let mut statement = self
            .db
            .prepare("SELECT feed_id FROM article_feeds WHERE article_id = ?1 ORDER BY feed_id")?;
        let ids = statement
            .query_map([id.as_str()], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<FeedId>>>()?;
        Ok(ids)
    }

    /// Adds the feed at `url` (checked) as the last one: under Feeds and in
    /// Home if `listed`, else only in the categories it's put in. Its id.
    pub fn add_feed(&mut self, url: &str, title: &str, listed: bool) -> Result<FeedId> {
        let count: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM feeds", [], |r| r.get(0))?;
        if count as usize >= MAX_FEEDS {
            bail!("that's {MAX_FEEDS} feeds already");
        }
        let exists: Option<FeedId> = self
            .db
            .query_row("SELECT id FROM feeds WHERE url = ?1", [url], |r| r.get(0))
            .optional()?;
        if exists.is_some() {
            bail!("that feed is yours already");
        }
        insert_feed(&self.db, url, title, listed)
    }

    /// Puts a feed that was only a section in categories under Feeds and in
    /// Home, last, as a feed added is.
    pub fn list_feed(&self, id: FeedId) -> Result<()> {
        let changed = list(&self.db, id)?;
        if changed == 0 {
            bail!("that feed is gone");
        }
        Ok(())
    }

    /// Adds an OPML file's folders as categories (yours by that name, in
    /// any case, else new ones), then its feeds under Feeds and in Home, each
    /// into its folder's category, all of them or none. A site's section, as
    /// `opml::write` marks one, goes only into its category. One that's
    /// yours already goes into the category too, and under Feeds if it was
    /// only a section and isn't marked one. Nothing is fetched here: new
    /// feeds are due at once.
    pub fn import(&mut self, opml: &opml::Opml) -> Result<Imported> {
        // Each's id, and whether it's under Feeds.
        let mut ids: HashMap<String, (FeedId, bool)> = self
            .db
            .prepare("SELECT url, id, listed FROM feeds")?
            .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
            .collect::<rusqlite::Result<_>>()?;
        let mut categories: HashMap<String, CategoryId> = self
            .category_names()?
            .into_iter()
            .map(|(id, name)| (name.to_lowercase(), id))
            .collect();
        let mut done = Imported::default();
        let tx = self.db.transaction()?;
        // A folder once, though two have its name.
        let mut no_room = HashSet::new();
        for folder in &opml.folders {
            let key = folder.to_lowercase();
            if categories.contains_key(&key) {
                continue;
            }
            if categories.len() >= MAX_CATEGORIES {
                no_room.insert(key);
                continue;
            }
            categories.insert(key, insert_category(&tx, folder)?);
            done.categories += 1;
        }
        done.folders_over = no_room.len();
        // A feed once, though it's in two folders.
        let mut counted = HashSet::new();
        let mut left_out = HashSet::new();
        for outline in &opml.feeds {
            let known = ids.get(&outline.url).copied();
            if known.is_none() && ids.len() >= MAX_FEEDS {
                left_out.insert(&outline.url);
                continue;
            }
            let category = outline
                .folder
                .as_ref()
                .and_then(|f| categories.get(&f.to_lowercase()).copied());
            let id = match known {
                Some((id, listed)) => {
                    if counted.insert(id) {
                        done.known += 1;
                    }
                    // A section of yours is in its categories already.
                    if !listed && !outline.section {
                        list(&tx, id)?;
                        ids.insert(outline.url.clone(), (id, true));
                    }
                    id
                }
                None => {
                    // A section with no category to go in would show nowhere.
                    let listed = !(outline.section && category.is_some());
                    let id = insert_feed(&tx, &outline.url, &outline.title, listed)?;
                    ids.insert(outline.url.clone(), (id, listed));
                    counted.insert(id);
                    done.added += 1;
                    id
                }
            };
            if let Some(category) = category {
                put_in(&tx, category, id)?;
            }
        }
        done.over = left_out.len();
        tx.commit()?;
        Ok(done)
    }

    /// Removes a feed, from its categories too, and its articles but those
    /// another feed brought too, or you saved or opened.
    pub fn remove_feed(&mut self, id: FeedId) -> Result<()> {
        let tx = self.db.transaction()?;
        forget_feed(&tx, id)?;
        tx.commit()?;
        self.tidy()?;
        self.scrub()
    }

    /// Makes a category, the last one. Its id.
    pub fn add_category(&mut self, name: &str) -> Result<CategoryId> {
        let name = self.category_name(name, None)?;
        let count: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM categories", [], |r| r.get(0))?;
        if count as usize >= MAX_CATEGORIES {
            bail!("that's {MAX_CATEGORIES} categories already");
        }
        insert_category(&self.db, &name)
    }

    pub fn rename_category(&mut self, id: CategoryId, name: &str) -> Result<()> {
        let name = self.category_name(name, Some(id))?;
        self.db.execute(
            "UPDATE categories SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// `name` as a category is called, if no other is called that (in any
    /// case, Unicode's: SQLite's `NOCASE` folds only ASCII).
    fn category_name(&self, name: &str, other_than: Option<CategoryId>) -> Result<String> {
        let name = article::one_line(name, MAX_CATEGORY_NAME);
        if name.is_empty() {
            bail!("a category needs a name");
        }
        let taken = self
            .category_names()?
            .into_iter()
            .any(|(id, n)| Some(id) != other_than && n.to_lowercase() == name.to_lowercase());
        if taken {
            bail!("there's a category called that already");
        }
        Ok(name)
    }

    /// Deletes a category, and the feeds that were only in it (a site's
    /// section that isn't under Feeds, nor in another category), with their
    /// articles but those you saved or opened. Their names.
    pub fn delete_category(&mut self, id: CategoryId) -> Result<Vec<String>> {
        let tx = self.db.transaction()?;
        let feeds: Vec<FeedId> = tx
            .prepare("SELECT feed_id FROM category_feeds WHERE category_id = ?1")?
            .query_map([id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        tx.execute("DELETE FROM category_feeds WHERE category_id = ?1", [id])?;
        tx.execute("DELETE FROM categories WHERE id = ?1", [id])?;
        let mut gone = Vec::new();
        for feed in feeds {
            if let Some(name) = forget_if_orphan(&tx, feed)? {
                gone.push(name);
            }
        }
        tx.commit()?;
        self.tidy()?;
        self.scrub()?;
        Ok(gone)
    }

    /// Puts a feed in a category, or takes it out. Taken out of its last
    /// category, a feed that isn't under Feeds is removed: its name, then.
    pub fn set_in_category(
        &mut self,
        category: CategoryId,
        feed: FeedId,
        on: bool,
    ) -> Result<Option<String>> {
        let tx = self.db.transaction()?;
        let gone = if on {
            put_in(&tx, category, feed)?;
            None
        } else {
            tx.execute(
                "DELETE FROM category_feeds WHERE category_id = ?1 AND feed_id = ?2",
                params![category, feed],
            )?;
            forget_if_orphan(&tx, feed)?
        };
        tx.commit()?;
        if gone.is_some() {
            self.tidy()?;
            self.scrub()?;
        }
        Ok(gone)
    }

    /// Names a feed as the feed names itself, unless you've named it (one
    /// of the defaults, or one you renamed).
    pub fn name_feed_if_unnamed(&self, id: FeedId, title: &str) -> Result<()> {
        let title = article::one_line(title, article::MAX_SOURCE);
        if title.is_empty() {
            return Ok(());
        }
        self.db.execute(
            "UPDATE feeds SET title = ?2 WHERE id = ?1 AND title = ''",
            params![id, title],
        )?;
        Ok(())
    }

    /// Notes a fetch that worked: when, and what to send next time to hear
    /// it's unchanged.
    pub fn feed_fetched(
        &self,
        id: FeedId,
        at: i64,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE feeds SET fetched_at = ?2, error = NULL,
                    etag = COALESCE(?3, etag), last_modified = COALESCE(?4, last_modified)
             WHERE id = ?1",
            params![id, at, etag, last_modified],
        )?;
        Ok(())
    }

    /// Notes a fetch that failed, and why: it's tried again on the next
    /// round that it's due.
    pub fn feed_failed(&self, id: FeedId, at: i64, why: &str) -> Result<()> {
        self.db.execute(
            "UPDATE feeds SET fetched_at = ?2, error = ?3 WHERE id = ?1",
            params![id, at, article::one_line(why, 200)],
        )?;
        Ok(())
    }

    /// Keeps what `feed` brought, each article linked to it. An article
    /// already kept gets the feed's newer words and picture, but keeps its
    /// date and its first feed (the one it says it's from). How many were
    /// new.
    pub fn save_articles(
        &mut self,
        feed_id: FeedId,
        source: &str,
        items: &[feed::Item],
        now: i64,
    ) -> Result<usize> {
        let tx = self.db.transaction()?;
        let mut new = 0;
        for item in items {
            let id = ArticleId::of(&item.link);
            // A date ahead of now (a wrong zone) would stay first forever.
            let published = item.published.unwrap_or(now).min(now);
            let added = tx.execute(
                "INSERT OR IGNORE INTO articles
                    (id, feed_id, source, link, title, summary, image, author, category, published, seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    id.as_str(),
                    feed_id,
                    source,
                    item.link,
                    item.title,
                    item.summary,
                    item.image,
                    item.author,
                    item.category,
                    published,
                    now
                ],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO article_feeds (article_id, feed_id, published)
                 VALUES (?1, ?2, (SELECT published FROM articles WHERE id = ?1))",
                params![id.as_str(), feed_id],
            )?;
            // The text the feed carries, as if read from its page.
            if !item.body.is_empty() {
                tx.execute(
                    "INSERT OR REPLACE INTO bodies (article_id, body, fetched_at) VALUES (?1, ?2, ?3)",
                    params![id.as_str(), Block::encode(&item.body), now],
                )?;
            }
            let hidden = !self.hidden.is_empty()
                && self
                    .hidden
                    .hides(&article::one_line(&item.title, article::MAX_TITLE));
            if hidden {
                tx.execute(
                    "INSERT OR IGNORE INTO temp.hidden VALUES (?1)",
                    [id.as_str()],
                )?;
            }
            if added > 0 {
                new += 1;
                continue;
            }
            // Its words are the feed's newer ones: hidden or not again.
            if !hidden && !self.hidden.is_empty() {
                tx.execute(
                    "DELETE FROM temp.hidden WHERE article_id = ?1",
                    [id.as_str()],
                )?;
            }
            // A feed again for one whose feeds were all removed (kept as
            // saved, or in History): it says it's from this one now.
            tx.execute(
                "UPDATE articles SET title = ?2, summary = ?3, image = COALESCE(?4, image),
                        author = ?5, category = ?6, seen_at = ?7,
                        source = CASE WHEN feed_id IS NULL THEN ?9 ELSE source END,
                        feed_id = COALESCE(feed_id, ?8)
                 WHERE id = ?1",
                params![
                    id.as_str(),
                    item.title,
                    item.summary,
                    item.image,
                    item.author,
                    item.category,
                    now,
                    feed_id,
                    source
                ],
            )?;
        }
        tx.commit()?;
        Ok(new)
    }

    /// The articles `listing` shows, in its order.
    pub fn articles(&self, listing: &Listing) -> Result<Vec<Article>> {
        let from = "FROM articles a LEFT JOIN feeds f ON f.id = a.feed_id";
        let (sql, param) = if let Some((newest, param)) = newest(listing) {
            let sql = format!(
                "SELECT {COLUMNS} {from} JOIN ({newest}) w ON w.article_id = a.id
                 ORDER BY w.published DESC, w.article_id"
            );
            (sql, param)
        } else {
            let sql = match listing {
                Listing::Saved => format!(
                    "SELECT {COLUMNS} {from} JOIN saved s ON s.article_id = a.id
                     ORDER BY s.at DESC"
                ),
                Listing::History => format!(
                    "SELECT {COLUMNS} {from} JOIN history h ON h.article_id = a.id
                     ORDER BY h.at DESC"
                ),
                // Search: the only other one.
                _ => format!("SELECT {COLUMNS} {from} ORDER BY a.published DESC LIMIT {SEARCHED}"),
            };
            (sql, None)
        };
        let mut statement = self.db.prepare(&sql)?;
        let rows = match param {
            Some(p) => statement.query_map([p], article_row)?.collect::<Vec<_>>(),
            None => statement.query_map([], article_row)?.collect::<Vec<_>>(),
        };
        let mut articles: Vec<Article> =
            rows.into_iter().filter_map(|r| r.ok().flatten()).collect();
        if let Listing::Search(query) = listing {
            articles.retain(|a| a.matches(query));
        }
        Ok(articles)
    }

    /// The ids of the articles you've read.
    pub fn read(&self) -> Result<HashSet<ArticleId>> {
        self.ids("SELECT article_id FROM read")
    }

    /// The ids of the articles you've saved.
    pub fn saved(&self) -> Result<HashSet<ArticleId>> {
        self.ids("SELECT article_id FROM saved")
    }

    fn ids(&self, sql: &str) -> Result<HashSet<ArticleId>> {
        let mut statement = self.db.prepare(sql)?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(Result::ok)
            .filter_map(|id| ArticleId::parse(&id))
            .collect();
        Ok(ids)
    }

    pub fn mark_read(&mut self, ids: &[ArticleId], at: i64) -> Result<()> {
        let tx = self.db.transaction()?;
        for id in ids {
            tx.execute(
                "INSERT OR IGNORE INTO read (article_id, at) VALUES (?1, ?2)",
                params![id.as_str(), at],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mark_unread(&self, id: &ArticleId) -> Result<()> {
        self.db
            .execute("DELETE FROM read WHERE article_id = ?1", [id.as_str()])?;
        Ok(())
    }

    pub fn save(&self, id: &ArticleId, at: i64) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO saved (article_id, at) VALUES (?1, ?2)",
            params![id.as_str(), at],
        )?;
        Ok(())
    }

    /// Takes an article off Saved, at once off the disk too.
    pub fn unsave(&self, id: &ArticleId) -> Result<()> {
        self.db
            .execute("DELETE FROM saved WHERE article_id = ?1", [id.as_str()])?;
        self.scrub()
    }

    /// Puts an article first in History, keeping [`HISTORY_SIZE`] (cut back
    /// [`HISTORY_SLACK`] over): those left out go at once off the disk too.
    pub fn opened(&self, id: &ArticleId, at: i64) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO history (article_id, at) VALUES (?1, ?2)",
            params![id.as_str(), at],
        )?;
        let gone = self.db.execute(
            &format!(
                "DELETE FROM history
                 WHERE (SELECT COUNT(*) FROM history) > {}
                   AND article_id NOT IN
                       (SELECT article_id FROM history ORDER BY at DESC LIMIT {HISTORY_SIZE})",
                HISTORY_SIZE + HISTORY_SLACK
            ),
            [],
        )?;
        if gone > 0 {
            self.scrub()?;
        }
        Ok(())
    }

    /// Takes an article off History, at once off the disk too.
    pub fn take_off_history(&self, id: &ArticleId) -> Result<()> {
        self.db
            .execute("DELETE FROM history WHERE article_id = ?1", [id.as_str()])?;
        self.scrub()
    }

    /// An article's text, as read from its page before.
    pub fn body(&self, id: &ArticleId) -> Result<Option<String>> {
        let body: Option<String> = self
            .db
            .query_row(
                "SELECT body FROM bodies WHERE article_id = ?1",
                [id.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(body.map(|b| text::clean(&b)))
    }

    pub fn save_body(&self, id: &ArticleId, body: &str, at: i64) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO bodies (article_id, body, fetched_at) VALUES (?1, ?2, ?3)",
            params![id.as_str(), body, at],
        )?;
        Ok(())
    }

    /// Forgets articles first seen more than `days` ago that you haven't
    /// saved or opened, and what was kept of them, at once off the disk
    /// too. At start.
    pub fn prune(&mut self, days: u32, now: i64) -> Result<usize> {
        let before = now.saturating_sub(i64::from(days) * 86_400);
        let gone = self.db.execute(
            "DELETE FROM articles WHERE seen_at < ?1
               AND id NOT IN (SELECT article_id FROM saved)
               AND id NOT IN (SELECT article_id FROM history)",
            [before],
        )?;
        let tidied = self.tidy()?;
        if gone + tidied > 0 {
            self.scrub()?;
        }
        Ok(gone)
    }

    /// Read marks, texts, links to feeds and hidden marks of articles no
    /// longer kept. How many were.
    fn tidy(&self) -> Result<usize> {
        let mut gone = 0;
        for table in ["read", "bodies", "article_feeds", "temp.hidden"] {
            gone += self.db.execute(
                &format!("DELETE FROM {table} WHERE article_id NOT IN (SELECT id FROM articles)"),
                [],
            )?;
        }
        Ok(gone)
    }

    /// Writes deleted rows' zeroed pages into the database file now.
    fn scrub(&self) -> Result<()> {
        self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }
}

/// Gives a link with no date its article's: a tuinews from before version 3
/// that ran on this database since writes them so (it runs on it as it is).
/// It puts its index back too, which says it was here.
fn date_links(db: &Connection) -> Result<()> {
    let older_was_here = db
        .prepare("SELECT 1 FROM sqlite_master WHERE name = 'article_feeds_by_feed'")?
        .exists([])?;
    if older_was_here {
        db.execute_batch(
            "DROP INDEX article_feeds_by_feed;
             UPDATE article_feeds SET published =
                (SELECT published FROM articles WHERE id = article_feeds.article_id)
             WHERE published = 0
               AND EXISTS (SELECT 1 FROM articles WHERE id = article_feeds.article_id);",
        )?;
    }
    Ok(())
}

/// Brings a database an earlier tuinews made up to [`VERSION`]: the
/// `listed` column of feeds (all of an earlier one's are), each article
/// linked to the feed that brought it, and its date beside each link.
fn migrate(db: &Connection) -> Result<()> {
    let version: i64 = db
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'version'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(1);
    if version >= VERSION {
        return Ok(());
    }
    let has = |table: &str, column: &str| -> rusqlite::Result<bool> {
        db.prepare(&format!(
            "SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}'"
        ))?
        .exists([])
    };
    if !has("feeds", "listed")? {
        db.execute_batch("ALTER TABLE feeds ADD COLUMN listed INTEGER NOT NULL DEFAULT 1;")?;
    }
    if !has("article_feeds", "published")? {
        db.execute_batch(
            "ALTER TABLE article_feeds ADD COLUMN published INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    db.execute_batch(&format!(
        "INSERT OR IGNORE INTO article_feeds (article_id, feed_id)
            SELECT id, feed_id FROM articles WHERE feed_id IS NOT NULL;
         UPDATE article_feeds SET published = COALESCE(
            (SELECT published FROM articles WHERE id = article_feeds.article_id), 0);
         DROP INDEX IF EXISTS article_feeds_by_feed;
         CREATE INDEX IF NOT EXISTS article_feeds_by_date
            ON article_feeds (feed_id, published DESC, article_id);
         INSERT OR REPLACE INTO meta (key, value) VALUES ('version', '{VERSION}');"
    ))?;
    Ok(())
}

/// The articles `listing` shows, if it's one of feeds' (Home, a feed's, a
/// category's): its newest of them, each once, as `article_id` and
/// `published`, and what `?1` is. A feed's come in order off the index by
/// feed and date; Home's and a category's are those of their feeds, sorted.
fn newest(listing: &Listing) -> Option<(String, Option<i64>)> {
    // An article's rows all have its date, so DISTINCT keeps one of each
    // (much faster than GROUP BY).
    let of = |feeds: &str| {
        format!(
            "SELECT DISTINCT article_id, published FROM article_feeds
             WHERE feed_id IN ({feeds})
             ORDER BY published DESC, article_id LIMIT {HOME_SIZE}"
        )
    };
    Some(match listing {
        Listing::Home => (of("SELECT id FROM feeds WHERE listed = 1"), None),
        Listing::Feed(id) => (
            format!(
                "SELECT article_id, published FROM article_feeds WHERE feed_id = ?1
                 ORDER BY published DESC, article_id LIMIT {FEED_SIZE}"
            ),
            Some(*id),
        ),
        Listing::Category(id) => (
            of("SELECT feed_id FROM category_feeds WHERE category_id = ?1"),
            Some(*id),
        ),
        Listing::Saved | Listing::History | Listing::Search(_) => return None,
    })
}

/// Puts a feed in, the last one. Its id.
fn insert_feed(db: &Connection, url: &str, title: &str, listed: bool) -> Result<FeedId> {
    db.execute(
        "INSERT INTO feeds (url, title, place, listed)
         VALUES (?1, ?2, (SELECT COALESCE(MAX(place), -1) + 1 FROM feeds), ?3)",
        params![url, article::one_line(title, article::MAX_SOURCE), listed],
    )?;
    Ok(db.last_insert_rowid())
}

/// Makes a category, the last one. Its id.
fn insert_category(db: &Connection, name: &str) -> Result<CategoryId> {
    db.execute(
        "INSERT INTO categories (name, place)
         VALUES (?1, (SELECT COALESCE(MAX(place), -1) + 1 FROM categories))",
        [name],
    )?;
    Ok(db.last_insert_rowid())
}

/// Puts a feed under Feeds and in Home, last, as a feed added is.
fn list(db: &Connection, id: FeedId) -> rusqlite::Result<usize> {
    db.execute(
        "UPDATE feeds SET listed = 1,
                place = (SELECT COALESCE(MAX(place), -1) + 1 FROM feeds)
         WHERE id = ?1",
        [id],
    )
}

/// Puts a feed in a category, if it isn't in it yet.
fn put_in(db: &Connection, category: CategoryId, feed: FeedId) -> rusqlite::Result<usize> {
    db.execute(
        "INSERT OR IGNORE INTO category_feeds (category_id, feed_id) VALUES (?1, ?2)",
        params![category, feed],
    )
}

/// The default categories and their sections. A section that's a feed of
/// yours already is that feed; a feed of [`defaults::FEEDS`] you removed
/// isn't put back.
fn seed_categories(tx: &rusqlite::Transaction) -> Result<()> {
    for category in &defaults::CATEGORIES {
        let id = insert_category(tx, category.name)?;
        let feed_id = |url: &str| -> rusqlite::Result<Option<FeedId>> {
            tx.query_row("SELECT id FROM feeds WHERE url = ?1", [url], |r| r.get(0))
                .optional()
        };
        for feed in category.feeds {
            if feed_id(feed.url)?.is_none() {
                tx.execute(
                    "INSERT INTO feeds (url, title, place, listed)
                     VALUES (?1, ?2, (SELECT COALESCE(MAX(place), -1) + 1 FROM feeds), 0)",
                    params![feed.url, feed.name],
                )?;
            }
        }
        for url in category
            .feeds
            .iter()
            .map(|f| f.url)
            .chain(category.also.iter().copied())
        {
            if let Some(feed) = feed_id(url)? {
                put_in(tx, id, feed)?;
            }
        }
    }
    Ok(())
}

/// Removes a feed: from its categories, and its articles but those another
/// feed brought too, or you saved or opened. An article it was the first to
/// bring now says it's from another that brought it, by that one's name if
/// it has one yet.
fn forget_feed(tx: &rusqlite::Transaction, id: FeedId) -> Result<()> {
    tx.execute("DELETE FROM feeds WHERE id = ?1", [id])?;
    tx.execute("DELETE FROM category_feeds WHERE feed_id = ?1", [id])?;
    tx.execute("DELETE FROM article_feeds WHERE feed_id = ?1", [id])?;
    tx.execute(
        "UPDATE articles SET feed_id = other.feed_id,
                source = COALESCE(NULLIF(f.title, ''), articles.source)
         FROM (SELECT article_id, MIN(feed_id) AS feed_id FROM article_feeds
               WHERE article_id IN (SELECT id FROM articles WHERE feed_id = ?1)
               GROUP BY article_id) other
         JOIN feeds f ON f.id = other.feed_id
         WHERE articles.id = other.article_id AND articles.feed_id = ?1",
        [id],
    )?;
    // Those no other feed brought.
    tx.execute(
        "UPDATE articles SET feed_id = NULL WHERE feed_id = ?1",
        [id],
    )?;
    tx.execute(
        "DELETE FROM articles WHERE feed_id IS NULL
           AND id NOT IN (SELECT article_id FROM saved)
           AND id NOT IN (SELECT article_id FROM history)",
        [],
    )?;
    Ok(())
}

/// Removes a feed that's neither under Feeds nor in a category: nowhere
/// shows it any more. Its name, if it was removed.
fn forget_if_orphan(tx: &rusqlite::Transaction, id: FeedId) -> Result<Option<String>> {
    let orphan: Option<String> = tx
        .query_row(
            "SELECT title FROM feeds WHERE id = ?1 AND listed = 0
               AND NOT EXISTS (SELECT 1 FROM category_feeds WHERE feed_id = ?1)",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    if orphan.is_some() {
        forget_feed(tx, id)?;
    }
    Ok(orphan.map(|t| article::one_line(&t, article::MAX_SOURCE)))
}

fn protect(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let mut file = path.as_os_str().to_owned();
            file.push(suffix);
            let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(n: u32, published: i64) -> feed::Item {
        feed::Item {
            title: format!("Bài số {n}"),
            link: format!("https://vnexpress.net/bai-{n}.html"),
            summary: format!("Tóm tắt {n}"),
            image: Some(format!("https://i1-vnexpress.vnecdn.net/{n}.jpg")),
            published: Some(published),
            ..feed::Item::default()
        }
    }

    fn listed(store: &Store) -> Vec<FeedRow> {
        store
            .feeds()
            .unwrap()
            .into_iter()
            .filter(|f| f.listed)
            .collect()
    }

    #[test]
    fn the_defaults_go_in_once_and_removed_ones_stay_removed() {
        let mut store = Store::in_memory();
        assert!(store.seed_defaults().unwrap());
        let feeds = listed(&store);
        assert_eq!(feeds.len(), defaults::FEEDS.len());
        assert_eq!(feeds[0].title, "VnExpress");
        store.remove_feed(feeds[0].id).unwrap();
        assert!(!store.seed_defaults().unwrap());
        assert_eq!(listed(&store).len(), defaults::FEEDS.len() - 1);
        let categories = store.categories().unwrap();
        assert_eq!(categories.len(), defaults::CATEGORIES.len());
        let tech = &categories[0];
        assert_eq!(tech.name, "Tech");
        // Its sites' sections, and GenK, which is under Feeds too.
        assert_eq!(tech.feeds.len(), defaults::CATEGORIES[0].feeds.len() + 1);
        let genk = feeds.iter().find(|f| f.title == "GenK").unwrap();
        assert!(tech.feeds.contains(&genk.id));
        let all = store.feeds().unwrap();
        let sections = all.iter().filter(|f| !f.listed).count();
        let wanted: usize = defaults::CATEGORIES.iter().map(|c| c.feeds.len()).sum();
        assert_eq!(sections, wanted, "each section once");
    }

    #[test]
    fn a_category_lists_its_feeds_articles_and_home_only_yours() {
        let mut store = Store::in_memory();
        let home_feed = store
            .add_feed(
                "https://vnexpress.net/rss/tin-moi-nhat.rss",
                "VnExpress",
                true,
            )
            .unwrap();
        let cars = store
            .add_feed(
                "https://vnexpress.net/rss/oto-xe-may.rss",
                "VnExpress Xe",
                false,
            )
            .unwrap();
        let category = store.add_category("Cars").unwrap();
        assert!(store.add_category("cars").is_err(), "the name is taken");
        assert!(store.add_category("  ").is_err());
        store.set_in_category(category, cars, true).unwrap();
        // One article in both feeds, one only in the section.
        store
            .save_articles(home_feed, "VnExpress", &[item(1, 10)], 100)
            .unwrap();
        store
            .save_articles(cars, "VnExpress Xe", &[item(1, 10), item(2, 20)], 100)
            .unwrap();
        let titles = |listing| -> Vec<String> {
            store
                .articles(&listing)
                .unwrap()
                .into_iter()
                .map(|a| a.title)
                .collect()
        };
        assert_eq!(titles(Listing::Home), ["Bài số 1"]);
        assert_eq!(
            titles(Listing::Category(category)),
            ["Bài số 2", "Bài số 1"]
        );
        assert_eq!(titles(Listing::Feed(cars)), ["Bài số 2", "Bài số 1"]);
        assert_eq!(store.home_unread().unwrap(), 1);
        let cars_row = &store.categories().unwrap()[0];
        assert_eq!((cars_row.feeds.len(), cars_row.unread), (1, 2));
        let first = store.articles(&Listing::Home).unwrap()[0].id.clone();
        assert_eq!(store.feeds_of(&first).unwrap(), [home_feed, cars]);

        // Removing the feed it came from first: it says it's from the other.
        store.remove_feed(home_feed).unwrap();
        let left = store.articles(&Listing::Category(category)).unwrap();
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|a| a.feed == Some(cars)));
        assert!(left.iter().all(|a| a.source == "VnExpress Xe"));
    }

    #[test]
    fn deleting_a_category_removes_the_sections_only_it_had() {
        let mut store = Store::in_memory();
        let listed_feed = store
            .add_feed("https://genk.vn/rss/home.rss", "GenK", true)
            .unwrap();
        let section = store
            .add_feed("https://tuoitre.vn/rss/cong-nghe.rss", "Tuổi Trẻ", false)
            .unwrap();
        let shared = store
            .add_feed(
                "https://dantri.com.vn/rss/suc-manh-so.rss",
                "Dân Trí",
                false,
            )
            .unwrap();
        let tech = store.add_category("Tech").unwrap();
        let other = store.add_category("Other").unwrap();
        for feed in [listed_feed, section, shared] {
            store.set_in_category(tech, feed, true).unwrap();
        }
        store.set_in_category(other, shared, true).unwrap();
        store
            .save_articles(section, "Tuổi Trẻ", &[item(1, 10), item(2, 20)], 100)
            .unwrap();
        let kept = store.articles(&Listing::Category(tech)).unwrap()[0]
            .id
            .clone();
        store.save(&kept, 100).unwrap();

        let gone = store.delete_category(tech).unwrap();
        assert_eq!(gone, ["Tuổi Trẻ"], "GenK is under Feeds, Dân Trí in Other");
        let ids: Vec<FeedId> = store.feeds().unwrap().iter().map(|f| f.id).collect();
        assert_eq!(ids, [listed_feed, shared]);
        assert_eq!(
            store.articles(&Listing::Saved).unwrap().len(),
            1,
            "saved stays"
        );
        assert_eq!(store.categories().unwrap().len(), 1);

        // Out of its last category, a section goes too.
        assert_eq!(
            store
                .set_in_category(other, shared, false)
                .unwrap()
                .as_deref(),
            Some("Dân Trí")
        );
        assert_eq!(
            store.set_in_category(other, listed_feed, false).unwrap(),
            None
        );
        store.rename_category(other, "Khác").unwrap();
        assert_eq!(store.categories().unwrap()[0].name, "Khác");
    }

    #[test]
    fn a_database_from_before_categories_is_brought_up_to_date() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE feeds (id INTEGER PRIMARY KEY, url TEXT NOT NULL UNIQUE,
                 title TEXT NOT NULL DEFAULT '', place INTEGER NOT NULL, fetched_at INTEGER,
                 etag TEXT, last_modified TEXT, error TEXT);
             CREATE TABLE articles (id TEXT PRIMARY KEY, feed_id INTEGER,
                 source TEXT NOT NULL DEFAULT '', link TEXT NOT NULL, title TEXT NOT NULL,
                 summary TEXT NOT NULL DEFAULT '', image TEXT, author TEXT NOT NULL DEFAULT '',
                 category TEXT NOT NULL DEFAULT '', published INTEGER NOT NULL,
                 seen_at INTEGER NOT NULL);
             INSERT INTO meta VALUES ('seeded', '1');
             INSERT INTO feeds (id, url, title, place) VALUES (1, 'https://e.vn/rss', 'E', 0);
             INSERT INTO articles (id, feed_id, link, title, published, seen_at)
                 VALUES ('0123456789abcdef', 1, 'https://e.vn/a', 'Tin', 1, 1);",
        )
        .unwrap();
        db.execute_batch(SCHEMA).unwrap();
        migrate(&db).unwrap();
        let mut store = Store::with(db).unwrap();
        assert!(
            !store.seed_defaults().unwrap(),
            "its feeds stay as they are"
        );
        assert_eq!(store.articles(&Listing::Home).unwrap().len(), 1);
        assert_eq!(store.articles(&Listing::Feed(1)).unwrap().len(), 1);
        assert!(listed(&store).iter().any(|f| f.id == 1));
        assert_eq!(
            store.categories().unwrap().len(),
            defaults::CATEGORIES.len()
        );
        migrate(&store.db).unwrap();
    }

    #[test]
    fn links_an_older_tuinews_left_without_dates_get_them() {
        let mut store = Store::in_memory();
        let a = store.add_feed("https://e.vn/rss", "E", true).unwrap();
        let b = store.add_feed("https://f.vn/rss", "F", true).unwrap();
        store
            .save_articles(a, "E", &[item(1, 10), item(2, 30)], 100)
            .unwrap();
        store.save_articles(b, "F", &[item(1, 10)], 100).unwrap();
        // As an older tuinews leaves them: links without dates, and its index.
        store
            .db
            .execute(
                "UPDATE article_feeds SET published = 0 WHERE feed_id = ?1",
                [b],
            )
            .unwrap();
        store
            .db
            .execute_batch("CREATE INDEX article_feeds_by_feed ON article_feeds (feed_id);")
            .unwrap();
        date_links(&store.db).unwrap();
        let titles: Vec<String> = store
            .articles(&Listing::Home)
            .unwrap()
            .into_iter()
            .map(|a| a.title)
            .collect();
        assert_eq!(titles, ["Bài số 2", "Bài số 1"], "each once, by its date");
    }

    #[test]
    fn a_version_2_database_gets_dates_beside_its_links() {
        let mut store = Store::in_memory();
        let feed = store.add_feed("https://e.vn/rss", "E", true).unwrap();
        store
            .save_articles(feed, "E", &[item(1, 10), item(2, 30), item(3, 20)], 100)
            .unwrap();
        // As version 2 left it: the links without dates, indexed by feed.
        store
            .db
            .execute_batch(
                "DROP INDEX article_feeds_by_date;
                 ALTER TABLE article_feeds DROP COLUMN published;
                 CREATE INDEX article_feeds_by_feed ON article_feeds (feed_id);
                 UPDATE meta SET value = '2' WHERE key = 'version';",
            )
            .unwrap();
        migrate(&store.db).unwrap();
        let titles: Vec<String> = store
            .articles(&Listing::Feed(feed))
            .unwrap()
            .into_iter()
            .map(|a| a.title)
            .collect();
        assert_eq!(titles, ["Bài số 2", "Bài số 3", "Bài số 1"]);
        assert_eq!(store.home_unread().unwrap(), 3);
        let old_index = store
            .db
            .prepare("SELECT 1 FROM sqlite_master WHERE name = 'article_feeds_by_feed'")
            .unwrap()
            .exists([])
            .unwrap();
        assert!(!old_index);
    }

    #[test]
    fn unread_counts_count_what_each_view_shows() {
        let mut store = Store::in_memory();
        let feed = store.add_feed("https://e.vn/rss", "E", true).unwrap();
        let category = store.add_category("Tin").unwrap();
        store.set_in_category(category, feed, true).unwrap();
        let n = HOME_SIZE + 10;
        let mut items: Vec<feed::Item> = (0..n as u32).map(|i| item(i, i64::from(i))).collect();
        items[n - 1].title = "Showbiz: tin mới nhất".into();
        store.save_articles(feed, "E", &items, 1_000_000).unwrap();
        let home = store.articles(&Listing::Home).unwrap();
        assert_eq!(home.len(), HOME_SIZE);
        assert_eq!(home[0].title, "Showbiz: tin mới nhất");
        assert_eq!(
            store.home_unread().unwrap(),
            HOME_SIZE,
            "not those it doesn't show"
        );
        assert_eq!(store.feeds().unwrap()[0].unread, FEED_SIZE);
        assert_eq!(store.categories().unwrap()[0].unread, HOME_SIZE);
        store.mark_read(&[home[1].id.clone()], 1).unwrap();
        assert_eq!(store.home_unread().unwrap(), HOME_SIZE - 1);
        let settings = crate::settings::Settings {
            blocked_words: vec!["showbiz".into()],
            ..crate::settings::Settings::default()
        };
        store.hide(settings.blocks()).unwrap();
        assert_eq!(
            store.home_unread().unwrap(),
            HOME_SIZE - 2,
            "nor what's hidden"
        );
        assert_eq!(store.feeds().unwrap()[0].unread, FEED_SIZE - 2);
        // One that comes in after, hidden too; it pushes an older one out.
        let mut newer = item(n as u32, n as i64);
        newer.title = "Showbiz: tin sau".into();
        store.save_articles(feed, "E", &[newer], 1_000_000).unwrap();
        assert_eq!(store.home_unread().unwrap(), HOME_SIZE - 3);
    }

    #[test]
    fn a_new_database_is_new_till_you_say_what_to_start_with() {
        let mut store = Store::in_memory();
        assert!(store.is_new().unwrap());
        store.start_empty().unwrap();
        assert!(!store.is_new().unwrap());
        assert!(!store.seed_defaults().unwrap(), "the defaults never go in");
        assert!(store.feeds().unwrap().is_empty());
        assert!(store.categories().unwrap().is_empty());
    }

    #[test]
    fn an_opml_files_feeds_and_folders_come_in_once() {
        let mut store = Store::in_memory();
        store.seed_defaults().unwrap();
        let outline = |url: &str, title: &str, folder: Option<&str>| opml::Outline {
            url: url.into(),
            title: title.into(),
            folder: folder.map(Into::into),
            section: false,
        };
        let bbc = "https://feeds.bbci.co.uk/news/rss.xml";
        let section = "https://vnexpress.net/rss/oto-xe-may.rss";
        let feeds = [
            outline(bbc, "BBC", Some("world")),
            outline(bbc, "BBC", Some("Reading")),
            outline(section, "VnExpress Xe", None),
            outline(
                "https://news.ycombinator.com/rss",
                "Hacker News",
                Some("Reading"),
            ),
        ];
        let file = opml::Opml {
            feeds: feeds.to_vec(),
            folders: vec!["world".into(), "Reading".into(), "Later".into()],
            skipped: 0,
        };
        let done = store.import(&file).unwrap();
        let wanted = Imported {
            added: 2,
            known: 1,
            categories: 2,
            over: 0,
            folders_over: 0,
        };
        assert_eq!(done, wanted);
        let all = store.feeds().unwrap();
        let bbc = all.iter().find(|f| f.url == bbc).unwrap();
        assert_eq!(bbc.title, "BBC");
        assert!(bbc.listed);
        let section = all.iter().find(|f| f.url == section).unwrap();
        assert!(section.listed, "a section of yours, under Feeds now");
        let categories = store.categories().unwrap();
        let in_category = |name: &str| {
            categories
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .feeds
                .clone()
        };
        assert!(in_category("World").contains(&bbc.id), "yours, in any case");
        assert_eq!(in_category("Reading").len(), 2);
        assert!(in_category("Later").is_empty(), "an empty folder too");
        let again = store.import(&file).unwrap();
        assert_eq!((again.added, again.known, again.categories), (0, 3, 0));

        let mut store = Store::in_memory();
        let many = opml::Opml {
            feeds: (0..MAX_FEEDS + 3)
                .map(|n| outline(&format!("https://e{n}.example/rss"), "", None))
                .collect(),
            ..opml::Opml::default()
        };
        let done = store.import(&many).unwrap();
        assert_eq!((done.added, done.over), (MAX_FEEDS, 3));

        // No room for a folder's category: its section goes under Feeds,
        // where it shows.
        let mut store = Store::in_memory();
        for n in 0..MAX_CATEGORIES {
            store.add_category(&format!("C{n}")).unwrap();
        }
        let mut section = outline("https://e.example/rss", "E", Some("Tech"));
        section.section = true;
        let file = opml::Opml {
            feeds: vec![section],
            folders: vec!["Tech".into(), "tech".into()],
            skipped: 0,
        };
        let done = store.import(&file).unwrap();
        assert_eq!((done.added, done.folders_over), (1, 1));
        assert!(store.feeds().unwrap()[0].listed);
    }

    #[test]
    fn articles_are_kept_once_newest_first_with_what_you_did_with_them() {
        let mut store = Store::in_memory();
        let feed = store
            .add_feed("https://vnexpress.net/rss/a.rss", "", true)
            .unwrap();
        assert!(
            store
                .add_feed("https://vnexpress.net/rss/a.rss", "", true)
                .is_err()
        );
        store.name_feed_if_unnamed(feed, "VnExpress").unwrap();
        let items = [item(1, 1_000), item(2, 3_000), item(3, 2_000)];
        assert_eq!(
            store
                .save_articles(feed, "VnExpress", &items, 5_000)
                .unwrap(),
            3
        );
        assert_eq!(
            store
                .save_articles(feed, "VnExpress", &items, 6_000)
                .unwrap(),
            0
        );
        let home = store.articles(&Listing::Home).unwrap();
        let titles: Vec<&str> = home.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["Bài số 2", "Bài số 3", "Bài số 1"]);
        assert_eq!(home[0].feed, Some(feed));
        assert_eq!(store.feeds().unwrap()[0].unread, 3);

        store.mark_read(&[home[0].id.clone()], 7_000).unwrap();
        assert_eq!(store.feeds().unwrap()[0].unread, 2);
        store.save(&home[1].id, 7_000).unwrap();
        store.opened(&home[2].id, 7_000).unwrap();
        assert_eq!(store.articles(&Listing::Saved).unwrap()[0].id, home[1].id);
        assert_eq!(store.articles(&Listing::History).unwrap()[0].id, home[2].id);

        // Removing the feed keeps what you saved and opened.
        store.remove_feed(feed).unwrap();
        assert!(store.articles(&Listing::Home).unwrap().is_empty());
        let saved = store.articles(&Listing::Saved).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].feed, None);
        assert_eq!(saved[0].source, "VnExpress");
        assert_eq!(store.articles(&Listing::History).unwrap().len(), 1);
        assert!(
            store.read().unwrap().is_empty(),
            "read marks of what's gone"
        );
    }

    #[test]
    fn history_is_cut_back_once_it_is_well_over() {
        let store = Store::in_memory();
        let count = || -> usize {
            store
                .db
                .query_row("SELECT COUNT(*) FROM history", [], |r| r.get::<_, i64>(0))
                .unwrap() as usize
        };
        let most = HISTORY_SIZE + HISTORY_SLACK;
        for n in 0..=most {
            let id = ArticleId::of(&format!("https://e.vn/{n}"));
            store.opened(&id, n as i64).unwrap();
            if n < most {
                assert_eq!(count(), n + 1);
            }
        }
        assert_eq!(count(), HISTORY_SIZE, "the oldest are gone");
    }

    #[test]
    fn a_date_ahead_is_now_and_old_articles_are_pruned_unless_kept() {
        let mut store = Store::in_memory();
        let feed = store.add_feed("https://e.vn/rss", "E", true).unwrap();
        store
            .save_articles(feed, "E", &[item(1, 99_999), item(2, 10)], 100)
            .unwrap();
        let home = store.articles(&Listing::Home).unwrap();
        assert_eq!(home[0].published, 100);
        store.save(&home[1].id, 100).unwrap();
        let gone = store.prune(1, 100 + 2 * 86_400).unwrap();
        assert_eq!(gone, 1);
        assert_eq!(store.articles(&Listing::Saved).unwrap().len(), 1);
    }

    #[test]
    fn searching_finds_words_without_their_accents() {
        let mut store = Store::in_memory();
        let feed = store.add_feed("https://e.vn/rss", "E", true).unwrap();
        let mut hanoi = item(1, 10);
        hanoi.title = "Hà Nội mưa lớn".into();
        store
            .save_articles(feed, "E", &[hanoi, item(2, 20)], 100)
            .unwrap();
        let found = store
            .articles(&Listing::Search(article::fold("ha noi")))
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Hà Nội mưa lớn");
    }

    #[test]
    fn a_row_changed_on_disk_is_cleaned_or_skipped() {
        let mut store = Store::in_memory();
        let feed = store.add_feed("https://e.vn/rss", "E", true).unwrap();
        store.save_articles(feed, "E", &[item(1, 10)], 100).unwrap();
        store
            .db
            .execute_batch(
                "UPDATE articles SET title = 'bad' || char(27) || '[2J';
                 INSERT INTO articles (id, feed_id, link, title, published, seen_at)
                     VALUES ('../../x', 1, 'https://e.vn/x', 't', 1, 1),
                            ('0123456789abcdef', 1, 'file:///etc/passwd', 't', 1, 1);",
            )
            .unwrap();
        let home = store.articles(&Listing::Home).unwrap();
        assert_eq!(home.len(), 1);
        assert_eq!(home[0].title, "bad[2J");
    }
}
