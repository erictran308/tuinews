//! The README's picture (`docs/hero.png`): one frame of tuinews over
//! made-up news sites and articles, none of them real.
//! `cargo test -- --ignored export_hero_screen` writes it to
//! `target/hero/screen.json` (every cell, and where each picture went, with
//! the kind of picture its article has), which `tools/hero.py` draws as
//! Ghostty would, painting the pictures itself.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui_image::FontSize;
use ratatui_image::picker::{Picker, ProtocolType};

use crate::app::{App, Focus, now};
use crate::article::ArticleId;
use crate::feed::{self, Item};
use crate::images::{Images, Subject};
use crate::settings::Settings;
use crate::store::Store;

/// A made-up news site: its name, its address, and its category.
struct Site {
    name: &'static str,
    host: &'static str,
    category: Option<&'static str>,
}

const SITES: [Site; 17] = [
    Site {
        name: "The Lantern Daily",
        host: "lantern-daily.example",
        category: None,
    },
    Site {
        name: "Circuit Weekly",
        host: "circuit-weekly.example",
        category: Some("Tech"),
    },
    Site {
        name: "Open Road",
        host: "open-road.example",
        category: Some("Cars"),
    },
    Site {
        name: "Market Bell",
        host: "market-bell.example",
        category: Some("Business"),
    },
    Site {
        name: "Globe Desk",
        host: "globe-desk.example",
        category: Some("World"),
    },
    Site {
        name: "Full Time",
        host: "full-time.example",
        category: Some("Sports"),
    },
    Site {
        name: "Marquee",
        host: "marquee.example",
        category: Some("Entertainment"),
    },
    Site {
        name: "Pulse Health",
        host: "pulse-health.example",
        category: Some("Health"),
    },
    Site {
        name: "Wayfarer",
        host: "wayfarer.example",
        category: Some("Travel"),
    },
    Site {
        name: "Sóng Mới",
        host: "song-moi.example",
        category: None,
    },
    Site {
        name: "Harbor Times",
        host: "harbor-times.example",
        category: None,
    },
    Site {
        name: "Night Owl Tech",
        host: "night-owl.example",
        category: Some("Tech"),
    },
    Site {
        name: "Riverside Review",
        host: "riverside-review.example",
        category: None,
    },
    Site {
        name: "Morning Brief",
        host: "morning-brief.example",
        category: None,
    },
    Site {
        name: "The Weekend Table",
        host: "weekend-table.example",
        category: Some("Travel"),
    },
    Site {
        name: "Pixel & Pine",
        host: "pixel-pine.example",
        category: Some("Tech"),
    },
    Site {
        name: "Northern Star",
        host: "northern-star.example",
        category: Some("World"),
    },
];

const CATEGORIES: [&str; 8] = [
    "Tech",
    "Cars",
    "Business",
    "World",
    "Sports",
    "Entertainment",
    "Health",
    "Travel",
];

/// A made-up article: its site (in `SITES`), title, summary, minutes ago,
/// and the picture `tools/hero.py` paints for it.
struct Story {
    site: usize,
    title: &'static str,
    summary: &'static str,
    ago: i64,
    look: &'static str,
    read: bool,
}

