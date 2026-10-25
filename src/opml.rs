//! OPML: the list of feeds other readers export (Feedly, Inoreader,
//! NetNewsWire, Miniflux…), to bring yours over. Each feed's address and
//! name, and the folder it's in, which becomes a category (`Store::import`).
//!
//! The file is untrusted as a feed is: an address is kept only if
//! `article::checked_link` passes it, names become one line of plain text,
//! and the file is read only up to [`MAX_OPML`]. An outline that points at
//! another file (`type="include"`) isn't followed: only `xmlUrl` is read.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::store::{CategoryRow, FeedRow};
use crate::{article, config, feed, html, store, text};

/// The biggest OPML file read: thousands of feeds' worth.
pub const MAX_OPML: usize = 2 * 1024 * 1024;
/// Elements a file may nest, at most.
const MAX_DEPTH: usize = 64;
/// A name read, at most, in characters, before it's made text: HTML nested
/// deep in a long one would take long to parse.
const MAX_NAME: usize = 1_000;
/// The namespace of what only tuinews writes and reads: `tuinews:section`.
const NAMESPACE: &str = env!("CARGO_PKG_REPOSITORY");

/// A list of feeds, as read.
#[derive(Debug, Default)]
pub struct Opml {
    pub feeds: Vec<Outline>,
    /// Its folders' names, in order, those with no feeds too.
    pub folders: Vec<String>,
    /// Outlines with an address tuinews wouldn't fetch (not http or https,
    /// or on your computer or network).
    pub skipped: usize,
}

/// A feed of the list.
#[derive(Clone, Debug, PartialEq)]
pub struct Outline {
    pub url: String,
    pub title: String,
    /// The folder it's in, the innermost if folders are nested.
    pub folder: Option<String>,
    /// A site's section, only in its category (`tuinews:section`, from
    /// [`write`]).
    pub section: bool,
}

/// What was typed in the add box, if it names a file here rather than a
/// site: `/…`, `~/…`, `./…`, `C:\…`, a `file://` link, a name ending in
/// `.opml` (not one like a site's: `example.com/feeds.opml`). Quotes round
/// it are taken off; else the backslashes a terminal writes before a
/// space, a bracket or a quote of a file dropped on it are. Not a file on
/// another computer (`\\host\share`, `file://host/…`): on Windows, opening
/// one connects to that computer with your login.
pub fn path_in(typed: &str) -> Option<PathBuf> {
    let typed = typed.trim();
    let quoted = ['\'', '"']
        .iter()
        .find_map(|q| typed.strip_prefix(*q)?.strip_suffix(*q));
    let unquoted = quoted.unwrap_or(typed);
    // A path for sure (a file link, one in your home folder), or one if
    // it looks like one.
    let (path, sure) = if unquoted.starts_with("file://") {
        (
            reqwest::Url::parse(unquoted).ok()?.to_file_path().ok()?,
            true,
        )
    } else if unquoted.contains("://") {
        return None;
    } else {
        let text = if cfg!(windows) || quoted.is_some() {
            unquoted.to_string()
        } else {
            unescaped(unquoted)
        };
        match text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
            Some(rest) => (dirs::home_dir()?.join(rest), true),
            None => (PathBuf::from(text), false),
        }
    };
    // Before anything looks at the disk: on Windows, any path starting with
    // a separator may be on another computer (`\\host\share`, `/\host\…`,
    // `\??\UNC\host\…`), and opening it connects there with your login. A
    // link written without its scheme (`//site.vn/rss`) is refused too.
    // A relative path is in the folder tuinews was started in, which may be
    // another computer's: it's checked as the whole path too.
    let whole = if path.is_absolute() {
        path.clone()
    } else {
        std::env::current_dir().ok()?.join(&path)
    };
    if config::maybe_remote(&path) || config::maybe_remote(&whole) {
        return None;
    }
    let text = path.to_string_lossy();
    let here = ["./", "../", ".\\", "..\\"]
        .iter()
        .any(|p| text.starts_with(p));
    // A name like a site's (`example.com/feeds.opml`) isn't a file's: its
    // first part has a dot (`./my.folder/feeds.opml` is a file's). Nothing
    // here looks at the disk, which a stuck network folder would stall.
    let first = text.split(['/', '\\']).next().unwrap_or_default();
    let named = text.to_ascii_lowercase().ends_with(".opml")
        && (!text.contains(['/', '\\']) || !first.contains('.'));
    (sure || path.is_absolute() || here || named).then_some(path)
}

