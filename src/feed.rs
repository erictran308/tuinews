//! Feeds: fetched with the last answer's ETag and date, so a feed that
//! hasn't changed costs a few bytes, then read as RSS 2.0, RSS 1.0 (RDF) or
//! Atom, leniently: every field optional, a broken end keeps what came
//! before it. Also the pages of articles you open, for their text.
//!
//! Everything a feed says is untrusted: titles and summaries become plain
//! text (`html`, `article::one_line`), links and pictures are kept only if
//! `article::checked_link` passes them, and every read is capped.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::article::{self, checked_link};
use crate::html::{self, Block};

/// The biggest feed read. VietnamNet's carry a thousand articles, 1.2 MB.
pub const MAX_FEED: usize = 8 * 1024 * 1024;
/// Articles kept from one fetch of a feed: its newest.
pub const MAX_ITEMS: usize = 100;
/// Feeds fetched at once: the default categories' sections make some 60.
pub const PARALLEL: usize = 6;
/// Elements a feed may nest, at most.
const MAX_DEPTH: usize = 64;
/// The HTML of one summary read, at most, before it's made text.
const MAX_FIELD: usize = 256 * 1024;
/// Redirects followed for one request.
const MAX_REDIRECTS: usize = 5;
/// An article's text a feed carries counts as the whole article from this
/// many characters (or three paragraphs): less is a teaser.
const MIN_BODY: usize = 600;
/// Vietnam's time (UTC+7): a date with no zone in a feed from a `.vn` site
/// is taken to be in it (Tuổi Trẻ writes `10/24/2026 6:26:00 AM`).
const VIETNAM: i32 = 7 * 3600;

/// How tuinews says who it is. Some news sites turn away requests that
/// don't look like a browser's, so this names one as well.
pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; tuinews/",
    env!("CARGO_PKG_VERSION"),
    "; RSS reader)"
);

/// The HTTP client for feeds, pictures and pages: rustls with ring, no
/// cookies, no `Referer`, timeouts, the proxy chosen at start (`proxy`),
/// and redirects only to links `article::checked_link` passes, never from
/// https to plain http, at most [`MAX_REDIRECTS`].
pub fn client() -> reqwest::Client {
    // ring does the cryptography; the first client sets it for the process.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .connect_timeout(Duration::from_secs(10))
        .referer(false)
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let secure = attempt.previous().iter().any(|u| u.scheme() == "https");
            let url = attempt.url();
            let allowed =
                checked_link(url.as_str(), None).is_some() && !(secure && url.scheme() == "http");
            if attempt.previous().len() <= MAX_REDIRECTS && allowed {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }));
    match crate::proxy::get().and_then(|p| reqwest::Proxy::all(p.url.as_str()).ok()) {
        Some(proxy) => builder.proxy(proxy.no_proxy(reqwest::NoProxy::from_env())),
        None => builder.no_proxy(),
    }
    .build()
    .expect("the TLS backend is built in")
}

/// A feed, as read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Feed {
    pub title: String,
    pub items: Vec<Item>,
}

/// One of a feed's articles, as read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    pub title: String,
    pub link: String,
    pub summary: String,
    pub image: Option<String>,
    pub author: String,
    pub category: String,
    pub published: Option<i64>,
    /// The article's own text, when the feed carries it whole (WordPress's
    /// `content:encoded`, Tuoi Tre News's `content`): the reader needs no
    /// page then.
    pub body: Vec<Block>,
}

/// What fetching a feed got.
#[derive(Debug)]
pub enum Fetched {
    /// The same as last time (HTTP 304).
    Unchanged,
    New {
        feed: Feed,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

/// The site asked tuinews to slow down (HTTP 429).
#[derive(Debug)]
pub struct TooManyRequests;

impl std::fmt::Display for TooManyRequests {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the site asked to slow down (HTTP 429)")
    }
}

impl std::error::Error for TooManyRequests {}

/// Fetches the feed at `url` (a checked link), sending what the last answer
/// said, so an unchanged one answers 304.
pub async fn fetch(
    client: &reqwest::Client,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<Fetched> {
    let base = reqwest::Url::parse(url).context("not a link")?;
    let mut request = client.get(base.clone()).header(
        reqwest::header::ACCEPT,
        "application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.9, */*;q=0.5",
    );
    if let Some(etag) = etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    if let Some(at) = last_modified {
        request = request.header(reqwest::header::IF_MODIFIED_SINCE, at);
    }
    let response = request.send().await.map_err(plain)?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Fetched::Unchanged);
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(TooManyRequests.into());
    }
    if !status.is_success() {
        bail!("HTTP {}", status.as_u16());
    }
    let header = |name| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| article::one_line(v, 200))
            .filter(|v| !v.is_empty())
    };
    let etag = header(reqwest::header::ETAG);
    let last_modified = header(reqwest::header::LAST_MODIFIED);
    // Redirected: links in it are relative to where it ended up.
    let base = response.url().clone();
    let bytes = read_capped(response, MAX_FEED).await?;
    let vietnamese = base.host_str().is_some_and(|h| h.ends_with(".vn"));
    let mut feed = parse(&decode(&bytes), &base, vietnamese)?;
    if vietnamese {
        in_vietnams_time(&mut feed, chrono::Utc::now().timestamp());
    }
    Ok(Fetched::New {
        feed,
        etag,
        last_modified,
    })
}

