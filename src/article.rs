//! An article from a feed, and the helpers that make what feeds send safe
//! and short enough to show.

use std::fmt;

use unicode_normalization::UnicodeNormalization;

use crate::text;

/// Longest title kept, in characters.
pub const MAX_TITLE: usize = 300;
/// Longest source (feed) name kept.
pub const MAX_SOURCE: usize = 100;
/// Longest author or category kept.
pub const MAX_BYLINE: usize = 100;
/// Longest summary kept: cards show its start, the reader all of it.
pub const MAX_SUMMARY: usize = 3_000;
/// Longest link kept; a longer one is no article's.
pub const MAX_LINK: usize = 2_048;
/// Most lines of a summary kept.
const MAX_LINES: usize = 300;

/// A feed's row in the store.
pub type FeedId = i64;

/// An article's id: the first 16 hex digits of its link's SHA-256. Ids name
/// cached pictures too, so they're checked wherever one comes in.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArticleId(String);

impl ArticleId {
    /// The id of the article at `link`.
    pub fn of(link: &str) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, link.as_bytes());
        let hex: String = digest.as_ref()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Self(hex)
    }

    /// An id read back from the store: 16 lowercase hex digits, or nothing.
    pub fn parse(s: &str) -> Option<Self> {
        (s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ArticleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Article {
    pub id: ArticleId,
    /// The feed it came from, if that's still a feed of yours.
    pub feed: Option<FeedId>,
    /// The feed's name, as it was when the article came.
    pub source: String,
    pub title: String,
    /// Checked by [`checked_link`]: http or https, a name for a host.
    pub link: String,
    /// What the feed says of it, as text, a line per paragraph.
    pub summary: String,
    /// Its picture, checked by [`checked_link`].
    pub image: Option<String>,
    pub author: String,
    pub category: String,
    /// When it was published, in Unix seconds (when it was first seen, if
    /// the feed didn't say).
    pub published: i64,
}

impl Article {
    /// The article's host, as a source of last resort: `vnexpress.net`.
    pub fn host(&self) -> String {
        reqwest::Url::parse(&self.link)
            .ok()
            .and_then(|u| {
                u.host_str()
                    .map(|h| h.trim_start_matches("www.").to_string())
            })
            .unwrap_or_default()
    }

    /// The name of where it's from, never empty.
    pub fn source_name(&self) -> String {
        if self.source.is_empty() {
            self.host()
        } else {
            self.source.clone()
        }
    }

    /// Whether `query` (already [`fold`]ed) is in its title or summary.
    pub fn matches(&self, query: &str) -> bool {
        query.split_whitespace().all(|word| {
            fold(&self.title).contains(word)
                || fold(&self.summary).contains(word)
                || fold(&self.source).contains(word)
        })
    }
}

/// `link` if it's one tuinews may open, show or fetch: `http` or `https`,
/// a host that's a name (not an address on the network: an IP, `localhost`,
/// `*.local`), no user or password (`https://vnexpress.net@evil.example`
/// reads as one site and goes to another), at most [`MAX_LINK`] long.
/// `base` resolves a relative link (`/tin-tuc/…`, `//cdn…`).
pub fn checked_link(link: &str, base: Option<&reqwest::Url>) -> Option<String> {
    let link = text::clean(link.trim());
    if link.is_empty() || link.len() > MAX_LINK {
        return None;
    }
    let url = match base {
        Some(base) => base.join(&link).ok()?,
        None => reqwest::Url::parse(&link).ok()?,
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || !public_host(&url)
    {
        return None;
    }
    let url = url.to_string();
    (url.len() <= MAX_LINK).then_some(url)
}

/// Whether `url`'s host is a name on the internet, not this computer or the
/// network it's on: no IP address, `localhost`, or names only a local
/// network has (`.local`, `.lan`, `.internal`, `.home.arpa`), and a dot in
/// it.
pub fn public_host(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    // An IPv6 address comes in brackets; an IPv4 one the URL parser has
    // already written out as four numbers, however it was spelled.
    if host.starts_with('[') || host.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let local = [
        "localhost",
        "local",
        "lan",
        "internal",
        "intranet",
        "home.arpa",
        "corp",
    ];
    host.contains('.')
        && !local
            .iter()
            .any(|l| host == *l || host.ends_with(&format!(".{l}")))
        // A name made of digits and dots is an IPv4 address written oddly.
        && !host.split('.').all(|part| part.bytes().all(|b| b.is_ascii_digit() || b == b'x'))
}

/// Remote text on one line: cleaned of what can't be shown (`text::clean`),
/// whitespace collapsed, at most `max` characters.
pub fn one_line(s: &str, max: usize) -> String {
    let cleaned = text::clean(text::first_chars(s, max.saturating_mul(4)));
    let joined = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    text::first_chars(&joined, max).to_string()
}

/// Remote text shown on lines of its own (a summary, an article's text): as
/// [`one_line`] cleans a title, but each line on its own, a blank line kept
/// between paragraphs (never two), at most `max` characters and
/// `MAX_LINES` lines.
pub fn lines(s: &str, max: usize) -> String {
    let cleaned = text::clean(text::first_chars(s, max.saturating_mul(2)));
    let mut kept: Vec<String> = Vec::new();
    for line in cleaned.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let blank = line.is_empty();
        if blank && kept.last().is_none_or(String::is_empty) {
            continue;
        }
        kept.push(line);
        if kept.len() >= MAX_LINES {
            break;
        }
    }
    while kept.last().is_some_and(String::is_empty) {
        kept.pop();
    }
    text::first_chars(&kept.join("\n"), max).to_string()
}

/// `text` in lower case without its accents, for searching: `Hà Nội` is
/// `ha noi`, `Đà Nẵng` is `da nang`, so a search typed without Vietnamese
/// accents finds them.
pub fn fold(text: &str) -> String {
    text.nfd()
        .filter(|c| !('\u{0300}'..='\u{036F}').contains(c))
        .map(|c| match c {
            'đ' | 'Đ' => 'd',
            c => c,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// `3 hours ago`, as the cards say it.
pub fn age(then: i64, now: i64) -> String {
    let seconds = now.saturating_sub(then).max(0);
    let (n, unit) = match seconds {
        0..60 => return "just now".into(),
        60..3_600 => (seconds / 60, "minute"),
        3_600..86_400 => (seconds / 3_600, "hour"),
        86_400..604_800 => (seconds / 86_400, "day"),
        604_800..2_629_800 => (seconds / 604_800, "week"),
        2_629_800..31_557_600 => (seconds / 2_629_800, "month"),
        _ => (seconds / 31_557_600, "year"),
    };
    let s = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{s} ago")
}

/// `3h`, `2d`: an age where there's little room.
pub fn short_age(then: i64, now: i64) -> String {
    let seconds = now.saturating_sub(then).max(0);
    match seconds {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        86_400..2_629_800 => format!("{}d", seconds / 86_400),
        _ => format!("{}mo", seconds / 2_629_800),
    }
}

/// When it was published, on the reader's page: `Sat 24 Oct 2026, 08:15`, in
/// this computer's time zone.
pub fn when(at: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(at, 0) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => {
            t.format("%a %-d %b %Y, %H:%M").to_string()
        }
        chrono::LocalResult::None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_short_hex_of_the_link_and_only_those_read_back() {
        let id = ArticleId::of("https://vnexpress.net/a-5135847.html");
        assert_eq!(id.as_str().len(), 16);
        assert_eq!(ArticleId::parse(id.as_str()), Some(id.clone()));
        assert_eq!(id, ArticleId::of("https://vnexpress.net/a-5135847.html"));
        assert_ne!(id, ArticleId::of("https://vnexpress.net/b.html"));
        for bad in ["../../etc/passwd", "0123456789ABCDEF", "0123", ""] {
            assert_eq!(ArticleId::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_links_to_named_hosts_on_the_web_are_kept() {
        let base = reqwest::Url::parse("https://vnexpress.net/rss/tin-moi-nhat.rss").unwrap();
        assert_eq!(
            checked_link("/thoi-su/a.html", Some(&base)).as_deref(),
            Some("https://vnexpress.net/thoi-su/a.html")
        );
        assert_eq!(
            checked_link("//i1-vnexpress.vnecdn.net/x.jpg", Some(&base)).as_deref(),
            Some("https://i1-vnexpress.vnecdn.net/x.jpg")
        );
        for bad in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "http://127.0.0.1:8080/admin",
            "https://192.168.1.1/",
            "http://[::1]/",
            "http://localhost/",
            "https://router.local/",
            "https://2130706433/",
            "https://0x7f.1/",
            "https://vnexpress.net@evil.example/",
            "https://user:pw@vnexpress.net/",
            "data:text/html,hi",
        ] {
            assert_eq!(checked_link(bad, None), None, "{bad}");
        }
        let long = format!("https://vnexpress.net/{}", "a".repeat(MAX_LINK));
        assert_eq!(checked_link(&long, None), None);
    }

    #[test]
    fn searching_ignores_case_and_vietnamese_accents() {
        assert_eq!(fold("Hà Nội"), "ha noi");
        assert_eq!(fold("ĐÀ NẴNG"), "da nang");
        assert_eq!(fold("Thời sự"), "thoi su");
    }

    #[test]
    fn text_is_cleaned_and_capped() {
        assert_eq!(one_line("  a\n\tb \u{1b}[2J c ", 100), "a b [2J c");
        assert_eq!(one_line("abcdef", 3), "abc");
        assert_eq!(lines("one\n\n\n\ntwo\n\n", 100), "one\n\ntwo");
    }

    #[test]
    fn ages_read_as_people_say_them() {
        assert_eq!(age(0, 30), "just now");
        assert_eq!(age(0, 2 * 3600), "2 hours ago");
        assert_eq!(age(0, 86_400), "1 day ago");
        assert_eq!(short_age(0, 90 * 60), "1h");
        assert_eq!(age(100, 0), "just now", "a date ahead is now");
    }
}