const STORIES: [Story; 14] = [
    Story {
        site: 1,
        title: "A phone battery that lasts a week passes its first real test",
        summary: "Engineers ran it through seven days of maps, music and video calls. It came back with a fifth left.",
        ago: 24,
        look: "phone",
        read: false,
    },
    Story {
        site: 0,
        title: "The city's first night market on the river opens with 300 stalls",
        summary: "Lanterns, street food and live music every Friday and Saturday, from six until midnight.",
        ago: 52,
        look: "lanterns",
        read: false,
    },
    Story {
        site: 2,
        title: "The small electric hatchback everyone waited for finally gets a price",
        summary: "400 km on a charge, room for five and a boot that takes two suitcases.",
        ago: 95,
        look: "car",
        read: true,
    },
    Story {
        site: 3,
        title: "Coffee prices reach a ten-year high as the harvest runs late",
        summary: "Rain in the highlands held back the picking by three weeks; roasters say prices will follow.",
        ago: 140,
        look: "coffee",
        read: false,
    },
    Story {
        site: 5,
        title: "A last-minute winner sends the underdogs into their first cup final",
        summary: "Forty thousand fans stayed on their feet long after the whistle.",
        ago: 190,
        look: "stadium",
        read: false,
    },
    Story {
        site: 8,
        title: "Twelve quiet islands to visit before the crowds find them",
        summary: "White sand, clear water and a ferry twice a day: our picks for the dry season.",
        ago: 250,
        look: "island",
        read: true,
    },
    Story {
        site: 9,
        title: "Hà Nội mở thêm ba tuyến xe buýt điện từ tháng sau",
        summary: "Các tuyến mới nối trung tâm với các khu đô thị phía tây, chạy từ 5 giờ sáng đến 11 giờ đêm.",
        ago: 310,
        look: "bus",
        read: false,
    },
    Story {
        site: 4,
        title: "Ministers agree on a plan to bring the river delta's mangroves back",
        summary: "A million trees by 2030, and the fishing villages that depend on them get a say.",
        ago: 380,
        look: "delta",
        read: false,
    },
    Story {
        site: 7,
        title: "Ten minutes of walking after meals: what the new study found",
        summary: "Blood sugar rose half as much, and the walk didn't have to be fast.",
        ago: 450,
        look: "walk",
        read: false,
    },
    Story {
        site: 6,
        title: "The indie film shot on a phone that sold out every cinema in town",
        summary: "Made for less than a used car, it has now played to full houses for six weeks.",
        ago: 530,
        look: "cinema",
        read: false,
    },
    Story {
        site: 11,
        title: "Open maps pass a billion edits, most of them made by volunteers",
        summary: "Footpaths, bus stops and shop hours, mapped by people who walk past them every day.",
        ago: 640,
        look: "map",
        read: false,
    },
    Story {
        site: 10,
        title: "Fishing boats bring in the season's first catch at dawn",
        summary: "The harbour was busy before sunrise, and the market sold out by seven.",
        ago: 720,
        look: "harbor",
        read: false,
    },
    Story {
        site: 1,
        title: "The keyboard that types in three languages without switching",
        summary: "It guesses the language from the first few letters, and gets it right nine times in ten.",
        ago: 800,
        look: "keyboard",
        read: false,
    },
    Story {
        site: 0,
        title: "Rooftop gardens cool the old quarter by two degrees",
        summary: "Two hundred roofs planted in a year, and the summer nights are easier for it.",
        ago: 900,
        look: "garden",
        read: false,
    },
];

fn link(story: &Story, i: usize) -> String {
    format!("https://{}/2026/10/story-{i}.html", SITES[story.site].host)
}

/// An app over the made-up sites and articles, nothing fetched.
fn hero_app(picker: Picker) -> App {
    let at = now();
    let mut store = Store::in_memory();
    // Made-up sites only: not the first start's question.
    store.start_empty().unwrap();
    let categories: Vec<_> = CATEGORIES
        .iter()
        .map(|name| store.add_category(name).unwrap())
        .collect();
    let feeds: Vec<_> = SITES
        .iter()
        .map(|site| {
            let url = format!("https://{}/rss", site.host);
            let id = store.add_feed(&url, site.name, true).unwrap();
            store.feed_fetched(id, at, None, None).unwrap();
            if let Some(name) = site.category {
                let i = CATEGORIES.iter().position(|c| *c == name).unwrap();
                store.set_in_category(categories[i], id, true).unwrap();
            }
            id
        })
        .collect();
    for (i, story) in STORIES.iter().enumerate() {
        let item = Item {
            title: story.title.into(),
            link: link(story, i),
            summary: story.summary.into(),
            image: Some(format!("https://{}/img/{i}.jpg", SITES[story.site].host)),
            published: Some(at - story.ago * 60),
            ..Item::default()
        };
        let site = &SITES[story.site];
        store
            .save_articles(feeds[story.site], site.name, &[item], at)
            .unwrap();
        if story.read {
            store
                .mark_read(&[ArticleId::of(&link(story, i))], at)
                .unwrap();
        }
    }
    store
        .save(&ArticleId::of(&link(&STORIES[4], 4)), at)
        .unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::mem::forget(rx);
    let mut images = Images::new(picker, tx.clone(), feed::client(), None);
    images.go_offline();
    App::new(store, Settings::default(), None, feed::client(), tx, images)
}