/// The feed at `url`, or the one the page there names (as browsers find a
/// site's feed: `<link rel="alternate" type="application/rss+xml">`):
/// where it is, and what it holds. For adding a feed from a site's address.
pub async fn discover(client: &reqwest::Client, url: &str) -> Result<(String, Feed)> {
    let first = match fetch(client, url, None, None).await {
        Ok(Fetched::New { feed, .. }) => return Ok((url.to_string(), feed)),
        Ok(Fetched::Unchanged) => anyhow::anyhow!("no answer"),
        Err(e) if e.is::<TooManyRequests>() => return Err(e),
        Err(e) => e,
    };
    let Ok(page) = page(client, url).await else {
        return Err(first);
    };
    let base = reqwest::Url::parse(url).context("not a link")?;
    for link in html::feeds_named(&page, &base).into_iter().take(3) {
        if let Ok(Fetched::New { feed, .. }) = fetch(client, &link, None, None).await {
            return Ok((link, feed));
        }
    }
    bail!("it isn't a feed, and its page names none")
}

/// An article's page, as text to read its words from: only HTML, at most
/// [`html::MAX_PAGE`].
pub async fn page(client: &reqwest::Client, link: &str) -> Result<String> {
    let response = client
        .get(link)
        .header(reqwest::header::ACCEPT, "text/html, application/xhtml+xml")
        .send()
        .await
        .map_err(plain)?;
    let status = response.status();
    if !status.is_success() {
        bail!("HTTP {}", status.as_u16());
    }
    let kind = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/html")
        .to_ascii_lowercase();
    if !kind.contains("html") {
        bail!("not a web page");
    }
    let charset = kind
        .split(';')
        .find_map(|part| part.trim().strip_prefix("charset="))
        .map(|c| c.trim_matches('"').to_string());
    let bytes = read_capped(response, html::MAX_PAGE).await?;
    Ok(decode_page(&bytes, charset.as_deref()))
}

/// A network error in a few words, without the URL reqwest puts in it.
fn plain(e: reqwest::Error) -> anyhow::Error {
    let what = if e.is_timeout() {
        "it took too long to answer"
    } else if e.is_connect() {
        "couldn't connect"
    } else if e.is_redirect() {
        "it redirected somewhere tuinews won't go"
    } else {
        "the request failed"
    };
    anyhow::anyhow!(what)
}

