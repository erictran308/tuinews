//! HTML as text: what a feed says of an article (`summary`), the picture
//! in it (`first_image`), and an article's own text, read from its page
//! (`article_text`).
//!
//! Only text comes out: tags, scripts, styles, forms and links are gone,
//! and what's left goes through `article::lines`, so nothing in it can
//! reach the terminal as anything but text.

use std::collections::{HashMap, HashSet};

use ego_tree::NodeRef;
use scraper::{ElementRef, Html, Node};

use crate::article;

/// An article's text, longest kept, in characters.
pub const MAX_BODY: usize = 60_000;
/// Paragraphs of an article kept, at most.
const MAX_BLOCKS: usize = 400;
/// The biggest page read for an article's text.
pub const MAX_PAGE: usize = 5 * 1024 * 1024;
/// A paragraph shorter than this doesn't count when looking for where the
/// text is: a menu's items, a date, a caption.
const MIN_COUNTED: usize = 40;
/// Elements above one looked at, at most, to say whether it's in a menu, a
/// box of links or a link: pages nest some 30 deep. Deeper than this, it's
/// taken to be in one ([`near`]), and a page nested thousands deep doesn't
/// make each look take long.
const MAX_ANCESTORS: usize = 256;

/// Elements whose text is never anyone's to read: code, styles, forms and
/// the like, and lists, which in a feed's summary are links to other
/// articles (Tiền Phong's, VietnamPlus's "related" lists).
const SKIPPED: [&str; 14] = [
    "script", "style", "noscript", "template", "iframe", "svg", "button", "form", "select",
    "textarea", "object", "video", "audio", "canvas",
];

/// Elements that end a line.
const BLOCKS: [&str; 22] = [
    "p",
    "div",
    "br",
    "li",
    "ul",
    "ol",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "blockquote",
    "tr",
    "table",
    "section",
    "article",
    "figure",
    "figcaption",
    "header",
    "footer",
    "pre",
];

/// What a feed says of an article, as text: paragraphs on lines of their
/// own, lists (other articles' links) left out. Entities escaped twice, as
/// VietnamNet's are in CDATA (`&amp;apos;`), are read as one.
pub fn summary(html: &str) -> String {
    let text = as_text(html);
    if !text.contains('<') && holds_entity(&text) {
        return as_text(&text);
    }
    text
}

fn as_text(html: &str) -> String {
    // Plain text, as many feeds send: nothing to parse.
    if !html.contains('<') && !html.contains('&') {
        return article::lines(html, article::MAX_SUMMARY);
    }
    let fragment = Html::parse_fragment(html);
    let mut out = String::new();
    text_into(*fragment.root_element(), &mut out, &["ul", "ol", "table"]);
    article::lines(&out, article::MAX_SUMMARY)
}

/// Whether `text` holds what reads as an HTML entity: `&apos;`, `&#39;`.
fn holds_entity(text: &str) -> bool {
    text.match_indices('&').any(|(i, _)| {
        let rest = &text[i + 1..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '#')
            .collect();
        !name.is_empty() && rest[name.len()..].starts_with(';')
    })
}

/// Writes the text under `node` into `out`, a line break after each block,
/// leaving out [`SKIPPED`] elements and those in `also`. With a stack, not
/// recursion: a feed or a page may nest elements deeper than a thread's
/// stack goes.
fn text_into(node: NodeRef<Node>, out: &mut String, also: &[&str]) {
    enum Step<'a> {
        Into(NodeRef<'a, Node>),
        /// A block's end.
        Out,
    }
    let mut steps: Vec<Step> = node.children().rev().map(Step::Into).collect();
    while let Some(step) = steps.pop() {
        let Step::Into(child) = step else {
            out.push('\n');
            continue;
        };
        match child.value() {
            Node::Text(text) => out.push_str(text),
            Node::Element(element) => {
                let name = element.name();
                if SKIPPED.contains(&name) || also.contains(&name) {
                    continue;
                }
                if BLOCKS.contains(&name) {
                    if !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                    steps.push(Step::Out);
                }
                steps.extend(child.children().rev().map(Step::Into));
            }
            _ => {}
        }
    }
}

/// The first picture in `html` (its `src`, or the `data-src` of a picture
/// that loads late), as written: `article::checked_link` checks it.
pub fn first_image(html: &str) -> Option<String> {
    if !html.contains("<img") && !html.contains("<IMG") {
        return None;
    }
    let fragment = Html::parse_fragment(html);
    fragment
        .root_element()
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter(|e| e.value().name() == "img")
        .find_map(|e| {
            ["src", "data-src", "data-original"]
                .iter()
                .filter_map(|a| e.attr(a))
                .find(|src| !src.trim().is_empty() && !src.starts_with("data:"))
                .map(str::to_string)
        })
}

/// The feeds a site's page names in its head (`<link rel="alternate"
/// type="application/rss+xml" href="…">`), as checked links.
pub fn feeds_named(page: &str, base: &reqwest::Url) -> Vec<String> {
    let document = Html::parse_document(page);
    document
        .root_element()
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter(|e| e.value().name() == "link")
        .filter(|e| {
            let rel = e.attr("rel").unwrap_or_default().to_ascii_lowercase();
            let kind = e.attr("type").unwrap_or_default().to_ascii_lowercase();
            rel.split_whitespace().any(|r| r == "alternate")
                && (kind.contains("rss") || kind.contains("atom"))
        })
        .filter_map(|e| article::checked_link(e.attr("href")?, Some(base)))
        .collect()
}

/// A piece of an article's text.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Paragraph(String),
    Heading(String),
    /// A picture's caption, or a quote: set apart from the text.
    Aside(String),
    /// A picture in the text: its address, checked and over https
    /// (`feed::picture_url`).
    Picture(String),
    /// A video in the text, played on Enter.
    Video(Video),
}

/// A video of an article: where it streams from (a file or an HLS
/// playlist), checked and over https ([`video_url`]), and the picture shown
/// until it plays, checked as a picture is.
#[derive(Clone, Debug, PartialEq)]
pub struct Video {
    pub url: String,
    pub poster: Option<String>,
}

