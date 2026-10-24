//! HTML as text: what a feed says of an article (`summary`), the picture
//! in it (`first_image`), and an article's own text, read from its page
//! (`article_text`).
//!
//! Only text comes out: tags, scripts, styles, forms and links are gone,
//! and what's left goes through `article::lines`, so nothing in it can
//! reach the terminal as anything but text.

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
/// leaving out [`SKIPPED`] elements and those in `also`.
fn text_into(node: NodeRef<Node>, out: &mut String, also: &[&str]) {
    for child in node.children() {
        match child.value() {
            Node::Text(text) => out.push_str(text),
            Node::Element(element) => {
                let name = element.name();
                if SKIPPED.contains(&name) || also.contains(&name) {
                    continue;
                }
                let block = BLOCKS.contains(&name);
                if block && !out.ends_with('\n') && !out.is_empty() {
                    out.push('\n');
                }
                text_into(child, out, also);
                if block {
                    out.push('\n');
                }
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
}

/// What the store's copy of an article's text starts with. Copies without
/// it were made before pictures were kept, and are read from the page again.
const STORED: &str = "v 2";

impl Block {
    /// Its words; a picture has none.
    pub fn text(&self) -> &str {
        match self {
            Block::Paragraph(t) | Block::Heading(t) | Block::Aside(t) => t,
            Block::Picture(_) => "",
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
            }))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Blocks as [`Block::encode`] wrote them, cleaned and checked again;
    /// `None` for a copy made before pictures were kept.
    pub fn decode(stored: &str) -> Option<Vec<Block>> {
        let mut lines = stored.lines();
        if lines.next() != Some(STORED) {
            return None;
        }
        let blocks = lines
            .filter_map(|line| {
                let (mark, text) = line.split_once(' ')?;
                if mark == "i" {
                    let url = article::checked_link(text, None)?;
                    return url.starts_with("https://").then_some(Block::Picture(url));
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
/// paragraphs, headings, pictures and their captions in order. Empty when
/// there's none, a page that's all pictures or video.
pub fn article_text(page: &str, base: &str) -> Vec<Block> {
    let document = Html::parse_document(page);
    let paragraphs: Vec<ElementRef> = document
        .root_element()
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter(|e| e.value().name() == "p" && !outside_article(e))
        .collect();
    // Each paragraph's text counts for its parent, and half for the one
    // above: the text is where the most is.
    let mut scores: Vec<(ego_tree::NodeId, usize)> = Vec::new();
    let mut add = |id: ego_tree::NodeId, n: usize| match scores.iter_mut().find(|(i, _)| *i == id) {
        Some((_, score)) => *score += n,
        None => scores.push((id, n)),
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
        .max_by_key(|(_, score)| *score)
        .and_then(|(id, _)| document.tree.get(*id))
        .and_then(ElementRef::wrap)
    else {
        return Vec::new();
    };
    let mut blocks: Vec<Block> = Vec::new();
    let mut length = 0;
    let mut pictures = 0;
    for element in best.descendants().filter_map(ElementRef::wrap) {
        if element.value().name() == "img" {
            if pictures < MAX_PICTURES
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
            "p" | "li" => Block::Paragraph,
            "h2" | "h3" | "h4" => Block::Heading,
            "figcaption" | "blockquote" => Block::Aside,
            _ => continue,
        };
        // Once only: a paragraph in a quote or a list item is the quote's.
        let nested = element
            .ancestors()
            .take_while(|a| a.id() != best.id())
            .filter_map(ElementRef::wrap)
            .any(|a| matches!(a.value().name(), "p" | "li" | "blockquote" | "figcaption"));
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
    blocks
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
    let to_elsewhere = img
        .ancestors()
        .take_while(|a| a.id() != container.id())
        .filter_map(ElementRef::wrap)
        .filter(|a| a.value().name() == "a")
        .filter_map(|a| a.attr("href"))
        .any(|href| {
            let href = href.trim().to_ascii_lowercase();
            let page = href.split('?').next().unwrap_or_default();
            !(href.is_empty()
                || href.starts_with('#')
                || href.starts_with("javascript:")
                || [".jpg", ".jpeg", ".png", ".webp", ".gif", ".avif"]
                    .iter()
                    .any(|ext| page.ends_with(ext)))
        });
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
    std::iter::once(*element)
        .chain(element.ancestors().filter_map(ElementRef::wrap))
        .take_while(|e| !matches!(e.value().name(), "body" | "html"))
        .any(|e| {
            let name = e.value().name();
            NOT_ARTICLE.contains(&name) || SKIPPED.contains(&name)
        })
}

/// Whether `element`, inside the article's `container`, is marked as what
/// isn't the article's text by its class or id ([`NOT_TEXT`]): a list of
/// related links, a box of sharing buttons. Only what's inside counts: a
/// site's whole column can be called `sidebar-1` (VnExpress's is).
fn marked_aside(element: &ElementRef, container: &ElementRef) -> bool {
    std::iter::once(*element)
        .chain(element.ancestors().filter_map(ElementRef::wrap))
        .take_while(|e| e.id() != container.id())
        .any(|e| {
            let value = e.value();
            let marks = [value.attr("class"), value.attr("id")]
                .into_iter()
                .flatten()
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
                .join(" ");
            NOT_TEXT.iter().any(|word| marks.contains(word))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