/// The body of `response`, refused once it's past `max` bytes.
async fn read_capped(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > max as u64) {
        bail!("too big");
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(plain)? {
        if data.len() + chunk.len() > max {
            bail!("too big");
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

/// A feed's bytes as text: by its byte order mark, else the encoding its
/// XML declaration names, else UTF-8 (a bad byte becomes �).
pub fn decode(bytes: &[u8]) -> String {
    if let Some((encoding, bom)) = encoding_rs::Encoding::for_bom(bytes) {
        return encoding
            .decode_without_bom_handling(&bytes[bom..])
            .0
            .into_owned();
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]);
    let declared = head
        .strip_prefix("<?xml")
        .and_then(|rest| rest.split("?>").next())
        .and_then(|decl| decl.split("encoding=").nth(1))
        .and_then(|value| {
            let quote = value.chars().next()?;
            value[1..].split(quote).next()
        });
    match declared.and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes())) {
        Some(encoding) if encoding != encoding_rs::UTF_8 => {
            encoding.decode_without_bom_handling(bytes).0.into_owned()
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// A page's bytes as text: by the charset its header or its `<meta>` names,
/// else UTF-8.
fn decode_page(bytes: &[u8], charset: Option<&str>) -> String {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).to_ascii_lowercase();
    let meta = head.split("charset=").nth(1).map(|rest| {
        rest.trim_start_matches(['"', '\''])
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .next()
            .unwrap_or_default()
            .to_string()
    });
    let label = charset.map(str::to_string).or(meta);
    match label.and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())) {
        Some(encoding) if encoding != encoding_rs::UTF_8 => encoding.decode(bytes).0.into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// What an item's text goes into, by the element it's in.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Field {
    Title,
    Link,
    Guid,
    Description,
    Content,
    Published,
    Updated,
    Author,
    Category,
    Thumb,
    FeedTitle,
}

/// An item while it's read.
#[derive(Default)]
struct Draft {
    title: String,
    link: String,
    guid: String,
    description: String,
    content: String,
    published: String,
    updated: String,
    author: String,
    category: String,
    /// Pictures the feed names, best first by where they came from.
    pictures: Vec<(u8, String)>,
}

impl Draft {
    fn field(&mut self, field: Field) -> Option<&mut String> {
        Some(match field {
            Field::Title => &mut self.title,
            Field::Link => &mut self.link,
            Field::Guid => &mut self.guid,
            Field::Description => &mut self.description,
            Field::Content => &mut self.content,
            Field::Published => &mut self.published,
            Field::Updated => &mut self.updated,
            Field::Author => &mut self.author,
            Field::Category => &mut self.category,
            Field::Thumb | Field::FeedTitle => return None,
        })
    }

    fn item(self, base: &reqwest::Url, zone: FixedOffset) -> Option<Item> {
        let guid = self.guid.trim();
        let link = checked_link(&self.link, Some(base)).or_else(|| {
            guid.contains("://")
                .then(|| checked_link(guid, Some(base)))
                .flatten()
        })?;
        let summary_html = if self.description.trim().is_empty() {
            &self.content
        } else {
            &self.description
        };
        let mut summary = html::summary(summary_html);
        // A summary of a dot or a dash (SGGP's, now and then) says nothing.
        if !summary.chars().any(char::is_alphanumeric) {
            summary.clear();
        }
        let mut title = article::one_line(&html::summary(&self.title), article::MAX_TITLE);
        if title.is_empty() {
            title = article::one_line(&summary, 120);
        }
        if title.is_empty() {
            return None;
        }
        let mut pictures = self.pictures;
        for (rank, html) in [(4, &self.description), (5, &self.content)] {
            if let Some(src) = html::first_image(html) {
                pictures.push((rank, src));
            }
        }
        pictures.sort_by_key(|(rank, _)| *rank);
        let image = pictures.iter().find_map(|(_, url)| picture_url(url, &link));
        let date = |s: &str| parse_date(s, zone);
        // A whole article, not a teaser: a few paragraphs, or a long one.
        let body = if self.content.contains("<p") {
            let blocks = html::article_text(&self.content, &link);
            let length: usize = blocks.iter().map(|b| b.text().chars().count()).sum();
            if blocks.len() >= 3 || length >= MIN_BODY {
                blocks
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        Some(Item {
            title,
            link,
            summary,
            image,
            author: article::one_line(&html::summary(&self.author), article::MAX_BYLINE),
            category: article::one_line(&html::summary(&self.category), article::MAX_BYLINE),
            published: date(&self.published).or_else(|| date(&self.updated)),
            body,
        })
    }
}

/// A picture's link, if it's one tuinews may fetch, relative to its
/// article's, and over https: pictures are never asked for in the clear
/// (`images::allowed`), and news sites' image servers all answer https. An
/// AVIF picture is asked for as the JPEG, PNG or WebP it was made from
/// (`a.jpg.avif` is `a.jpg`): news sites serve both, and tuinews decodes no
/// AVIF.
pub fn picture_url(url: &str, article: &str) -> Option<String> {
    let base = reqwest::Url::parse(article).ok();
    let mut url = reqwest::Url::parse(&checked_link(url, base.as_ref())?).ok()?;
    if url.scheme() == "http" {
        url.set_scheme("https").ok()?;
    }
    let path = url.path().to_string();
    if let Some(original) = path.strip_suffix(".avif")
        && [".jpg", ".jpeg", ".png", ".webp", ".gif"]
            .iter()
            .any(|ext| original.to_ascii_lowercase().ends_with(ext))
    {
        url.set_path(original);
    }
    Some(url.to_string())
}

/// The element's local name, without its prefix (`media:content` is
/// `content`), and its prefix.
fn names(e: &BytesStart) -> (String, String) {
    let name = e.name().as_ref().to_ascii_lowercase();
    match name.split_once(':') {
        Some((prefix, local)) => (local.to_string(), prefix.to_string()),
        None => (name, String::new()),
    }
}

/// An attribute's value, entities resolved.
fn attr(e: &BytesStart, key: &str) -> Option<String> {
    let a = e.try_get_attribute(key).ok()??;
    let value = a
        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
        .ok()?;
    Some(value.into_owned())
}

/// Reads a feed. `base` is where it was fetched from: relative links are
/// relative to it. `vietnamese`: a date with no zone is Vietnam's.
pub fn parse(xml: &str, base: &reqwest::Url, vietnamese: bool) -> Result<Feed> {
    let zone = FixedOffset::east_opt(if vietnamese { VIETNAM } else { 0 }).expect("in range");
    let mut reader = Reader::from_str(xml);
    let config = reader.config_mut();
    config.check_end_names = false;
    config.allow_unmatched_ends = true;
    let mut depth = 0usize;
    let mut saw_feed = false;
    let mut feed = Feed::default();
    let mut draft: Option<(Draft, usize)> = None;
    // The field being read, and how deep its element is.
    let mut reading: Option<(Field, usize)> = None;
    let mut text = String::new();
    loop {
        let event = match reader.read_event() {
            Ok(event) => event,
            // What came before a mistake is kept.
            Err(_) if saw_feed => break,
            Err(e) => return Err(e).context("not a feed"),
        };
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                if !empty {
                    depth += 1;
                    if depth > MAX_DEPTH {
                        bail!("the feed nests too deep");
                    }
                }
                let (name, prefix) = names(e);
                if matches!(name.as_str(), "rss" | "rdf" | "feed") && depth <= 1 {
                    saw_feed = true;
                }
                // Markup inside a field (Atom's XHTML): its words count.
                if reading.is_some() {
                    text.push(' ');
                    continue;
                }
                // Tuoi Tre News calls its items `article`.
                let item_start = match name.as_str() {
                    "item" | "entry" => true,
                    "article" => draft.is_none(),
                    _ => false,
                };
                if item_start && prefix.is_empty() && !empty {
                    draft = Some((Draft::default(), depth));
                    continue;
                }
                let Some((item, item_depth)) = draft.as_mut() else {
                    // The feed's own title, right under the channel or feed.
                    if name == "title" && prefix.is_empty() && !empty && feed.title.is_empty() {
                        reading = Some((Field::FeedTitle, depth));
                        text.clear();
                    }
                    continue;
                };
                let child = depth == *item_depth + 1 || (empty && depth == *item_depth);
                // Pictures, from their attributes.
                let picture = match (prefix.as_str(), name.as_str()) {
                    ("", "enclosure") => attr(e, "type")
                        .is_none_or(|t| t.starts_with("image/"))
                        .then(|| attr(e, "url"))
                        .flatten()
                        .map(|url| (0, url)),
                    ("media", "content") => {
                        let image = attr(e, "medium").as_deref() == Some("image")
                            || attr(e, "type").is_some_and(|t| t.starts_with("image/"))
                            || (attr(e, "medium").is_none() && attr(e, "type").is_none());
                        image.then(|| attr(e, "url")).flatten().map(|url| (1, url))
                    }
                    ("media", "thumbnail") => attr(e, "url").map(|url| (2, url)),
                    // RSS's source is the feed an item came from, but
                    // Tuoi Tre News puts its picture there.
                    ("", "source") => attr(e, "type")
                        .is_some_and(|t| t.starts_with("image/"))
                        .then(|| attr(e, "url"))
                        .flatten()
                        .map(|url| (0, url)),
                    _ => None,
                };
                if let Some(picture) = picture {
                    item.pictures.push(picture);
                }
                // Atom's link and category are attributes.
                if prefix.is_empty()
                    && name == "link"
                    && let Some(href) = attr(e, "href")
                {
                    let rel = attr(e, "rel").unwrap_or_else(|| "alternate".into());
                    if rel == "alternate" && item.link.is_empty() {
                        item.link = href;
                    } else if rel == "enclosure"
                        && attr(e, "type").is_some_and(|t| t.starts_with("image/"))
                    {
                        item.pictures.push((0, href));
                    }
                }
                if prefix.is_empty()
                    && name == "category"
                    && item.category.is_empty()
                    && let Some(term) = attr(e, "term")
                {
                    item.category = term;
                }
                if empty {
                    continue;
                }
                let field = match (prefix.as_str(), name.as_str()) {
                    ("", "title") if child => Some(Field::Title),
                    ("", "link") if child => Some(Field::Link),
                    ("", "guid" | "id") if child => Some(Field::Guid),
                    ("", "description" | "summary" | "headline") if child => {
                        Some(Field::Description)
                    }
                    ("content", "encoded") | ("", "content") if child => Some(Field::Content),
                    ("", "pubdate" | "published") | ("dc", "date") if child => {
                        Some(Field::Published)
                    }
                    ("", "updated") if child => Some(Field::Updated),
                    ("dc", "creator") | ("", "author") if child => Some(Field::Author),
                    // Atom's author is a name inside an element.
                    ("", "name") if depth == *item_depth + 2 => Some(Field::Author),
                    ("", "category") if child && item.category.is_empty() => Some(Field::Category),
                    ("", "thumb" | "image") if child => Some(Field::Thumb),
                    _ => None,
                };
                if let Some(field) = field {
                    reading = Some((field, depth));
                    text.clear();
                }
            }
            Event::End(_) => {
                if let Some((field, at)) = reading
                    && at == depth
                {
                    reading = None;
                    let value = std::mem::take(&mut text);
                    match (field, draft.as_mut()) {
                        (Field::FeedTitle, _) => {
                            feed.title =
                                article::one_line(&html::summary(&value), article::MAX_SOURCE);
                        }
                        (Field::Thumb, Some((item, _))) => {
                            item.pictures.push((3, value.trim().to_string()))
                        }
                        (field, Some((item, _))) => {
                            if let Some(slot) = item.field(field)
                                && slot.is_empty()
                            {
                                *slot = value;
                            }
                        }
                        _ => {}
                    }
                }
                if let Some((_, at)) = &draft
                    && *at == depth
                    && let Some((done, _)) = draft.take()
                    && let Some(item) = done.item(base, zone)
                    && !feed.items.iter().any(|i| i.link == item.link)
                {
                    feed.items.push(item);
                }
                depth = depth.saturating_sub(1);
            }
            Event::Text(t) if reading.is_some() => push_capped(&mut text, &t.xml10_content()),
            Event::CData(t) if reading.is_some() => push_capped(&mut text, &t.into_inner()),
            Event::GeneralRef(r) if reading.is_some() => {
                let c = match r.resolve_char_ref() {
                    Ok(Some(c)) => Some(c),
                    _ => entity(r.as_ref()),
                };
                if let Some(c) = c {
                    push_capped(&mut text, c.encode_utf8(&mut [0; 4]));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_feed {
        bail!("not a feed");
    }
    // The newest, when a feed holds more than is kept.
    if feed.items.len() > MAX_ITEMS {
        feed.items
            .sort_by_key(|item| std::cmp::Reverse(item.published.unwrap_or(0)));
        feed.items.truncate(MAX_ITEMS);
    }
    Ok(feed)
}

/// Dates from hours ahead, put right. A Vietnamese site's server can write
/// Vietnam's time with the zone of UTC (one of Tuoi Tre News's writes
/// `08:06:00 +0000` for 08:06 in Hanoi, at 01:06 UTC; another, `+0700`),
/// so its newest articles would come from the future. When the newest is
/// more than an hour ahead of `now` and Vietnam's time would put it right,
/// every date in the feed is taken as Vietnam's: they come from one server.
pub fn in_vietnams_time(feed: &mut Feed, now: i64) {
    let Some(newest) = feed.items.iter().filter_map(|i| i.published).max() else {
        return;
    };
    let fixed = newest - i64::from(VIETNAM);
    if newest > now + 3600 && fixed <= now + 3600 {
        for item in &mut feed.items {
            if let Some(at) = item.published.as_mut() {
                *at -= i64::from(VIETNAM);
            }
        }
    }
}

fn push_capped(field: &mut String, text: &str) {
    if field.len() + text.len() <= MAX_FIELD {
        field.push_str(text);
    }
}

/// The character an entity names: XML's own, and the HTML ones feeds put
/// outside their CDATA (`&nbsp;`, `&hellip;`).
fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "hellip" => '…',
        "ndash" => '–',
        "mdash" => '—',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "middot" => '·',
        "copy" => '©',
        "deg" => '°',
        _ => return None,
    })
}

/// When an article was published, from what its feed says: RFC 2822 as RSS
/// wants (with the zones news sites write, `+07:00`, `+07`, `GMT+7`, and a
/// two-digit year), RFC 3339 as Atom does, and the dates some sites write
/// with no zone, taken to be in `zone`. Only 1990 to 2100.
pub fn parse_date(s: &str, zone: FixedOffset) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() || s.len() > 100 {
        return None;
    }
    let at = rfc2822(s)
        .or_else(|| DateTime::parse_from_rfc3339(s).ok())
        .or_else(|| DateTime::parse_from_rfc3339(&s.replacen(' ', "T", 1)).ok())
        .or_else(|| zoneless(s, zone))?;
    let at = at.timestamp();
    (631_152_000..4_102_444_800).contains(&at).then_some(at)
}

fn rfc2822(s: &str) -> Option<DateTime<FixedOffset>> {
    if let Ok(at) = DateTime::parse_from_rfc2822(s) {
        return Some(at);
    }
    // The zone as news sites write it, made RFC 2822's: `+07:00` and `+07`
    // are `+0700`, `GMT+7` is `+0700`.
    let (rest, zone) = s.rsplit_once(' ')?;
    let zone = zone
        .strip_prefix("GMT")
        .filter(|z| !z.is_empty())
        .unwrap_or(zone);
    let fixed = match zone.as_bytes() {
        [sign @ (b'+' | b'-'), ..] => {
            let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
            let digits = match digits.len() {
                1 => format!("0{digits}00"),
                2 => format!("{digits}00"),
                4 => digits,
                _ => return None,
            };
            format!("{rest} {}{digits}", *sign as char)
        }
        _ => s.to_string(),
    };
    DateTime::parse_from_rfc2822(&fixed).ok().or_else(|| {
        // The day of the week, wrong or in another language, is dropped;
        // the month may be written out (`24 October 2026`).
        let (_, without_day) = fixed.split_once(", ")?;
        DateTime::parse_from_rfc2822(without_day)
            .or_else(|_| DateTime::parse_from_str(without_day, "%d %B %Y %H:%M:%S %z"))
            .ok()
    })
}

fn zoneless(s: &str, zone: FixedOffset) -> Option<DateTime<FixedOffset>> {
    const FORMATS: [&str; 7] = [
        "%m/%d/%Y %I:%M:%S %p",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%d/%m/%Y %H:%M:%S",
        "%d/%m/%Y %H:%M",
        "%a, %d %b %Y %H:%M:%S",
    ];
    FORMATS.iter().find_map(|format| {
        let naive = NaiveDateTime::parse_from_str(s, format).ok()?;
        zone.from_local_datetime(&naive).single()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(url: &str) -> reqwest::Url {
        reqwest::Url::parse(url).unwrap()
    }

    fn vietnam() -> FixedOffset {
        FixedOffset::east_opt(VIETNAM).unwrap()
    }

    #[test]
    fn dates_are_read_as_vietnamese_sites_write_them() {
        let z = vietnam();
        let at = 1_792_804_539; // Sat, 24 Oct 2026 08:15:39 +07:00
        for s in [
            "Sat, 24 Oct 2026 08:15:39 +0700",
            "Sat, 24 Oct 2026 08:15:39 +07:00",
            "Sat, 24 Oct 2026 08:15:39 +07",
            "Sat, 24 Oct 2026 08:15:39 GMT+7",
            "Sat, 24 Oct 26 08:15:39 +0700",
            "Sun, 24 Oct 2026 08:15:39 +0700",
            "2026-10-24T08:15:39+07:00",
            "2026-10-24T01:15:39Z",
            "2026-10-24 08:15:39",
            "Saturday, 24 October 2026 08:15:39 +0700",
        ] {
            assert_eq!(parse_date(s, z), Some(at), "{s}");
        }
        // Tuổi Trẻ's: no zone, in Vietnam's time.
        assert_eq!(parse_date("10/24/2026 8:15:39 AM", z), Some(at), "Tuổi Trẻ");
        assert_eq!(
            parse_date("Friday, 23 October 2026 22:28:00 +0700", z),
            Some(1_792_769_280),
            "Tuoi Tre News, in the evening"
        );
        assert_eq!(parse_date("not a date", z), None);
        assert_eq!(parse_date("Mon, 01 Jan 1900 00:00:00 +0000", z), None);
    }

    #[test]
    fn an_rss_feed_gives_its_articles_with_their_pictures() {
        let xml = include_str!("../testdata/vnexpress.xml");
        let feed = parse(
            xml,
            &base("https://vnexpress.net/rss/tin-moi-nhat.rss"),
            true,
        )
        .unwrap();
        assert_eq!(feed.title, "Tin mới nhất - VnExpress RSS");
        assert_eq!(feed.items.len(), 3);
        let first = &feed.items[0];
        assert_eq!(
            first.title,
            "SpaceX muốn tiếp cận 80.000 trạm thu phát sóng di động ở Mỹ"
        );
        assert_eq!(
            first.link,
            "https://vnexpress.net/spacex-muon-tiep-can-80-000-tram-thu-phat-song-di-dong-o-my-5135847.html"
        );
        assert!(
            first.summary.starts_with("SpaceX đang đàm phán"),
            "{}",
            first.summary
        );
        assert!(
            first
                .image
                .as_deref()
                .unwrap()
                .starts_with("https://i1-vnexpress.vnecdn.net/2026/10/23/bts-"),
            "the enclosure"
        );
        assert_eq!(first.published, Some(1_792_742_400));
    }

    #[test]
    fn cdata_links_odd_dates_and_avif_pictures_are_read() {
        let xml = include_str!("../testdata/mixed.xml");
        let feed = parse(xml, &base("https://tuoitre.vn/rss/tin-moi-nhat.rss"), true).unwrap();
        let items = &feed.items;
        assert_eq!(items.len(), 4, "{items:#?}");
        // Tuổi Trẻ: the link in CDATA, a zoneless US date.
        assert_eq!(items[0].link, "https://tuoitre.vn/a-100261023231810945.htm");
        assert_eq!(items[0].author, "HUY ĐĂNG");
        assert!(items[0].published.is_some());
        // Tiền Phong: the AVIF picture asked for as its JPEG, related links
        // left out of the summary.
        assert_eq!(
            items[1].image.as_deref(),
            Some("https://cdn.tienphong.vn/images/x/sap-nha.jpg")
        );
        assert_eq!(items[1].summary, "Một tòa nhà cao tầng đổ sập.");
        // Only a guid, and an entity outside CDATA.
        assert_eq!(items[2].link, "https://example.vn/only-guid.html");
        assert_eq!(items[2].title, "A & B… “quoted”");
        // A link elsewhere than the web is dropped, with its article; a
        // picture on this computer is dropped alone.
        assert!(items.iter().all(|i| !i.link.contains("127.0.0.1")));
        assert_eq!(items[3].image, None);
        assert_eq!(items[3].title, "Picture on the network");
    }

    #[test]
    fn an_atom_feed_reads_too() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:media="http://search.yahoo.com/mrss/">
  <title>Example Atom</title>
  <entry>
    <title type="html">One &lt;b&gt;bold&lt;/b&gt;</title>
    <link rel="alternate" href="/posts/one"/>
    <link rel="enclosure" type="image/png" href="https://cdn.example.com/one.png"/>
    <id>urn:uuid:1</id>
    <updated>2026-10-24T01:15:39Z</updated>
    <author><name>Writer</name></author>
    <category term="Tech"/>
    <content type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml"><p>Hello <b>world</b></p></div></content>
  </entry>
</feed>"#;
        let feed = parse(xml, &base("https://example.com/feed.atom"), false).unwrap();
        assert_eq!(feed.title, "Example Atom");
        let item = &feed.items[0];
        assert_eq!(item.title, "One bold");
        assert_eq!(item.link, "https://example.com/posts/one");
        assert_eq!(
            item.image.as_deref(),
            Some("https://cdn.example.com/one.png")
        );
        assert_eq!(item.author, "Writer");
        assert_eq!(item.category, "Tech");
        assert_eq!(item.summary, "Hello world");
        assert_eq!(item.published, Some(1_792_804_539));
    }

    #[test]
    fn tuoi_tre_news_articles_and_their_whole_text_are_read() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title><![CDATA[Tuoi tre news - RSS Feed]]></title>
<article><title><![CDATA[Polish PM arrives in Da Nang]]></title><headline><![CDATA[Polish Prime Minister Donald Tusk arrived in Da Nang.]]></headline><content><![CDATA[<p>Vice-chairwoman Nguyen Thi Anh Thi greeted the delegation at Da Nang International Airport.</p><figure><img src="a.jpg"><figcaption>At the airport</figcaption></figure><p>The Polish PM was accompanied by the Minister of Science and Higher Education.</p><p>Ties between the two countries rest on a long history and political trust.</p>]]></content><pubDate>Saturday, 24 October 2026 08:06:00 +0700</pubDate><source url="https://cdn2.tuoitre.vn/zoom/1200_900/a.png" length="0" type="image/jpeg" /><link><![CDATA[https://news.tuoitre.vn/polish-pm-103261024080024247.htm]]></link></article>
</channel></rss>"#;
        let feed = parse(xml, &base("https://tuoitrenews.vn/rss/home.rss"), true).unwrap();
        let item = &feed.items[0];
        assert_eq!(item.title, "Polish PM arrives in Da Nang");
        assert_eq!(
            item.summary,
            "Polish Prime Minister Donald Tusk arrived in Da Nang."
        );
        assert_eq!(
            item.image.as_deref(),
            Some("https://cdn2.tuoitre.vn/zoom/1200_900/a.png")
        );
        assert!(item.published.is_some());
        assert_eq!(item.body.len(), 5, "{:#?}", item.body);
        assert_eq!(
            item.body[1],
            Block::Picture("https://news.tuoitre.vn/a.jpg".into())
        );
        assert_eq!(item.body[2], Block::Aside("At the airport".into()));
    }

    #[test]
    fn a_page_that_isnt_a_feed_is_said_to_be_so() {
        let page = "<!doctype html><html><head><title>Lỗi 404</title></head></html>";
        assert!(parse(page, &base("https://example.vn/rss"), true).is_err());
        assert!(parse("RSS feed not found", &base("https://example.vn/rss"), true).is_err());
    }

    #[test]
    fn a_feed_in_utf16_is_read() {
        let text = "<?xml version=\"1.0\" encoding=\"utf-16\"?><rss><channel><title>Việt</title></channel></rss>";
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode(&bytes), text);
        assert_eq!(decode("héllo".as_bytes()), "héllo");
    }

    #[test]
    fn a_feed_with_more_than_is_kept_keeps_its_newest() {
        let items: String = (0..150)
            .map(|i| {
                format!(
                    "<item><title>n{i}</title><link>https://e.vn/{i}</link><pubDate>{}</pubDate></item>",
                    chrono::DateTime::from_timestamp(1_700_000_000 + i * 60, 0).unwrap().to_rfc2822()
                )
            })
            .collect();
        let xml = format!("<rss><channel>{items}</channel></rss>");
        let feed = parse(&xml, &base("https://e.vn/rss"), true).unwrap();
        assert_eq!(feed.items.len(), MAX_ITEMS);
        assert_eq!(feed.items[0].title, "n149");
    }

    #[tokio::test]
    #[ignore = "talks to the news sites"]
    async fn live_an_articles_text_is_found_on_every_default_site() {
        let client = client();
        let mut missing = Vec::new();
        for default in crate::defaults::FEEDS {
            let Ok(Fetched::New { feed, .. }) = fetch(&client, default.url, None, None).await
            else {
                panic!("{}: no feed", default.name);
            };
            let item = &feed.items[0];
            let blocks = if item.body.is_empty() {
                match page(&client, &item.link).await {
                    Ok(page) => html::article_text(&page, &item.link),
                    Err(e) => {
                        missing.push(format!("{}: {e:#}", default.name));
                        continue;
                    }
                }
            } else {
                item.body.clone()
            };
            let length: usize = blocks.iter().map(|b| b.text().chars().count()).sum();
            let pictures = blocks
                .iter()
                .filter(|b| matches!(b, Block::Picture(_)))
                .count();
            let first = blocks
                .iter()
                .map(Block::text)
                .find(|t| !t.is_empty())
                .unwrap_or_default();
            println!(
                "{:<24} {:>3} blocks {:>2} pictures {:>6} chars  {}",
                default.name,
                blocks.len(),
                pictures,
                length,
                crate::text::first_chars(first, 50)
            );
            if length < 300 {
                missing.push(format!("{}: {length} chars at {}", default.name, item.link));
            }
        }
        assert!(missing.is_empty(), "{missing:#?}");
    }

    #[test]
    fn dates_hours_ahead_are_taken_as_vietnams_time() {
        let item = |at| Item {
            published: Some(at),
            ..Item::default()
        };
        // 08:06 and 22:28 the night before, written as UTC; now 02:04 UTC.
        let now = 1_792_807_476;
        let mut feed = Feed {
            title: String::new(),
            items: vec![item(1_792_829_160), item(1_792_794_480)],
        };
        in_vietnams_time(&mut feed, now);
        assert_eq!(feed.items[0].published, Some(1_792_829_160 - 7 * 3600));
        assert_eq!(feed.items[1].published, Some(1_792_794_480 - 7 * 3600));
        // Right already, or days ahead (not a zone): left be.
        let mut right = Feed {
            title: String::new(),
            items: vec![item(now - 60), item(now + 3 * 86_400)],
        };
        in_vietnams_time(&mut right, now);
        assert_eq!(right.items[1].published, Some(now + 3 * 86_400));
    }

    #[tokio::test]
    #[ignore = "talks to the news sites"]
    async fn live_every_default_feed_loads() {
        let client = client();
        let sections = crate::defaults::CATEGORIES.iter().flat_map(|category| {
            category
                .feeds
                .iter()
                .map(move |feed| (format!("{}: {}", category.name, feed.name), feed.url))
        });
        let all: Vec<(String, &str)> = crate::defaults::FEEDS
            .iter()
            .map(|feed| (feed.name.to_string(), feed.url))
            .chain(sections)
            .collect();
        let mut failed = Vec::new();
        for (name, url) in all {
            match fetch(&client, url, None, None).await {
                Ok(Fetched::New { feed: got, .. }) => {
                    let pictures = got.items.iter().filter(|i| i.image.is_some()).count();
                    let dated = got.items.iter().filter(|i| i.published.is_some()).count();
                    println!(
                        "{name:<26} {:>3} items, {pictures:>3} pictures, {dated:>3} dated",
                        got.items.len()
                    );
                    if got.items.len() < 10 || dated * 2 < got.items.len() {
                        failed.push(format!("{name}: {} items, {dated} dated", got.items.len()));
                    }
                }
                other => failed.push(format!("{name}: {other:?}")),
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
    }
}