/// `s` as a JSON string.
fn json(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A color as `"#rrggbb"`; `null` for the terminal's own.
fn hex(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("\"#{r:02x}{g:02x}{b:02x}\""),
        _ => "null".into(),
    }
}

fn rect(r: ratatui::layout::Rect) -> String {
    format!("[{}, {}, {}, {}]", r.x, r.y, r.width, r.height)
}

#[tokio::test]
async fn the_made_up_front_page_fills_the_screen() {
    let mut app = hero_app(Picker::halfblocks());
    let mut terminal = Terminal::new(TestBackend::new(140, 46)).unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("The Lantern Daily"), "{text}");
    assert!(text.contains("A phone battery"));
    assert!(app.refreshing.is_none(), "nothing fetched");
    // No color is the terminal palette's guess, which hero.py couldn't know.
    let named = terminal.backend().buffer().content().iter().any(|c| {
        !matches!(c.fg, Color::Rgb(..) | Color::Reset)
            || !matches!(c.bg, Color::Rgb(..) | Color::Reset)
    });
    assert!(!named);
}

/// One frame for `tools/hero.py`: every cell's character, colors and
/// weight, and where the pictures go. The cell size is the one hero.py
/// draws with, so pictures keep their shape.
#[tokio::test]
#[ignore = "writes target/hero/screen.json for tools/hero.py"]
async fn export_hero_screen() {
    let (cols, rows) = (140u16, 46u16);
    let font = FontSize::new(16, 34);
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(font);
    picker.set_protocol_type(ProtocolType::Kitty);
    let mut app = hero_app(picker);
    // As Ghostty shows it, whatever terminal runs the test.
    app.icons = crate::icons::NERD;
    // The keys in the articles: the selected card lit, their keys below.
    app.focus = Focus::Grid;
    let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
    // The first frame asks for the pictures; then they're ready.
    let blank = image::DynamicImage::new_rgb8(160, 90);
    for _ in 0..2 {
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        app.images.build_wanted_now(&blank);
    }
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    let mut cells = Vec::new();
    for y in 0..rows {
        for x in 0..cols {
            let cell = &buffer[(x, y)];
            // Kitty's picture cells hold its escape codes and placeholders:
            // hero.py paints the picture there.
            let picture = cell.symbol().contains(['\u{1b}', '\u{10EEEE}']);
            let symbol = if picture { " " } else { cell.symbol() };
            cells.push(format!(
                "[{}, {}, {}, {}]",
                json(symbol),
                hex(cell.fg),
                hex(cell.bg),
                cell.modifier.contains(Modifier::BOLD),
            ));
        }
    }
    let pictures: Vec<String> = app
        .images
        .placed
        .iter()
        .filter_map(|p| {
            let Subject::Article(id) = &p.subject else {
                return None;
            };
            let (_, story) = STORIES
                .iter()
                .enumerate()
                .find(|(i, story)| ArticleId::of(&link(story, *i)) == *id)?;
            Some(format!(
                "{{\"look\": {}, \"area\": {}, \"shown\": {}}}",
                json(story.look),
                rect(p.area),
                rect(p.shown)
            ))
        })
        .collect();
    assert!(!pictures.is_empty(), "the cards' pictures are placed");
    let screen = format!(
        "{{\"cols\": {cols}, \"rows\": {rows}, \"cell\": [{}, {}], \"bg\": {}, \"fg\": {}, \"cells\": [{}], \"pictures\": [{}]}}",
        font.width,
        font.height,
        hex(app.colors.bg),
        hex(app.colors.text),
        cells.join(", "),
        pictures.join(", "),
    );
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/hero");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("screen.json"), screen).unwrap();
    eprintln!("wrote {}", dir.join("screen.json").display());
}