/// `path` without the backslashes a shell's quoting puts before characters
/// (`My\ Feeds\ \(1\).opml`).
fn unescaped(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut escaped = false;
    for c in path.chars() {
        if c == '\\' && !escaped {
            escaped = true;
            continue;
        }
        escaped = false;
        out.push(c);
    }
    out
}

/// Reads the list of feeds in the file at `path`: a file, not a folder or a
/// pipe (which would never end), at most [`MAX_OPML`]. It's looked at
/// before it's opened, so a device isn't, and again once it is: it may have
/// been swapped for a pipe meanwhile, which is opened without waiting.
pub fn read(path: &Path) -> Result<Opml> {
    let shown = config::shown(path);
    let cannot = || format!("cannot read {shown}");
    if !std::fs::metadata(path).with_context(cannot)?.is_file() {
        bail!("{shown} isn't a file");
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    let file = options.open(path).with_context(cannot)?;
    let meta = file.metadata().with_context(cannot)?;
    if !meta.is_file() {
        bail!("{shown} isn't a file");
    }
    let too_big = || format!("{shown} is too big for a list of feeds");
    if meta.len() > MAX_OPML as u64 {
        bail!(too_big());
    }
    let mut bytes = Vec::new();
    file.take(MAX_OPML as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(cannot)?;
    if bytes.len() > MAX_OPML {
        bail!(too_big());
    }
    parse(&feed::decode(&bytes)).with_context(|| format!("{shown} isn't a list of feeds"))
}

/// Reads OPML 1 or 2, leniently: a broken end keeps what came before it.
pub fn parse(xml: &str) -> Result<Opml> {
    let mut reader = Reader::from_str(xml);
    let config = reader.config_mut();
    config.check_end_names = false;
    config.allow_unmatched_ends = true;
    let mut opml = Opml::default();
    let mut saw_opml = false;
    let mut depth = 0usize;
    // The outlines open where the reader is, and how deep: a folder's name,
    // or none for a feed's (or a folder with no name).
    let mut open: Vec<(usize, Option<String>)> = Vec::new();
    loop {
        let event = match reader.read_event() {
            Ok(event) => event,
            Err(_) if saw_opml => break,
            Err(e) => return Err(e).context("not OPML"),
        };
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                if !empty {
                    depth += 1;
                    if depth > MAX_DEPTH {
                        // What came before it is kept, as before a mistake.
                        if saw_opml {
                            break;
                        }
                        bail!("it nests too deep");
                    }
                }
                let element = e.local_name().as_ref().to_ascii_lowercase();
                if element == "opml" {
                    saw_opml = true;
                }
                if element != "outline" || !saw_opml {
                    continue;
                }
                let attributes = attributes(e);
                // By its name without a prefix (`xmlUrl`), leniently.
                let get = |key: &str| {
                    attributes
                        .iter()
                        .find(|(k, v)| k.rsplit(':').next() == Some(key) && !v.trim().is_empty())
                        .map(|(_, v)| v.as_str())
                };
                // As `write` marks a site's section: that prefix only.
                let section = attributes
                    .iter()
                    .any(|(k, v)| k == "tuinews:section" && v.trim() == "true");
                let raw = get("text").or(get("title")).unwrap_or_default();
                let name = html::summary(text::first_chars(raw, MAX_NAME));
                let folder = match get("xmlurl") {
                    Some(url) => {
                        match article::checked_link(url.trim(), None) {
                            Some(url) => opml.feeds.push(Outline {
                                // `write` gives a feed with no name yet
                                // its host for `text`, and no `title`.
                                title: if get("title").is_some() {
                                    article::one_line(&name, article::MAX_SOURCE)
                                } else {
                                    unless_host(article::one_line(&name, article::MAX_SOURCE), &url)
                                },
                                url,
                                folder: open.iter().rev().find_map(|(_, f)| f.clone()),
                                section,
                            }),
                            None => opml.skipped += 1,
                        }
                        None
                    }
                    // A folder holds outlines, so it's written open and
                    // closed, and has no type: a note, a link or an
                    // `include` isn't one.
                    None if !empty
                        && get("type").is_none_or(|t| t.eq_ignore_ascii_case("folder")) =>
                    {
                        let folder = Some(article::one_line(&name, store::MAX_CATEGORY_NAME))
                            .filter(|n| !n.is_empty());
                        opml.folders.extend(folder.clone());
                        folder
                    }
                    None => None,
                };
                if !empty {
                    open.push((depth, folder));
                }
            }
            Event::End(_) => {
                if open.last().is_some_and(|(at, _)| *at == depth) {
                    open.pop();
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_opml {
        bail!("not OPML");
    }
    Ok(opml)
}

/// An element's attributes, their names (with their prefix) in lower case,
/// as some write `xmlurl`, entities resolved. A broken one is left out.
/// Nothing checks that a name comes once: quick-xml's check compares each
/// name with all before it, which a file of thousands of names would make
/// take long.
fn attributes(e: &BytesStart) -> Vec<(String, String)> {
    e.attributes()
        .with_checks(false)
        .filter_map(Result::ok)
        .filter_map(|a| {
            let key = a.key.as_ref().to_ascii_lowercase();
            let value = a
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()?
                .into_owned();
            Some((key, value))
        })
        .collect()
}

/// A feed's `title`, unless it's its address's host, which stands for a
/// name while it has none (`FeedRow::name`).
fn unless_host(title: String, url: &str) -> String {
    if store::host_name(url).as_deref() == Some(title.as_str()) {
        String::new()
    } else {
        title
    }
}

/// Your feeds as OPML 2.0, for another reader or to keep: those in no
/// category, then each category as a folder of its feeds, in the sidebar's
/// order. A site's section (only in categories) is marked `tuinews:section`,
/// so importing the file again puts it back as it was.
pub fn write(feeds: &[FeedRow], categories: &[CategoryRow], now: DateTime<Utc>) -> String {
    let outline = |feed: &FeedRow, indent: &str| {
        let name = escape(&feed.name());
        // One with no name yet has its host for `text`, which OPML wants,
        // and no `title`.
        let title = if feed.title.is_empty() {
            String::new()
        } else {
            format!(" title=\"{name}\"")
        };
        let section = if feed.listed {
            ""
        } else {
            " tuinews:section=\"true\""
        };
        format!(
            "{indent}<outline type=\"rss\" text=\"{name}\"{title} xmlUrl=\"{}\"{section}/>\n",
            escape(&feed.url)
        )
    };
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <opml version=\"2.0\" xmlns:tuinews=\"{NAMESPACE}\">\n  \
         <head>\n    <title>tuinews feeds</title>\n    \
         <dateCreated>{}</dateCreated>\n  </head>\n  <body>\n",
        now.to_rfc2822()
    );
    let in_one: HashSet<_> = categories.iter().flat_map(|c| c.feeds.iter()).collect();
    for feed in feeds.iter().filter(|f| f.listed && !in_one.contains(&f.id)) {
        out += &outline(feed, "    ");
    }
    for category in categories {
        let name = escape(&category.name);
        out += &format!("    <outline text=\"{name}\" title=\"{name}\">\n");
        for feed in feeds.iter().filter(|f| category.feeds.contains(&f.id)) {
            out += &outline(feed, "      ");
        }
        out += "    </outline>\n";
    }
    out + "  </body>\n</opml>\n"
}

/// `text` as it can stand in an attribute, without what XML 1.0 has no
/// place for (control characters, U+FFFE, U+FFFF).
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if c < ' ' || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Writes `text` to a new file at `path`, readable only by you (your feeds
/// say what you read). A file there already is left as it is; the one made
/// here goes again if it can't all be written, so half a list isn't left
/// to look like all of it (and in the way of trying again).
pub fn save(path: &Path, text: &str) -> Result<()> {
    let shown = config::shown(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("{shown} is there already, and tuinews doesn't write over a file")
        }
        Err(e) => return Err(e).with_context(|| format!("cannot write {shown}")),
    };
    if let Err(e) = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
    {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(e).with_context(|| format!("cannot write {shown}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::store::Store;

    const FEEDLY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="1.0">
  <head><title>Eric's subscriptions in feedly Cloud</title></head>
  <body>
    <outline text="Tech &amp; Science" title="Tech &amp; Science">
      <outline type="rss" text="Ars Technica" title="Ars Technica"
        xmlUrl="https://feeds.arstechnica.com/arstechnica/index" htmlUrl="https://arstechnica.com"/>
      <outline type="rss" text="&lt;b&gt;The&lt;/b&gt; Verge" xmlUrl="https://www.theverge.com/rss/index.xml"/>
      <outline text="Deeper">
        <outline type="rss" title="Hacker News" xmlurl="https://news.ycombinator.com/rss"/>
      </outline>
    </outline>
    <outline type="rss" text="BBC" xmlUrl="https://feeds.bbci.co.uk/news/rss.xml"/>
    <outline type="rss" text="Local" xmlUrl="http://192.168.1.2/feed"/>
    <outline type="rss" text="Script" xmlUrl="javascript:alert(1)"/>
    <outline type="include" text="More" url="https://example.com/more.opml"/>
    <outline text="Read later"/>
    <outline text="World">
      <outline type="rss" text="BBC" xmlUrl="https://feeds.bbci.co.uk/news/rss.xml"/>
    </outline>
  </body>
</opml>"#;

    #[test]
    fn feeds_come_with_their_names_and_folders() {
        let opml = parse(FEEDLY).unwrap();
        let read: Vec<(&str, &str, Option<&str>)> = opml
            .feeds
            .iter()
            .map(|f| (f.url.as_str(), f.title.as_str(), f.folder.as_deref()))
            .collect();
        assert_eq!(
            read,
            [
                (
                    "https://feeds.arstechnica.com/arstechnica/index",
                    "Ars Technica",
                    Some("Tech & Science")
                ),
                (
                    "https://www.theverge.com/rss/index.xml",
                    "The Verge",
                    Some("Tech & Science")
                ),
                (
                    "https://news.ycombinator.com/rss",
                    "Hacker News",
                    Some("Deeper")
                ),
                ("https://feeds.bbci.co.uk/news/rss.xml", "BBC", None),
                (
                    "https://feeds.bbci.co.uk/news/rss.xml",
                    "BBC",
                    Some("World")
                ),
            ]
        );
        assert_eq!(opml.skipped, 2, "an address on the network, and a script");
        assert_eq!(
            opml.folders,
            ["Tech & Science", "Deeper", "World"],
            "not an include"
        );
    }

    #[test]
    fn what_isnt_opml_is_refused_and_a_broken_end_keeps_the_rest() {
        assert!(parse("<rss><channel></channel></rss>").is_err());
        assert!(parse("not xml at all").is_err());
        let broken = r#"<opml><body><outline text="A" xmlUrl="https://a.example/rss"/><outline <<"#;
        assert_eq!(parse(broken).unwrap().feeds.len(), 1);
        let deep = format!(
            r#"<opml><body><outline text="A" xmlUrl="https://a.example/rss"/>{}"#,
            "<outline>".repeat(MAX_DEPTH + 1)
        );
        assert_eq!(parse(&deep).unwrap().feeds.len(), 1, "what came before it");
        assert!(parse(&"<x>".repeat(MAX_DEPTH + 1)).is_err());
    }

    #[test]
    fn a_path_is_told_from_a_site() {
        assert_eq!(path_in("genk.vn"), None);
        assert_eq!(path_in("https://genk.vn/rss"), None);
        assert_eq!(path_in("example.com/feeds.opml"), None, "a site's");
        assert_eq!(
            path_in("sub/feeds.opml"),
            Some(PathBuf::from("sub/feeds.opml"))
        );
        assert_eq!(path_in("//site.vn/rss"), None);
        assert_eq!(path_in("feeds.opml"), Some(PathBuf::from("feeds.opml")));
        assert_eq!(path_in("./x.xml"), Some(PathBuf::from("./x.xml")));
        let home = dirs::home_dir().unwrap();
        assert_eq!(path_in("~/a.opml"), Some(home.join("a.opml")));
        #[cfg(unix)]
        {
            assert_eq!(path_in("/tmp/a.xml"), Some(PathBuf::from("/tmp/a.xml")));
            assert_eq!(
                path_in("'/tmp/My Feeds.opml' "),
                Some(PathBuf::from("/tmp/My Feeds.opml"))
            );
            assert_eq!(
                path_in(r"/tmp/My\ Feeds.opml"),
                Some(PathBuf::from("/tmp/My Feeds.opml"))
            );
            assert_eq!(
                path_in("file:///tmp/My%20Feeds.opml"),
                Some(PathBuf::from("/tmp/My Feeds.opml"))
            );
            assert_eq!(
                path_in(r"/tmp/Feedly\ \(1\).opml"),
                Some(PathBuf::from("/tmp/Feedly (1).opml"))
            );
            assert_eq!(
                path_in(r"'/tmp/a\b.opml'"),
                Some(PathBuf::from(r"/tmp/a\b.opml")),
                "quoted, as it is"
            );
        }
        assert_eq!(
            path_in("file://evil.example/s/f.opml"),
            None,
            "another computer's"
        );
        // A home folder's path made another computer's by what follows `~/`.
        assert_eq!(path_in("~///evil/s/f.opml"), None);
        #[cfg(windows)]
        {
            assert_eq!(path_in(r"\\evil\share\f.opml"), None);
            assert_eq!(path_in("//evil/share/f.opml"), None);
            // Separators mixed: Windows takes either.
            assert_eq!(path_in(r"/\evil\s\f.opml"), None);
            assert_eq!(path_in(r"\/evil/s/f.opml"), None);
            assert_eq!(path_in(r"~/\\evil\s\f.opml"), None);
            assert_eq!(path_in(r"\??\UNC\evil\s\f.opml"), None);
            assert_eq!(path_in(r"\Users\me\f.opml"), None, "no drive: maybe remote");
            assert!(path_in(r"C:\Users\me\f.opml").is_some());
        }
    }

    #[test]
    fn only_tuinews_marks_a_section_and_xml_gets_only_what_it_takes() {
        let file = |mark: &str| {
            format!(
                r#"<opml><body><outline text="F"><outline text="A" xmlUrl="https://a.example/rss" {mark}/></outline></body></opml>"#
            )
        };
        assert!(parse(&file(r#"tuinews:section="true""#)).unwrap().feeds[0].section);
        assert!(!parse(&file(r#"section="true""#)).unwrap().feeds[0].section);
        assert!(!parse(&file(r#"other:section="true""#)).unwrap().feeds[0].section);
        assert_eq!(escape("a\u{FFFE}b\u{1}c&\"\u{FFFF}"), "abc&amp;&quot;");
        // A name like its host is a name when the file gives a `title` too.
        let named = |attributes: &str| {
            let file = format!(
                r#"<opml><body><outline {attributes} xmlUrl="https://genk.vn/rss"/></body></opml>"#
            );
            parse(&file).unwrap().feeds[0].title.clone()
        };
        assert_eq!(named(r#"text="genk.vn" title="genk.vn""#), "genk.vn");
        assert_eq!(
            named(r#"text="genk.vn""#),
            "",
            "as `write` puts one with none"
        );
    }

    #[test]
    fn many_attributes_are_read_at_once() {
        // Looking for a name written twice would compare each with all
        // before it: billions of steps here.
        let names: String = (0..60_000).map(|n| format!(" a{n}=\"\"")).collect();
        let file = format!(
            r#"<opml><body><outline text="A"{names} xmlUrl="https://a.example/rss"/></body></opml>"#
        );
        assert_eq!(parse(&file).unwrap().feeds.len(), 1);
    }

    #[test]
    fn your_feeds_written_come_back_as_they_were() {
        let mut store = Store::in_memory();
        store.seed_defaults().unwrap();
        store
            .add_feed("https://example.com/rss?a=1&b=2", "Tom & \"Jerry\"", true)
            .unwrap();
        store
            .add_feed("https://unnamed.example/rss", "", true)
            .unwrap();
        store.add_category("Later").unwrap();
        let written = write(
            &store.feeds().unwrap(),
            &store.categories().unwrap(),
            Utc::now(),
        );
        assert!(written.contains("xmlUrl=\"https://example.com/rss?a=1&amp;b=2\""));
        let read = parse(&written).unwrap();
        assert_eq!(read.skipped, 0);
        let mut again = Store::in_memory();
        again.start_empty().unwrap();
        again.import(&read).unwrap();
        let feeds = |s: &Store| -> BTreeSet<(String, String, bool)> {
            let feeds = s.feeds().unwrap().into_iter();
            feeds.map(|f| (f.url, f.title, f.listed)).collect()
        };
        assert_eq!(feeds(&again), feeds(&store), "sections stay sections");
        let categories = |s: &Store| -> Vec<(String, BTreeSet<String>)> {
            let feeds = s.feeds().unwrap();
            let url = |id: &i64| feeds.iter().find(|f| f.id == *id).unwrap().url.clone();
            let categories = s.categories().unwrap().into_iter();
            categories
                .map(|c| (c.name, c.feeds.iter().map(url).collect()))
                .collect()
        };
        assert_eq!(categories(&again), categories(&store));
    }

    #[test]
    fn saving_makes_a_new_file_and_leaves_one_there_alone() {
        let dir = std::env::temp_dir().join(format!("tuinews-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("feeds.opml");
        save(&file, "one").unwrap();
        assert!(save(&file, "two").is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_file_is_read_and_not_a_huge_one() {
        let dir = std::env::temp_dir().join(format!("tuinews-opml-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("feeds.opml");
        std::fs::write(&file, FEEDLY).unwrap();
        assert_eq!(read(&file).unwrap().feeds.len(), 5);
        assert!(read(&dir).is_err(), "a folder");
        assert!(read(&dir.join("gone.opml")).is_err());
        #[cfg(unix)]
        {
            let pipe = dir.join("pipe.opml");
            let name = std::ffi::CString::new(pipe.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: a valid path; mkfifo touches nothing else.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            assert!(read(&pipe).is_err(), "a pipe, refused without waiting");
        }
        std::fs::write(&file, vec![b' '; MAX_OPML + 1]).unwrap();
        assert!(read(&file).is_err(), "too big");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