/// What the store's copy of an article's text starts with. Copies without
/// it were made before videos were kept, and are read from the page again.
const STORED: &str = "v 3";
/// What a copy made before videos were kept starts with: the same lines, no
/// videos. Shown while its page can't be read again (`Block::decode_older`):
/// a feed's own text, or a page that's gone, can't be.
const STORED_BEFORE_VIDEOS: &str = "v 2";

impl Block {
    /// Its words; a picture or a video has none.
    pub fn text(&self) -> &str {
        match self {
            Block::Paragraph(t) | Block::Heading(t) | Block::Aside(t) => t,
            Block::Picture(_) | Block::Video(_) => "",
        }
    }

    /// An article's text as the store keeps it: a line per block, each
    /// marked with what it is, after [`STORED`].
    pub fn encode(blocks: &[Block]) -> String {
        std::iter::once(STORED.to_string())
            .chain(blocks.iter().map(|block| match block {
                Block::Paragraph(t) => format!("p {t}"),
                Block::Heading(t) => format!("h {t}"),
                Block::Aside(t) => format!("a {t}"),
                Block::Picture(url) => format!("i {url}"),
                Block::Video(Video { url, poster }) => match poster {
                    Some(poster) => format!("v {url} {poster}"),
                    None => format!("v {url}"),
                },
            }))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Blocks as [`Block::encode`] wrote them, cleaned and checked again;
    /// `None` for a copy made before videos were kept.
    pub fn decode(stored: &str) -> Option<Vec<Block>> {
        Self::decode_marked(stored, STORED)
    }

    /// A copy made before videos were kept, as it was, for when its page
    /// can't be read again for them.
    pub fn decode_older(stored: &str) -> Option<Vec<Block>> {
        Self::decode_marked(stored, STORED_BEFORE_VIDEOS)
    }

    fn decode_marked(stored: &str, marked: &str) -> Option<Vec<Block>> {
        let mut lines = stored.lines();
        if lines.next() != Some(marked) {
            return None;
        }
        let blocks = lines
            .filter_map(|line| {
                let (mark, text) = line.split_once(' ')?;
                if mark == "i" {
                    let url = article::checked_link(text, None)?;
                    return url.starts_with("https://").then_some(Block::Picture(url));
                }
                if mark == "v" {
                    let (url, poster) = match text.split_once(' ') {
                        Some((url, poster)) => (url, Some(poster)),
                        None => (text, None),
                    };
                    return Some(Block::Video(Video {
                        url: video_url(url, None)?,
                        poster: poster
                            .and_then(|p| article::checked_link(p, None))
                            .filter(|p| p.starts_with("https://")),
                    }));
                }
                let text = article::one_line(text, MAX_BODY);
                Some(match mark {
                    "h" => Block::Heading(text),
                    "a" => Block::Aside(text),
                    _ => Block::Paragraph(text),
                })
            })
            .collect();
        Some(blocks)
    }
}

/// Pictures of an article kept, at most.
const MAX_PICTURES: usize = 40;
/// A picture narrower or shorter than this (by its `width`, `height`) is an
/// icon or a logo, not the article's.
const MIN_PICTURE: u32 = 120;
/// Words in a picture's address that mark it as no picture of the article.
const NOT_PICTURE: [&str; 6] = ["icon", "logo", "emoji", "blank.", "1x1", "spacer"];

/// Words in a class or id that mark what isn't the article: links to
/// others (in English and Vietnamese), sharing buttons, comments, ads.
const NOT_TEXT: [&str; 16] = [
    "related",
    "lien-quan",
    "lienquan",
    "share",
    "social",
    "comment",
    "banner",
    "advert",
    "ads-",
    "footer",
    "menu",
    "sidebar",
    "breadcrumb",
    "newsletter",
    "popup",
    "cookie",
];

/// Elements that are never an article's text.
const NOT_ARTICLE: [&str; 6] = ["nav", "aside", "footer", "header", "form", "menu"];

/// An article's own text, from its page at `base`: the element holding the
/// most paragraphs' text (as Firefox's reader view looks for it), then its
/// paragraphs, headings, pictures, videos and their captions in order.
/// Empty when there's none, a page that's all pictures; a page that's a
/// video alone, the video its data names ([`named_videos`]).
pub fn article_text(page: &str, base: &str) -> Vec<Block> {
    let document = Html::parse_document(page);
    let named = named_videos(&document, base);
    let paragraphs: Vec<ElementRef> = document
        .root_element()
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter(|e| e.value().name() == "p" && !outside_article(e))
        .collect();
    // Each paragraph's text counts for its parent, and half for the one
    // above: the text is where the most is (a tie, the one counted last).
    // Each's score, and when it was first counted.
    let mut scores: HashMap<ego_tree::NodeId, (usize, usize)> = HashMap::new();
    let mut add = |id: ego_tree::NodeId, n: usize| {
        let next = scores.len();
        scores.entry(id).or_insert((0, next)).0 += n;
    };
    for p in &paragraphs {
        let length = words_of(p).chars().count();
        if length < MIN_COUNTED {
            continue;
        }
        let mut above = p.ancestors();
        if let Some(parent) = above.next() {
            add(parent.id(), length);
        }
        if let Some(grandparent) = above.next() {
            add(grandparent.id(), length / 2);
        }
    }
    let Some(best) = scores
        .iter()
        .max_by_key(|(_, scored)| **scored)
        .and_then(|(id, _)| document.tree.get(*id))
        .and_then(ElementRef::wrap)
    else {
        return named.into_iter().take(1).map(Block::Video).collect();
    };
    // The text's videos, each with the box it's shown in on the page: the
    // pictures there are its poster, the paragraphs its caption.
    let page = reqwest::Url::parse(base).ok();
    let mut videos: HashMap<ego_tree::NodeId, Block> = HashMap::new();
    let mut boxes: HashSet<ego_tree::NodeId> = HashSet::new();
    for element in best.descendants().filter_map(ElementRef::wrap) {
        if videos.len() >= MAX_VIDEOS {
            break;
        }
        // Most elements are no video: the walks above them only for those
        // that are. A <video> in a player already found is that player's.
        if !plays_video(&element) {
            continue;
        }
        let parent = element.parent().and_then(ElementRef::wrap);
        let found_in = near(
            element
                .ancestors()
                .filter_map(ElementRef::wrap)
                .take_while(|a| a.id() != best.id()),
            |a| boxes.contains(&a.id()),
        );
        let outside =
            parent.is_none_or(|parent| outside_article(&parent) || marked_aside(&parent, &best));
        if found_in || outside {
            continue;
        }
        if let Some((block, boxed)) = video_of(&element, &best, page.as_ref(), base, &named) {
            boxes.insert(boxed);
            videos.insert(element.id(), block);
        }
    }
    // No walk up from each picture and paragraph on a page with no video.
    let in_box = |element: &ElementRef| {
        !boxes.is_empty()
            && near(
                std::iter::once(*element)
                    .chain(element.ancestors().filter_map(ElementRef::wrap))
                    .take_while(|a| a.id() != best.id()),
                |a| boxes.contains(&a.id()),
            )
    };
    let mut blocks: Vec<Block> = Vec::new();
    let mut length = 0;
    let mut pictures = 0;
    for element in best.descendants().filter_map(ElementRef::wrap) {
        if let Some(block) = videos.remove(&element.id()) {
            // A moving picture is a picture: kept within the same number.
            let picture = matches!(block, Block::Picture(_));
            if !blocks.contains(&block) && (!picture || pictures < MAX_PICTURES) {
                pictures += usize::from(picture);
                blocks.push(block);
            }
            continue;
        }
        if element.value().name() == "img" {
            if pictures < MAX_PICTURES
                && !in_box(&element)
                && !outside_article(&element)
                && !marked_aside(&element, &best)
                && let Some(url) = picture_of(&element, &best, base)
                && !blocks.iter().any(|b| *b == Block::Picture(url.clone()))
            {
                pictures += 1;
                blocks.push(Block::Picture(url));
            }
            continue;
        }
        let block = match element.value().name() {
            // A video's words are its caption.
            "p" | "li" if in_box(&element) => Block::Aside,
            "p" | "li" => Block::Paragraph,
            "h2" | "h3" | "h4" => Block::Heading,
            "figcaption" | "blockquote" => Block::Aside,
            _ => continue,
        };
        // Once only: a paragraph in a quote or a list item is the quote's.
        let nested = near(
            element
                .ancestors()
                .take_while(|a| a.id() != best.id())
                .filter_map(ElementRef::wrap),
            |a| matches!(a.value().name(), "p" | "li" | "blockquote" | "figcaption"),
        );
        if nested || outside_article(&element) || marked_aside(&element, &best) {
            continue;
        }
        let text = article::one_line(&words_of(&element), MAX_BODY);
        if text.is_empty() || blocks.last().is_some_and(|b| b.text() == text) {
            continue;
        }
        length += text.chars().count();
        blocks.push(block(text));
        if length > MAX_BODY || blocks.len() >= MAX_BLOCKS {
            break;
        }
    }
    // A page whose video plays above its text (VnExpress's, VTC News's
    // video pages) names it only in its data: at the top.
    if !blocks.iter().any(|b| matches!(b, Block::Video(_)))
        && let Some(video) = named.into_iter().next()
    {
        blocks.insert(0, Block::Video(video));
    }
    blocks
}

/// Videos of an article kept, at most.
const MAX_VIDEOS: usize = 10;
/// The biggest of a page's data (`application/ld+json`) read for its videos.
const MAX_DATA: usize = 256 * 1024;
/// What a video's address ends with: a file mpv plays as it is, or an HLS
/// playlist. Anything else (a page, an embedded player) isn't one.
const VIDEO_FILES: [&str; 5] = [".mp4", ".m4v", ".webm", ".mov", ".m3u8"];

/// A video's address as tuinews plays it: checked as a link is
/// (`article::checked_link`, relative to `base`), over https (one on http
/// is asked for on https, as pictures are), and a file or playlist by its
/// name ([`VIDEO_FILES`]).
pub fn video_url(url: &str, base: Option<&reqwest::Url>) -> Option<String> {
    let mut url = reqwest::Url::parse(&article::checked_link(url, base)?).ok()?;
    if url.scheme() == "http" {
        url.set_scheme("https").ok()?;
    }
    let path = url.path().to_ascii_lowercase();
    VIDEO_FILES
        .iter()
        .any(|ext| path.ends_with(ext))
        .then(|| url.to_string())
}

/// The videos a page's data says are its own (`<script
/// type="application/ld+json">`, schema.org's `VideoObject`): its address
/// (`contentUrl`) and picture (`thumbnailUrl`). Dân Trí's `<video>` names
/// its file by a path that isn't where it streams from; VTC News's player
/// is made by a script. Its own: one at the data's top (or in a list
/// there, or its `@graph`), or the `video` of an entity there (Dân Trí's
/// `NewsArticle`); not one deeper, in a list of others (an `ItemList`).
fn named_videos(document: &Html, base: &str) -> Vec<Video> {
    use serde_json::Value;
    let page = reqwest::Url::parse(base).ok();
    let mut found: Vec<Video> = Vec::new();
    let scripts = document
        .root_element()
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter(|e| {
            e.value().name() == "script"
                && e.attr("type")
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/ld+json"))
        });
    for script in scripts {
        // Measured as it's read: a page's megabytes aren't copied to be
        // thrown away.
        let mut data = String::new();
        for piece in script.text() {
            if data.len() + piece.len() > MAX_DATA {
                data.clear();
                break;
            }
            data.push_str(piece);
        }
        if data.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let entities: Vec<&Value> = listed(Some(&value))
            .into_iter()
            .flat_map(|top| std::iter::once(top).chain(listed(top.get("@graph"))))
            .collect();
        let own = entities
            .iter()
            .flat_map(|&entity| std::iter::once(entity).chain(listed(entity.get("video"))));
        for video in own {
            if found.len() >= MAX_VIDEOS {
                break;
            }
            if is_video_object(video)
                && let Some(url) = first_text(video.get("contentUrl"))
                    .and_then(|url| video_url(url, page.as_ref()))
                && !found.iter().any(|v| v.url == url)
            {
                let poster = first_text(video.get("thumbnailUrl"))
                    .and_then(|p| crate::feed::picture_url(p, base));
                found.push(Video { url, poster });
            }
        }
    }
    found
}

/// A value of a page's data as a list: the items of a list, or the value
/// alone; none for none.
fn listed(value: Option<&serde_json::Value>) -> Vec<&serde_json::Value> {
    match value {
        Some(serde_json::Value::Array(items)) => items.iter().collect(),
        Some(serde_json::Value::Null) | None => Vec::new(),
        Some(one) => vec![one],
    }
}

/// Whether `element` plays a video on the page: a `<video>`, or VCCorp's
/// player.
fn plays_video(element: &ElementRef) -> bool {
    let value = element.value();
    value.name() == "video"
        || value
            .attr("type")
            .is_some_and(|t| t.eq_ignore_ascii_case("VideoStream"))
}

/// Whether a value of a page's data is a video: its `@type` is, or lists,
/// `VideoObject` (or schema.org's whole name for it, `…/VideoObject`), not
/// any name that ends so.
fn is_video_object(value: &serde_json::Value) -> bool {
    let is = |t: &serde_json::Value| {
        t.as_str().is_some_and(|t| {
            t == "VideoObject" || t.ends_with("/VideoObject") || t.ends_with(":VideoObject")
        })
    };
    match value.get("@type") {
        Some(serde_json::Value::Array(types)) => types.iter().any(is),
        Some(t) => is(t),
        None => false,
    }
}

/// A text, or the first of a list of them.
fn first_text(value: Option<&serde_json::Value>) -> Option<&str> {
    match value? {
        serde_json::Value::String(text) => Some(text),
        serde_json::Value::Array(items) => items.iter().find_map(serde_json::Value::as_str),
        _ => None,
    }
}

/// The video `element` of the article's `container` is, if it's one: a
/// `<video>` (its `src`, `data-src` or a `<source>`'s) or VCCorp's player
/// (Thanh Niên, Kenh14, GenK, Tuổi Trẻ: `type="VideoStream"`, its
/// `data-vid` a host and a path), with its poster; and the box it's shown
/// in. Where the page's data names a video of the same file, its address is
/// taken: the element's may be a path the page's script turns into another.
/// A `<video>` that plays on its own, muted, over and over is a moving
/// picture (Tuổi Trẻ's GIFs): its poster, as a picture.
fn video_of(
    element: &ElementRef,
    container: &ElementRef,
    page: Option<&reqwest::Url>,
    base: &str,
    named: &[Video],
) -> Option<(Block, ego_tree::NodeId)> {
    let value = element.value();
    let picture = |attr: &str| {
        value
            .attr(attr)
            .and_then(|src| crate::feed::picture_url(src, base))
    };
    let (raw, poster) = if value.name() == "video" {
        let poster = picture("poster").or_else(|| picture("data-poster"));
        if value.attr("loop").is_some() && value.attr("muted").is_some() {
            return Some((Block::Picture(poster?), element.id()));
        }
        let sources = element
            .children()
            .filter_map(ElementRef::wrap)
            .filter(|e| e.value().name() == "source")
            .flat_map(|e| [e.attr("src"), e.attr("data-src")]);
        let raw = [value.attr("src"), value.attr("data-src")]
            .into_iter()
            .chain(sources)
            .flatten()
            .map(str::trim)
            .find(|src| !src.is_empty() && !src.starts_with("blob:"))?;
        (raw.to_string(), poster)
    } else if plays_video(element) {
        let vid = value.attr("data-vid")?.trim();
        // A host and a path, as VCCorp writes it; a whole address, or a
        // path on the site, as it is.
        let raw = if vid.contains("://") || vid.starts_with('/') {
            vid.to_string()
        } else {
            format!("https://{vid}")
        };
        (raw, picture("data-thumb"))
    } else {
        return None;
    };
    let file = file_stem(&raw);
    let same = named.iter().find(|v| {
        v.url == raw
            || file.is_some_and(|file| {
                reqwest::Url::parse(&v.url).is_ok_and(|u| u.path().contains(file))
            })
    });
    let url = match same {
        Some(video) => video.url.clone(),
        None => video_url(&raw, page)?,
    };
    // A <video>'s box, once it's known to be one: VCCorp's player is its own.
    let boxed = if value.name() == "video" {
        video_box(element, container)
    } else {
        element.id()
    };
    let poster = poster
        .or_else(|| same.and_then(|v| v.poster.clone()))
        .or_else(|| poster_in(boxed, container, base));
    Some((Block::Video(Video { url, poster }), boxed))
}

/// The name of the file a video's address names, without its extension,
/// when it tells one file from another: long, with digits in it, as news
/// sites name theirs (`fsavercom-1447956340609174-960p`), not a name every
/// stream has (`playlist`, `master`, `manifest`).
fn file_stem(url: &str) -> Option<&str> {
    let path = url.split(['?', '#']).next()?;
    let name = path.rsplit('/').next()?;
    let stem = name.split('.').next()?;
    (stem.len() >= 12 && stem.bytes().any(|b| b.is_ascii_digit())).then_some(stem)
}

/// The box a `<video>` is shown in, inside the article's `container`: what
/// holds it and nothing else of the text, with at most one picture (the
/// poster VnExpress shows until it plays).
fn video_box(video: &ElementRef, container: &ElementRef) -> ego_tree::NodeId {
    let text = ["p", "li", "h2", "h3", "h4", "blockquote", "figcaption"];
    let mut boxed = video.id();
    for above in video
        .ancestors()
        .filter_map(ElementRef::wrap)
        .take(MAX_ANCESTORS)
    {
        if above.id() == container.id() {
            break;
        }
        let (mut pictures, mut videos) = (0, 0);
        let more = above.descendants().filter_map(ElementRef::wrap).any(|e| {
            let name = e.value().name();
            pictures += usize::from(name == "img");
            videos += usize::from(name == "video");
            text.contains(&name) || pictures > 1 || videos > 1
        });
        if more {
            break;
        }
        boxed = above.id();
    }
    boxed
}

/// The picture in a video's box, its poster.
fn poster_in(boxed: ego_tree::NodeId, container: &ElementRef, base: &str) -> Option<String> {
    let boxed = ElementRef::wrap(container.tree().get(boxed)?)?;
    boxed
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter(|e| e.value().name() == "img")
        .find_map(|img| picture_of(&img, container, base))
}

/// The picture an `img` in the article's `container` shows, if it's one of
/// the article's: where it is (a picture that loads late keeps it in
/// `data-src` or `data-original`, its `src` a blank one), not an icon or a
/// logo by its size or address, nor a link's to another page (a related
/// article's thumbnail), checked as `feed::picture_url` checks it.
fn picture_of(img: &ElementRef, container: &ElementRef, base: &str) -> Option<String> {
    let value = img.value();
    let small = |attr: &str| {
        value
            .attr(attr)
            .and_then(|v| v.trim().trim_end_matches("px").parse::<u32>().ok())
            .is_some_and(|n| n < MIN_PICTURE)
    };
    if small("width") || small("height") {
        return None;
    }
    let srcset = |attr: &str| {
        value
            .attr(attr)
            .and_then(|set| set.split(',').next_back())
            .and_then(|candidate| candidate.split_whitespace().next())
    };
    let src = ["data-src", "data-original", "data-lazy-src", "src"]
        .iter()
        .filter_map(|a| value.attr(a))
        .chain(srcset("data-srcset"))
        .chain(srcset("srcset"))
        .map(str::trim)
        .find(|src| !src.is_empty() && !src.starts_with("data:"))?;
    let lower = src.to_ascii_lowercase();
    let path = lower.split('?').next().unwrap_or_default();
    if path.ends_with(".svg") || NOT_PICTURE.iter().any(|word| path.contains(word)) {
        return None;
    }
    let to_elsewhere = near(
        img.ancestors()
            .take_while(|a| a.id() != container.id())
            .filter_map(ElementRef::wrap),
        |a| {
            let Some(href) = (a.value().name() == "a").then(|| a.attr("href")).flatten() else {
                return false;
            };
            let href = href.trim().to_ascii_lowercase();
            let page = href.split('?').next().unwrap_or_default();
            !(href.is_empty()
                || href.starts_with('#')
                || href.starts_with("javascript:")
                || [".jpg", ".jpeg", ".png", ".webp", ".gif", ".avif"]
                    .iter()
                    .any(|ext| page.ends_with(ext)))
        },
    );
    if to_elsewhere {
        return None;
    }
    crate::feed::picture_url(src, base)
}

/// The words under `element`, without its [`SKIPPED`] parts.
fn words_of(element: &ElementRef) -> String {
    let mut out = String::new();
    text_into(**element, &mut out, &[]);
    out
}

/// Whether `element` is in a part of the page that's never the article: a
/// menu, a header or footer, a form, or something [`SKIPPED`]. The page's
/// `body` and what's above it don't count.
fn outside_article(element: &ElementRef) -> bool {
    near(
        std::iter::once(*element)
            .chain(element.ancestors().filter_map(ElementRef::wrap))
            .take_while(|e| !matches!(e.value().name(), "body" | "html")),
        |e| {
            let name = e.value().name();
            NOT_ARTICLE.contains(&name) || SKIPPED.contains(&name)
        },
    )
}

/// Whether `element`, inside the article's `container`, is marked as what
/// isn't the article's text by its class or id ([`NOT_TEXT`]): a list of
/// related links, a box of sharing buttons. Only what's inside counts: a
/// site's whole column can be called `sidebar-1` (VnExpress's is).
fn marked_aside(element: &ElementRef, container: &ElementRef) -> bool {
    near(
        std::iter::once(*element)
            .chain(element.ancestors().filter_map(ElementRef::wrap))
            .take_while(|e| e.id() != container.id()),
        |e| {
            let value = e.value();
            let marks = [value.attr("class"), value.attr("id")]
                .into_iter()
                .flatten()
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
                .join(" ");
            NOT_TEXT.iter().any(|word| marks.contains(word))
        },
    )
}

/// Whether `found` holds for one of the first [`MAX_ANCESTORS`] elements of
/// `walk` (an element and those above it), or the walk goes further: what's
/// nested that deep is taken to be in what's looked for, so it's left out.
fn near<'a>(
    walk: impl Iterator<Item = ElementRef<'a>>,
    mut found: impl FnMut(&ElementRef<'a>) -> bool,
) -> bool {
    walk.take(MAX_ANCESTORS + 1)
        .enumerate()
        .any(|(i, e)| i == MAX_ANCESTORS || found(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_nested_deeper_than_a_stack_goes_are_read_or_left_out() {
        let deep = format!("{}x{}", "<span>".repeat(200_000), "</span>".repeat(200_000));
        assert_eq!(summary(&deep), "x");
        let page = |depth: usize| {
            format!(
                "<html><body><article>{}<p>{}</p></article></body></html>",
                "<span>".repeat(depth),
                "Đoạn văn đủ dài để được tính là văn bản của bài viết này. ".repeat(3)
            )
        };
        assert!(!article_text(&page(200), "https://e.vn/a").is_empty());
        // Deeper than pages go, it's taken to be no article's text.
        assert!(article_text(&page(100_000), "https://e.vn/a").is_empty());
    }

    #[test]
    fn a_summary_is_its_text_without_pictures_or_related_links() {
        let vnexpress = r#"<a href="https://vnexpress.net/a.html"><img src="https://i1-vnexpress.vnecdn.net/x.jpg"></a></br>SpaceX đang đàm phán với các công ty.</br>"#;
        assert_eq!(summary(vnexpress), "SpaceX đang đàm phán với các công ty.");
        let tienphong = r#"<a href="x"><img src="y.jpg.avif"/></a><p>Một tòa nhà đổ sập.</p><div style="clear:both;"><ul><li><a href='z'>Bài khác</a></li></ul></div>"#;
        assert_eq!(summary(tienphong), "Một tòa nhà đổ sập.");
        assert_eq!(
            summary("Võ sĩ &quot;tai tiếng&quot; bị phạt"),
            "Võ sĩ \"tai tiếng\" bị phạt"
        );
        assert_eq!(summary("plain text, no tags"), "plain text, no tags");
        assert_eq!(
            summary("và &amp;apos;mệnh lệnh từ trái tim&amp;apos;"),
            "và 'mệnh lệnh từ trái tim'",
            "VietnamNet's, escaped twice"
        );
        assert_eq!(summary("AT&amp;T &amp; co"), "AT&T & co");
        assert_eq!(summary("&lt;b&gt; stays text"), "<b> stays text");
        assert_eq!(
            summary("<script>alert(1)</script><p>one</p><p>two</p>"),
            "one\ntwo"
        );
    }

    #[test]
    fn an_articles_text_is_kept_as_its_blocks() {
        let blocks = vec![
            Block::Paragraph("Một".into()),
            Block::Heading("Hai".into()),
            Block::Aside("Ba".into()),
        ];
        assert_eq!(Block::decode(&Block::encode(&blocks)), Some(blocks));
    }

    #[test]
    fn a_sites_page_names_its_feeds() {
        let page = r#"<html><head>
            <link rel="stylesheet" href="/a.css">
            <link rel="alternate" type="application/rss+xml" title="RSS" href="/rss/home.rss">
            <link rel="alternate" type="application/atom+xml" href="http://127.0.0.1/feed">
        </head></html>"#;
        let base = reqwest::Url::parse("https://genk.vn/").unwrap();
        assert_eq!(feeds_named(page, &base), ["https://genk.vn/rss/home.rss"]);
    }

    #[test]
    fn the_first_picture_is_found_where_it_is() {
        assert_eq!(
            first_image(r#"<a><img src='https://cdn.example/a.jpg'/></a>"#).as_deref(),
            Some("https://cdn.example/a.jpg")
        );
        assert_eq!(
            first_image(r#"<img src="data:image/gif;base64,R0" data-src="/b.png">"#).as_deref(),
            Some("/b.png")
        );
        assert_eq!(first_image("no picture"), None);
    }

    #[test]
    fn an_articles_text_is_where_most_of_its_paragraphs_are() {
        let page = r#"<html><body>
            <nav><p>Thời sự · Thế giới · Kinh doanh · Giải trí · Thể thao · Pháp luật</p></nav>
            <div class="sidebar"><p>Một bài khác rất dài, mà không phải là bài đang đọc, nằm ở cột bên.</p></div>
            <div class="sidebar-1"><main>
            <article class="fck_detail">
              <p class="Normal">Đoạn đầu tiên của bài báo, đủ dài để được tính là văn bản thật sự.</p>
              <figure><img src="a.jpg"><figcaption>Ảnh minh họa</figcaption></figure>
              <h2>Một tiêu đề phụ</h2>
              <p class="Normal">Đoạn thứ hai của bài báo, cũng đủ dài để được tính vào điểm số.</p>
              <p class="Normal">Đoạn thứ hai của bài báo, cũng đủ dài để được tính vào điểm số.</p>
              <div class="box-related"><p>Tin liên quan: một bài khác, đủ dài để bị bỏ qua thôi.</p></div>
              <script>var x = "<p>not text</p>";</script>
              <p class="author"><strong>Hoàng Hướng</strong></p>
            </article>
            </main></div>
            <footer><p>© Báo điện tử, giấy phép số 548/GP-BTTTT, tòa soạn ở Hà Nội.</p></footer>
        </body></html>"#;
        let blocks = article_text(page, "https://vnexpress.net/a.html");
        assert_eq!(
            blocks,
            [
                Block::Paragraph(
                    "Đoạn đầu tiên của bài báo, đủ dài để được tính là văn bản thật sự.".into()
                ),
                Block::Picture("https://vnexpress.net/a.jpg".into()),
                Block::Aside("Ảnh minh họa".into()),
                Block::Heading("Một tiêu đề phụ".into()),
                Block::Paragraph(
                    "Đoạn thứ hai của bài báo, cũng đủ dài để được tính vào điểm số.".into()
                ),
                Block::Paragraph("Hoàng Hướng".into()),
            ],
            "the repeat dropped, the related box, script, menu and footer left out, \
             in a column called a sidebar as VnExpress's is"
        );
        assert!(article_text("<html><body><img src=x></body></html>", "https://e.vn/").is_empty());
    }

    #[test]
    fn an_articles_pictures_come_in_place_but_not_icons_or_other_articles() {
        let page = r#"<html><body><article class="fck_detail">
            <p>Đoạn đầu tiên của bài báo, đủ dài để được tính là văn bản thật sự.</p>
            <figure><picture><img class="lazy" src="data:image/gif;base64,R0lGOD"
                data-src="https://i1-vnexpress.vnecdn.net/2026/10/23/bts.jpg?w=680"></picture>
                <figcaption>Trạm thu phát sóng. Ảnh: Reuters</figcaption></figure>
            <p>Đoạn thứ hai của bài báo, cũng đủ dài để được tính vào điểm số.</p>
            <p><a href="/bai-khac.html"><img src="/khac.jpg" width="465" height="290"></a>
               <img src="https://s1.vnecdn.net/i/icons/icon-google.svg">
               <img src="/tiny.png" width="16" height="16">
               <a href="/to.jpg"><img src="/lon.jpg.avif"></a>
               <img srcset="/a-320.jpg 320w, /a-1200.jpg 1200w"></p>
            <img src="http://127.0.0.1/x.jpg">
        </article></body></html>"#;
        let blocks = article_text(page, "https://vnexpress.net/bai.html");
        let pictures: Vec<&Block> = blocks
            .iter()
            .filter(|b| matches!(b, Block::Picture(_)))
            .collect();
        assert_eq!(
            pictures,
            [
                &Block::Picture("https://i1-vnexpress.vnecdn.net/2026/10/23/bts.jpg?w=680".into()),
                &Block::Picture("https://vnexpress.net/lon.jpg".into()),
                &Block::Picture("https://vnexpress.net/a-1200.jpg".into()),
            ],
            "the late one's data-src, not the icon, the tiny one, the related \
             article's, the one on this computer; the AVIF as its JPEG"
        );
        assert_eq!(
            blocks[1], *pictures[0],
            "in its place, after the first paragraph"
        );
        assert_eq!(
            blocks[2],
            Block::Aside("Trạm thu phát sóng. Ảnh: Reuters".into())
        );
        // Kept and read back; a copy from before pictures is read again.
        assert_eq!(Block::decode(&Block::encode(&blocks)), Some(blocks));
        assert_eq!(Block::decode("p Một\nh Hai"), None);
    }

    /// A paragraph long enough to count, for pages in tests.
    const LONG: &str = "Đoạn văn của bài báo, đủ dài để được tính là văn bản thật sự của nó.";

    fn videos(blocks: &[Block]) -> Vec<&Video> {
        blocks
            .iter()
            .filter_map(|b| match b {
                Block::Video(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_video_in_the_text_comes_in_place_with_its_poster_and_caption() {
        // VnExpress's: the poster an img beside the player, the title a div.
        let page = format!(
            r#"<html><body><article class="fck_detail"><p>{LONG}</p>
            <figure><div class="box_embed_video_parent" data-vid="457106">
              <div class="box_img_video"><img class="thumb-above-video"
                src="https://iv1.vnecdn.net/vnexpress/images/web/2026/10/22/patriot.jpg?w=0"></div>
              <div class="box_embed_video" style="display:none"><div id="video-457106">
                <video id="media-video-457106" preload="auto" type="application/x-mpegURL"
                  src="https://d1.vnecdn.net/vnexpress/video/web/mp4/,240p,360p,,/2026/10/22/patriot/vne/master.m3u8"></video>
              </div><div class="parser_title">Patriot</div></div>
            </div><figcaption><p class="Image">Kiev ngày 22/10. Video: Reuters</p></figcaption></figure>
            <p>{LONG}</p></article></body></html>"#
        );
        let blocks = article_text(&page, "https://vnexpress.net/a-5136691.html");
        assert_eq!(
            blocks[1],
            Block::Video(Video {
                url: "https://d1.vnecdn.net/vnexpress/video/web/mp4/,240p,360p,,/2026/10/22/patriot/vne/master.m3u8".into(),
                poster: Some("https://iv1.vnecdn.net/vnexpress/images/web/2026/10/22/patriot.jpg?w=0".into()),
            }),
            "{blocks:#?}"
        );
        assert_eq!(
            blocks[2],
            Block::Aside("Kiev ngày 22/10. Video: Reuters".into())
        );
        assert!(
            !blocks.iter().any(|b| matches!(b, Block::Picture(_))),
            "the poster isn't a picture of its own: {blocks:#?}"
        );
        // Kept and read back as it was.
        assert_eq!(Block::decode(&Block::encode(&blocks)), Some(blocks));
    }

    #[test]
    fn vccorps_player_names_its_file_by_host_and_path() {
        // Thanh Niên's, Kenh14's, GenK's: the caption inside the player.
        let page = format!(
            r#"<html><body><div class="detail-content"><p>{LONG}</p>
            <div class="VCSortableInPreviewMode" type="VideoStream" embed-type="4"
              data-vid="kenh14cdn.com/2033/2026/10/26/flash-flood-1793000428.mp4"
              data-thumb="https://kenh14cdn.com/.v-thumb/2033/2026/10/26/flash-flood-1793000428.mp4.jpg">
              <div class="VideoCMS_Caption"><p>Clip: The Phuket Express</p></div></div>
            <p>{LONG}</p></div></body></html>"#
        );
        let blocks = article_text(&page, "https://kenh14.vn/canh-tuong.chn");
        assert_eq!(
            videos(&blocks),
            [&Video {
                url: "https://kenh14cdn.com/2033/2026/10/26/flash-flood-1793000428.mp4".into(),
                poster: Some(
                    "https://kenh14cdn.com/.v-thumb/2033/2026/10/26/flash-flood-1793000428.mp4.jpg"
                        .into()
                ),
            }]
        );
        assert_eq!(blocks[2], Block::Aside("Clip: The Phuket Express".into()));
    }

    #[test]
    fn a_pages_data_names_where_its_video_streams_from() {
        // Dân Trí's <video> names a path its script turns into another: the
        // page's data has the address.
        let data = r#"{"@context":"https://schema.org","@graph":[{"@type":"NewsArticle"},
            {"@type":"VideoObject","thumbnailUrl":["https://cdnphoto.dantri.com.vn/x/thumb-video/fsavercom-1447956340609174/0.jpg"],
             "contentUrl":"https://vcdn.dantri.com.vn/vod/2026/10/26/fsavercom-1447956340609174/playlist.m3u8"}]}"#;
        let page = format!(
            r#"<html><head><script type="application/ld+json">{data}</script></head><body>
            <div data-slot="content"><figure class="video"><video
              data-src="/2026/10/26/fsavercom-1447956340609174.mp4"></video>
              <figcaption><p>Bán ô tô cũ (Video: Cục CSGT).</p></figcaption></figure>
            <p>{LONG}</p><p>{LONG}</p></div></body></html>"#
        );
        let blocks = article_text(&page, "https://dantri.com.vn/o-to-xe-may/a.htm");
        assert_eq!(
            blocks[0],
            Block::Video(Video {
                url: "https://vcdn.dantri.com.vn/vod/2026/10/26/fsavercom-1447956340609174/playlist.m3u8".into(),
                poster: Some("https://cdnphoto.dantri.com.vn/x/thumb-video/fsavercom-1447956340609174/0.jpg".into()),
            })
        );
        assert_eq!(
            blocks[1],
            Block::Aside("Bán ô tô cũ (Video: Cục CSGT).".into())
        );
        // VTC News's player is made by a script: the video its data names
        // goes first. A page with a video alone is that video.
        let vtc = format!(
            r#"<html><head><script type="application/ld+json">
            {{"@type":"VideoObject","contentUrl":"https://cdn-v.vtcnews.vn/video/2026/10/26/5D90/5D90.mp4"}}
            </script></head><body><article><p>{LONG}</p></article></body></html>"#
        );
        let blocks = article_text(&vtc, "https://vtcnews.vn/a-ar1047426.html");
        assert_eq!(
            blocks[0],
            Block::Video(Video {
                url: "https://cdn-v.vtcnews.vn/video/2026/10/26/5D90/5D90.mp4".into(),
                poster: None,
            })
        );
        let alone = vtc.replace(&format!("<p>{LONG}</p>"), "");
        assert_eq!(article_text(&alone, "https://vtcnews.vn/a.html").len(), 1);
    }

    #[test]
    fn only_videos_over_https_from_the_internet_play() {
        let base = reqwest::Url::parse("https://vnexpress.net/a.html").unwrap();
        let url = |u: &str| video_url(u, Some(&base));
        assert_eq!(
            url("http://v.vnecdn.net/a.mp4").as_deref(),
            Some("https://v.vnecdn.net/a.mp4"),
            "asked for on https"
        );
        assert_eq!(
            url("/v/a.M3U8").as_deref(),
            Some("https://vnexpress.net/v/a.M3U8")
        );
        for refused in [
            "https://www.youtube.com/watch?v=abc",
            "https://embed.vietnamnet.vn/v/124244.html",
            "https://127.0.0.1/a.mp4",
            "https://localhost/a.mp4",
            "https://router.lan/a.mp4",
            "file:///etc/a.mp4",
            "https://user:pw@v.vnecdn.net/a.mp4",
        ] {
            assert_eq!(url(refused), None, "{refused}");
        }
        // Read back from the store, checked again.
        assert_eq!(
            Block::decode(
                "v 3\nv https://127.0.0.1/a.mp4\nv https://v.vnecdn.net/a.mp4 http://x.vn/p.jpg"
            ),
            Some(vec![Block::Video(Video {
                url: "https://v.vnecdn.net/a.mp4".into(),
                poster: None,
            })])
        );
        // A copy kept before videos were is read from its page again, and
        // shown as it was while it can't be.
        assert_eq!(Block::decode("v 2\np Một"), None);
        assert_eq!(
            Block::decode_older("v 2\np Một"),
            Some(vec![Block::Paragraph("Một".into())])
        );
        assert_eq!(Block::decode_older("v 3\np Một"), None);
    }

    #[test]
    fn only_a_video_object_is_a_video() {
        let typed = |t: &str| is_video_object(&serde_json::json!({ "@type": t }));
        assert!(typed("VideoObject"));
        assert!(typed("http://schema.org/VideoObject"));
        assert!(typed("schema:VideoObject"));
        assert!(!typed("NotAVideoObject"));
        assert!(is_video_object(
            &serde_json::json!({ "@type": ["Thing", "VideoObject"] })
        ));
    }

    #[test]
    fn only_the_videos_a_pages_data_says_are_its_own_go_first() {
        let page = |data: &str| {
            format!(
                r#"<html><head><script type="application/ld+json">{data}</script></head>
                <body><article><p>{LONG}</p><p>{LONG}</p></article></body></html>"#
            )
        };
        let video = r#"{"@type":"VideoObject","contentUrl":"https://v.vn/a.mp4"}"#;
        let first = |data: &str| article_text(&page(data), "https://e.vn/a.html").remove(0);
        let ours = Block::Video(Video {
            url: "https://v.vn/a.mp4".into(),
            poster: None,
        });
        // At the top, in its @graph, or an article's video.
        assert_eq!(first(video), ours);
        assert_eq!(
            first(&format!(r#"{{"@graph":[{{"@type":"WebPage"}},{video}]}}"#)),
            ours
        );
        assert_eq!(
            first(&format!(r#"{{"@type":"NewsArticle","video":{video}}}"#)),
            ours
        );
        // A list of other videos, the site's, isn't the article's.
        let others = format!(
            r#"{{"@type":"ItemList","itemListElement":[{{"@type":"ListItem","item":{video}}}]}}"#
        );
        assert!(matches!(first(&others), Block::Paragraph(_)));
    }

    #[test]
    fn a_name_every_stream_has_doesnt_make_two_videos_one() {
        // `playlist` is every HLS stream's name: the page's data naming
        // clip B says nothing of clip A's address.
        let data = r#"{"@type":"VideoObject","contentUrl":"https://v.vn/clip-b/playlist.m3u8"}"#;
        let page = format!(
            r#"<html><head><script type="application/ld+json">{data}</script></head><body>
            <article><p>{LONG}</p><video src="https://v.vn/clip-a/playlist.m3u8"></video>
            <p>{LONG}</p></article></body></html>"#
        );
        let blocks = article_text(&page, "https://e.vn/a.html");
        assert_eq!(
            videos(&blocks)
                .iter()
                .map(|v| v.url.as_str())
                .collect::<Vec<_>>(),
            ["https://v.vn/clip-a/playlist.m3u8"]
        );
    }

    #[test]
    fn a_video_inside_a_player_is_that_players() {
        let page = format!(
            r#"<html><body><div class="detail-content"><p>{LONG}</p>
            <div type="VideoStream" data-vid="thanhnien.mediacdn.vn/a/clip-17930073.mp4"
              data-thumb="https://thanhnien.mediacdn.vn/a/t.jpg">
              <video src="https://thanhnien.mediacdn.vn/a/clip-17930073.mp4"></video></div>
            <p>{LONG}</p></div></body></html>"#
        );
        let blocks = article_text(&page, "https://thanhnien.vn/a.htm");
        assert_eq!(videos(&blocks).len(), 1, "{blocks:#?}");
    }

    #[test]
    fn a_video_that_loops_muted_is_a_moving_picture() {
        // Tuổi Trẻ's GIFs come as videos that play on their own.
        let page = format!(
            r#"<html><body><div class="detail-content"><p>{LONG}</p>
            <video autoplay muted loop playsinline poster="https://cdn2.tuoitre.vn/a.gif.png"
              src="https://cdn2.tuoitre.vn/a.gif.mp4"></video><p>{LONG}</p></div></body></html>"#
        );
        let blocks = article_text(&page, "https://tuoitre.vn/a.htm");
        assert_eq!(
            blocks[1],
            Block::Picture("https://cdn2.tuoitre.vn/a.gif.png".into())
        );
        assert!(videos(&blocks).is_empty());
    }
}
