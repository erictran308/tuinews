//! Articles' pictures.
//!
//! Every frame, the cards and the reader ask for the pictures they have on
//! screen. After the frame, [`Images::fetch`] starts whatever is missing: a
//! download (https only, from a named host, at most [`MAX_BYTES`], kept in
//! a disk cache by the article's id), then decode and encode for the
//! terminal on a blocking thread, within size limits, with a panic in a
//! decoder caught.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use image::DynamicImage;
use image::imageops::FilterType;
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui_image::picker::cap_parser::Parser;
use ratatui_image::picker::{Capability, Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::protocol::kitty::Kitty;
use ratatui_image::sliced::SlicedProtocol;
use ratatui_image::{FontSize, Resize};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::AppEvent;
use crate::article::{ArticleId, checked_link};

/// The biggest picture downloaded. News sites' are 50–500 KB.
const MAX_BYTES: usize = 5 * 1024 * 1024;
/// At most this many pictures decode at once.
const MAX_BUILDING: usize = 4;
/// Pictures downloading at once.
const MAX_DOWNLOADS: usize = 6;
/// Encoded pictures kept; the ones drawn longest ago go first.
const MAX_READY: usize = 300;
/// Pictures of articles' text kept, the same way.
const MAX_SLICED: usize = 80;
/// What the pictures kept may take in a terminal speaking kitty's protocol,
/// in bytes of pixels. Such a terminal keeps every picture it's sent until
/// told to delete it, and past its quota (320 MB in Ghostty and kitty) it
/// drops the ones sent first: the cards of Home behind the reader, which
/// then stay blank, as a picture is sent only with its first drawing. Half
/// the quota leaves room for what's on screen past it.
const KITTY_BUDGET: usize = 160_000_000;
/// A picture still building after this long counts as failed.
const BUILD_TIMEOUT: Duration = Duration::from_secs(40);

/// Which picture: an article's own, or one in its text (named by the hash
/// of its address, as an article is by its link's).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Subject {
    Article(ArticleId),
    /// Drawn in rows (`SlicedProtocol`), so it can scroll out of the top of
    /// the reader a row at a time, at its own shape.
    Inline(ArticleId),
}

/// One encoded picture: a picture at one size in cells.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub subject: Subject,
    pub cols: u16,
    pub rows: u16,
}

/// What building a picture made.
enum Built {
    /// A card's or the reader's own picture, cropped to fill its box.
    Whole(Protocol, Option<KittyImage>),
    /// A picture in an article's text, in rows.
    Sliced(SlicedProtocol, Option<KittyImage>),
    /// A picture in an article's text whose shape (its size in pixels) is
    /// far from its box's, laid out before its shape was known: it's asked
    /// for again at its own.
    Shape,
}

/// Numbers of kitty pictures dropped since the terminal was last told; see
/// [`Images::deletions`].
type Dropped = Arc<Mutex<Vec<u32>>>;

/// A picture made for kitty's protocol: the number the terminal knows it
/// by and the bytes it takes there once it's been sent, with its first
/// drawing. Dropping one sent, wherever that happens, adds its number to
/// those the terminal is told to delete.
struct KittyImage {
    id: u32,
    bytes: usize,
    sent: bool,
    dropped: Dropped,
}

impl Drop for KittyImage {
    fn drop(&mut self) {
        if self.sent {
            self.dropped
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(self.id);
        }
    }
}

/// A picture kept, encoded for the terminal.
struct Encoded<T> {
    image: T,
    /// The frame it was last drawn (or asked for) in.
    used: u64,
    /// Deletes it from the terminal once it's dropped.
    kitty: Option<KittyImage>,
}

impl<T> Encoded<T> {
    /// It's in the terminal: a kitty picture drawn once.
    fn sent(&self) -> bool {
        self.kitty.as_ref().is_some_and(|k| k.sent)
    }

    /// What it takes in a terminal speaking kitty's protocol: nothing until
    /// it's sent.
    fn sent_bytes(&self) -> usize {
        match &self.kitty {
            Some(kitty) if self.sent() => kitty.bytes,
            _ => 0,
        }
    }
}

/// A picture finished encoding (or failed) on a background thread, with
/// its size in pixels once decoded.
pub struct ImageEvent {
    key: Key,
    result: Result<Built>,
    shape: Option<(u32, u32)>,
}

impl std::fmt::Debug for ImageEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageEvent")
            .field("key", &self.key)
            .field("shape", &self.shape)
            .finish()
    }
}

/// How far a picture's shape may be from its box's before it's asked for
/// again at its own: a part in this many.
const SHAPE_SLACK: f32 = 0.08;

/// The widest and tallest picture decoded. News sites' are at most about
/// 2000 px across; a much bigger one is only a way to run memory out.
const MAX_SIDE: u32 = 4096;

/// Caps for decoding, as [`MAX_SIDE`] says.
fn limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(128 * 1024 * 1024);
    limits
}

/// Decodes a JPEG, PNG, WebP or GIF picture (a GIF's first frame), the
/// formats news sites serve, and no other: a dependency may compile in more
/// decoders (arboard's TIFF) than tuinews wants to run on downloaded bytes.
fn decode(data: &[u8]) -> Result<DynamicImage> {
    use image::ImageFormat::{Gif, Jpeg, Png, WebP};
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
    if !matches!(reader.format(), Some(Jpeg | Png | WebP | Gif)) {
        bail!("not a JPEG, PNG, WebP or GIF picture");
    }
    if reader.format() == Some(WebP) {
        check_webp(data)?;
    }
    reader.limits(limits());
    Ok(reader.decode()?)
}

/// A WebP keeps its size in more than one place: the canvas (which
/// [`limits`] checks) and each frame's own header, from which the decoder
/// sizes its buffers before comparing the two. So a small file saying 1×1
/// outside and 16383×16383 inside would take hundreds of MB. Every frame
/// header is checked here first, against the canvas and [`MAX_SIDE`].
fn check_webp(data: &[u8]) -> Result<()> {
    let chunks = data.get(12..).context("a cut-off WebP picture")?;
    check_webp_chunks(chunks, MAX_SIDE, MAX_SIDE, false)
}

fn check_webp_chunks(mut rest: &[u8], max_w: u32, max_h: u32, in_frame: bool) -> Result<()> {
    let (mut max_w, mut max_h) = (max_w, max_h);
    let le24 = |b: &[u8]| u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16;
    for _ in 0..64 {
        if rest.len() < 8 {
            return Ok(());
        }
        let size = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
        let payload = rest
            .get(8..8usize.saturating_add(size))
            .context("a cut-off WebP chunk")?;
        let (w, h) = match &rest[..4] {
            b"VP8X" if !in_frame => {
                let p = payload.get(..10).context("a cut-off WebP header")?;
                let (w, h) = (le24(&p[4..7]) + 1, le24(&p[7..10]) + 1);
                (max_w, max_h) = (w.min(max_w), h.min(max_h));
                (w, h)
            }
            b"VP8 " => {
                // A key frame: its tag, the start code, then 14-bit sizes.
                let p = payload.get(..10).context("a cut-off WebP frame")?;
                if p[0] & 1 != 0 || p[3..6] != [0x9d, 0x01, 0x2a] {
                    bail!("not a WebP key frame");
                }
                let size = |b: &[u8]| u32::from(u16::from_le_bytes([b[0], b[1]]) & 0x3fff);
                (size(&p[6..8]), size(&p[8..10]))
            }
            b"VP8L" => {
                let p = payload.get(..5).context("a cut-off WebP frame")?;
                if p[0] != 0x2f {
                    bail!("not a WebP lossless frame");
                }
                let bits = u32::from_le_bytes([p[1], p[2], p[3], p[4]]);
                ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
            }
            b"ANMF" if !in_frame => {
                let p = payload.get(..16).context("a cut-off WebP frame")?;
                let (w, h) = (le24(&p[6..9]) + 1, le24(&p[9..12]) + 1);
                if w <= max_w && h <= max_h {
                    check_webp_chunks(&payload[16..], w, h, true)?;
                }
                (w, h)
            }
            b"VP8X" | b"ANMF" => bail!("a WebP chunk where it can't be"),
            _ => (0, 0),
        };
        if w > max_w || h > max_h {
            bail!("a WebP frame bigger than its picture ({w}×{h})");
        }
        let next = 8usize.saturating_add(size).saturating_add(size & 1);
        rest = rest.get(next..).unwrap_or_default();
    }
    bail!("a WebP picture in too many pieces")
}

thread_local! {
    /// Set while this thread decodes a downloaded picture.
    static DECODING: Cell<bool> = const { Cell::new(false) };
}

/// The panic is in a picture decoder, where it's caught: the picture shows
/// as broken and the app carries on.
pub fn panic_is_contained() -> bool {
    DECODING.with(Cell::get)
}

fn contained<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
    DECODING.with(|d| d.set(true));
    let result = catch_unwind(AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the picture couldn't be decoded")));
    DECODING.with(|d| d.set(false));
    result
}

/// `photo` cropped to fill `width`×`height` from its middle.
pub fn fill(photo: &DynamicImage, width: u32, height: u32) -> DynamicImage {
    if width == 0 || height == 0 {
        return DynamicImage::new_rgba8(width.max(1), height.max(1));
    }
    photo.resize_to_fill(width, height, FilterType::Triangle)
}

/// Whether a picture may be fetched from `url`: https, and a link
/// `article::checked_link` passes. Pictures are never asked for in the
/// clear, which would tell the network what you read.
pub fn allowed(url: &str) -> bool {
    url.starts_with("https://") && checked_link(url, None).is_some()
}

/// Which way pictures are drawn: what the terminal said it can do, unless
/// the settings (`images`) or `TN_IMAGES` say otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageMode {
    #[default]
    Auto,
    Kitty,
    Sixel,
    Iterm2,
    /// Colored half blocks: works in any terminal, coarse.
    Blocks,
}

impl ImageMode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "kitty" => Self::Kitty,
            "sixel" => Self::Sixel,
            "iterm2" => Self::Iterm2,
            "blocks" | "halfblocks" => Self::Blocks,
            _ => return None,
        })
    }
}

/// Asks the terminal how it draws pictures, then applies `mode`. Call after
/// the terminal is set up and before keys are read. With a mode set
/// (`images`, `TN_IMAGES`) nothing is asked: a terminal that doesn't answer
/// leaves the question's thread reading stdin, where it would take the
/// first keys typed for its answer.
pub fn picker(mode: ImageMode) -> Picker {
    let detected = if mode == ImageMode::Auto {
        Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
    } else {
        Picker::halfblocks()
    };
    choose(detected, mode, |name| std::env::var(name).ok(), cell_size())
}

/// Whether the terminal answered the image query at all: a terminal that
/// doesn't draw images still reports its cell size or sixel support.
fn answered(picker: &Picker) -> bool {
    picker.protocol_type() != ProtocolType::Halfblocks || !picker.capabilities().is_empty()
}

/// The picker to use. A terminal that didn't answer the query, but says
/// by its environment that it's Ghostty or kitty, gets the kitty protocol:
/// both draw it, and an answer can go missing (a slow start, something
/// between tuinews and the terminal). A terminal that answered is believed.
fn choose(
    detected: Picker,
    mode: ImageMode,
    env: impl Fn(&str) -> Option<String>,
    cell: Option<FontSize>,
) -> Picker {
    let font = if answered(&detected) {
        detected.font_size()
    } else {
        cell.unwrap_or(detected.font_size())
    };
    let forced = |protocol| {
        // Deprecated in favor of asking the terminal, which is what failed
        // here; it's the only way to give a picker the cell size.
        #[allow(deprecated)]
        let mut picker = Picker::from_fontsize(font);
        picker.set_protocol_type(protocol);
        picker
    };
    match mode {
        ImageMode::Kitty => forced(ProtocolType::Kitty),
        ImageMode::Sixel => forced(ProtocolType::Sixel),
        ImageMode::Iterm2 => forced(ProtocolType::Iterm2),
        ImageMode::Blocks => forced(ProtocolType::Halfblocks),
        ImageMode::Auto if answered(&detected) || detected.tmux_detected() => detected,
        ImageMode::Auto if kitty_terminal(&env) && cell.is_some() => forced(ProtocolType::Kitty),
        ImageMode::Auto => detected,
    }
}

/// The environment says the terminal is Ghostty or kitty.
fn kitty_terminal(env: &impl Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| env(name).filter(|v| !v.is_empty());
    set("TERM_PROGRAM").is_some_and(|p| p.eq_ignore_ascii_case("ghostty"))
        || set("TERM").is_some_and(|t| t == "xterm-ghostty" || t == "xterm-kitty")
        || set("KITTY_WINDOW_ID").is_some()
}

/// The terminal's cell size in pixels, from the window size it reports.
#[cfg(unix)]
pub fn cell_size() -> Option<FontSize> {
    // SAFETY: TIOCGWINSZ only fills in the winsize given; on a stdout that
    // isn't a terminal it fails and nothing is read.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } != 0 {
        return None;
    }
    let (w, h) = (size.ws_xpixel, size.ws_ypixel);
    let (cols, rows) = (size.ws_col, size.ws_row);
    (w > 0 && h > 0 && cols > 0 && rows > 0).then(|| FontSize::new(w / cols, h / rows))
}

#[cfg(not(unix))]
pub fn cell_size() -> Option<FontSize> {
    None
}

/// What `tuinews --check` says about pictures: what the terminal answered,
/// and what's used.
pub fn check(mode: ImageMode) -> String {
    let detected = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let answer = if answered(&detected) {
        format!("the terminal answered {}", name(detected.protocol_type()))
    } else {
        "no answer from the terminal".into()
    };
    let used = choose(detected, mode, |name| std::env::var(name).ok(), cell_size());
    let font = used.font_size();
    let mut out = format!(
        "{}; {answer}; cells {}×{} px",
        name(used.protocol_type()),
        font.width,
        font.height
    );
    if used.protocol_type() == ProtocolType::Halfblocks {
        out.push_str(
            "\n             (pictures are coarse; if your terminal draws images, \
             set images = \"kitty\" in settings.toml)",
        );
    }
    out
}

fn name(protocol: ProtocolType) -> &'static str {
    match protocol {
        ProtocolType::Kitty => "kitty graphics",
        ProtocolType::Sixel => "sixel",
        ProtocolType::Iterm2 => "iTerm2 images",
        ProtocolType::Halfblocks => "colored blocks",
    }
}

pub struct Images {
    picker: Picker,
    tx: UnboundedSender<AppEvent>,
    http: reqwest::Client,
    cache_dir: Option<PathBuf>,
    downloads: Arc<Semaphore>,
    /// At most `MAX_BUILDING` pictures decode at once, however many are
    /// downloading.
    decoding: Arc<Semaphore>,
    /// Encoded pictures: cards' and the reader's own, and those of
    /// articles' text, in rows.
    ready: HashMap<Key, Encoded<Protocol>>,
    sliced: HashMap<Key, Encoded<SlicedProtocol>>,
    /// Pictures' sizes in pixels, once decoded: the reader lays out a
    /// picture in its text at its own shape.
    shapes: HashMap<Subject, (u32, u32)>,
    frame: u64,
    building: HashMap<Key, Instant>,
    failed: HashSet<Key>,
    /// Pictures the last frame showed, with the URL to fetch.
    wanted: Vec<(Key, String)>,
    /// Nothing is downloaded (tests).
    offline: bool,
    /// Where the last frame drew each picture, for the animations to leave
    /// be.
    pub placed: Vec<Placement>,
    /// Pictures are fetched and made ready but not drawn (`hold`).
    holding: bool,
    /// How many pictures the last frame asked for that weren't ready and
    /// hadn't failed; `None` before a frame.
    missing: Option<usize>,
    /// The number the last picture built for kitty's protocol was given.
    kitty_id: u32,
    /// Pictures dropped since the terminal was last told.
    dropped: Dropped,
    /// The code that sends the pictures the last frame drew for the first
    /// time, for after it ([`place`](Self::place)).
    transmits: String,
    /// See [`KITTY_BUDGET`]; smaller in tests.
    kitty_budget: usize,
}

/// A picture drawn in a frame: the area it was made for, and the part of
/// it on screen (less, for a card cut off by the window's bottom).
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub subject: Subject,
    pub area: ratatui::layout::Rect,
    pub shown: ratatui::layout::Rect,
}

impl Images {
    pub fn new(
        picker: Picker,
        tx: UnboundedSender<AppEvent>,
        http: reqwest::Client,
        cache_dir: Option<PathBuf>,
    ) -> Self {
        if let Some(dir) = cache_dir.clone() {
            std::thread::spawn(move || tidy_cache(&dir));
        }
        Self {
            picker,
            tx,
            http,
            cache_dir,
            downloads: Arc::new(Semaphore::new(MAX_DOWNLOADS)),
            decoding: Arc::new(Semaphore::new(MAX_BUILDING)),
            ready: HashMap::new(),
            sliced: HashMap::new(),
            shapes: HashMap::new(),
            frame: 0,
            building: HashMap::new(),
            failed: HashSet::new(),
            wanted: Vec::new(),
            offline: false,
            placed: Vec::new(),
            holding: false,
            missing: None,
            kitty_id: kitty_id_base(),
            dropped: Dropped::default(),
            transmits: String::new(),
            kitty_budget: KITTY_BUDGET,
        }
    }

    /// Downloads nothing from now on.
    #[cfg(test)]
    pub fn go_offline(&mut self) {
        self.offline = true;
    }

    /// Notes where a picture was drawn this frame, `key`'s, in `buf`. A
    /// kitty picture's first drawing puts the code that sends it in the
    /// first cell drawn, the top left of what's `shown`, ahead of its
    /// placeholder: it's taken out of there, to be sent after the frame
    /// ([`take_transmits`](Self::take_transmits)). Whatever the frame draws
    /// over that cell later (the reader, a popup, the corner video's box)
    /// then covers only the placeholder, and the picture still comes.
    pub fn place(&mut self, key: &Key, area: Rect, shown: Rect, buf: &mut Buffer) {
        if self.picker.protocol_type() == ProtocolType::Kitty
            && let Some(cell) = buf.cell_mut(shown.as_position())
            && take_transmit(cell, &mut self.transmits)
            && let Some(kitty) = self.kitty_of(key)
        {
            kitty.sent = true;
        }
        self.placed.push(Placement {
            subject: key.subject.clone(),
            area,
            shown,
        });
    }

    /// The code that sends the pictures the last frame drew for the first
    /// time, taken out of their cells ([`place`](Self::place)); empty when
    /// there are none. Written right after the frame (`App::run`).
    pub fn take_transmits(&mut self) -> String {
        std::mem::take(&mut self.transmits)
    }

    /// `key`'s picture's kitty image, in whichever list holds it.
    fn kitty_of(&mut self, key: &Key) -> Option<&mut KittyImage> {
        match key.subject {
            Subject::Article(_) => self.ready.get_mut(key)?.kitty.as_mut(),
            Subject::Inline(_) => self.sliced.get_mut(key)?.kitty.as_mut(),
        }
    }

    /// Draws with `protocol` from now on: for a test of what a terminal with
    /// real pictures gets.
    #[cfg(test)]
    pub fn set_protocol(&mut self, protocol: ProtocolType) {
        self.picker.set_protocol_type(protocol);
    }

    /// Makes every picture the last frame wanted ready at once, from
    /// `photo`, on this thread: for drawing a frame with pictures in a test.
    #[cfg(test)]
    pub fn build_wanted_now(&mut self, photo: &DynamicImage) {
        let font = self.picker.font_size();
        for (key, _) in std::mem::take(&mut self.wanted) {
            let (width, height) = (
                u32::from(key.cols) * u32::from(font.width),
                u32::from(key.rows) * u32::from(font.height),
            );
            let kitty = self.next_kitty();
            if let Ok(built) = encode(&self.picker, &key, photo, width, height, kitty) {
                self.keep(key, built);
            }
        }
    }

    /// False when the terminal shows no real pictures, only colored blocks
    /// (halfblocks).
    pub fn draws_photos(&self) -> bool {
        self.picker.protocol_type() != ProtocolType::Halfblocks
    }

    /// The terminal paints each picture from one cell, over anything drawn
    /// on top since: sixel and iTerm2's protocol. Nothing may be drawn over
    /// such pictures, and they mustn't be drawn under popups.
    pub fn paints_over(&self) -> bool {
        matches!(
            self.picker.protocol_type(),
            ProtocolType::Sixel | ProtocolType::Iterm2
        )
    }

    pub fn font_size(&self) -> FontSize {
        self.picker.font_size()
    }

    /// The picture ready to draw, if it is; else it's fetched after the
    /// frame, from `url`.
    pub fn get(&mut self, key: &Key, url: &str) -> Option<&Protocol> {
        self.ready_to_draw(|images| &mut images.ready, key, url)
    }

    /// A picture of an article's text, ready to draw in rows, if it is;
    /// else it's fetched after the frame, from `url`.
    pub fn get_sliced(&mut self, key: &Key, url: &str) -> Option<&SlicedProtocol> {
        self.ready_to_draw(|images| &mut images.sliced, key, url)
    }

    /// [`get`](Self::get) from the pictures `kept` gives.
    fn ready_to_draw<T>(
        &mut self,
        kept: impl Fn(&mut Self) -> &mut HashMap<Key, Encoded<T>>,
        key: &Key,
        url: &str,
    ) -> Option<&T> {
        if !kept(self).contains_key(key) {
            self.want(key, url);
            return None;
        }
        // Sixel and iTerm2 pictures would be painted over the splash; a
        // kitty one is sent apart from the frame, its placeholders under it.
        let held = self.holding && self.paints_over();
        let frame = self.frame;
        let picture = kept(self).get_mut(key)?;
        picture.used = frame;
        // Kept for when they can be drawn.
        (!held).then_some(&picture.image)
    }

    /// The frame asked for a picture that isn't ready: it's fetched after it.
    fn want(&mut self, key: &Key, url: &str) {
        if !self.failed.contains(key) {
            *self.missing.get_or_insert(0) += 1;
        }
        self.wanted.push((key.clone(), url.to_string()));
    }

    /// While the splash screen covers the frame (`ui::splash`): pictures
    /// are fetched and made ready as usual, but sixel and iTerm2 ones
    /// aren't drawn, as they would be painted over the splash.
    pub fn hold(&mut self, holding: bool) {
        self.holding = holding;
    }

    /// A frame begins: where it draws pictures, and what it misses, are
    /// noted afresh.
    pub fn new_frame(&mut self) {
        self.placed.clear();
        self.missing = Some(0);
    }

    /// How many pictures the last frame asked for that aren't ready yet
    /// (failed ones don't count); `None` before a frame.
    pub fn missing(&self) -> Option<usize> {
        self.missing
    }

    /// A picture's size in pixels, once it has been decoded.
    pub fn shape(&self, subject: &Subject) -> Option<(u32, u32)> {
        self.shapes.get(subject).copied()
    }

    pub fn is_broken(&self, key: &Key) -> bool {
        self.failed.contains(key)
    }

    /// Starts downloads and encodes for what the last frame wanted.
    pub fn fetch(&mut self) {
        self.frame += 1;
        self.keep_within_budget();
        let failed = &mut self.failed;
        self.building.retain(|key, started| {
            let alive = started.elapsed() < BUILD_TIMEOUT;
            if !alive {
                failed.insert(key.clone());
            }
            alive
        });
        for (key, url) in std::mem::take(&mut self.wanted) {
            if self.ready.contains_key(&key)
                || self.sliced.contains_key(&key)
                || self.building.contains_key(&key)
                || self.failed.contains(&key)
            {
                continue;
            }
            if self.building.len() >= MAX_BUILDING * 3 {
                break;
            }
            if self.offline || !allowed(&url) {
                self.failed.insert(key);
                continue;
            }
            self.building.insert(key.clone(), Instant::now());
            let kitty = self.next_kitty();
            self.spawn(key, url, kitty);
        }
    }

    fn spawn(&self, key: Key, url: String, kitty: Option<(u32, Dropped)>) {
        let picker = self.picker.clone();
        let tx = self.tx.clone();
        let http = self.http.clone();
        let downloads = self.downloads.clone();
        let cache = self
            .cache_dir
            .as_ref()
            .map(|dir| dir.join(cache_name(&key.subject)));
        let font = self.picker.font_size();
        // Laid out at its own shape, the box is as near to it as whole cells
        // come: it's drawn in that, not asked for again without end.
        let laid_at_shape = self.shapes.contains_key(&key.subject);
        let decoding = self.decoding.clone();
        tokio::spawn(async move {
            let (data, from_cache) = match load(&http, &downloads, &url, cache.as_deref()).await {
                Ok(loaded) => loaded,
                Err(e) => {
                    let _ = tx.send(AppEvent::Image(ImageEvent {
                        key,
                        result: Err(e),
                        shape: None,
                    }));
                    return;
                }
            };
            let Ok(_permit) = decoding.acquire_owned().await else {
                return;
            };
            let build_key = key.clone();
            let data = Arc::new(data);
            let bytes = data.clone();
            let decoded = tokio::task::spawn_blocking(move || {
                contained(|| build(&picker, font, &build_key, &bytes, laid_at_shape, kitty))
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the picture couldn't be decoded")));
            let (result, shape) = match decoded {
                Ok((built, shape)) => (Ok(built), Some(shape)),
                Err(e) => (Err(e), None),
            };
            // Only a picture that decoded is kept, and a kept one that no
            // longer does is thrown away, to be fetched again.
            if let Some(path) = &cache {
                match (&result, from_cache) {
                    (Ok(_), false) => {
                        let _ = write_private(path, &data).await;
                    }
                    (Err(_), true) => {
                        let _ = tokio::fs::remove_file(path).await;
                    }
                    _ => {}
                }
            }
            let _ = tx.send(AppEvent::Image(ImageEvent { key, result, shape }));
        });
    }

    pub fn on_built(&mut self, event: ImageEvent) {
        self.building.remove(&event.key);
        if let Some(shape) = event.shape {
            self.shapes.insert(event.key.subject.clone(), shape);
        }
        match event.result {
            Ok(built) => self.keep(event.key, built),
            Err(_) => {
                self.failed.insert(event.key);
            }
        }
    }

    /// Keeps a picture built, within [`MAX_READY`] or [`MAX_SLICED`].
    fn keep(&mut self, key: Key, built: Built) {
        match built {
            Built::Whole(image, kitty) => {
                self.keep_in(|images| &mut images.ready, MAX_READY, key, image, kitty);
            }
            Built::Sliced(image, kitty) => {
                self.keep_in(|images| &mut images.sliced, MAX_SLICED, key, image, kitty);
            }
            // Asked for again, at its shape, by the next frame.
            Built::Shape => {}
        }
    }

    /// Keeps `image` among the pictures `kept` gives, at most `max` of them.
    /// One it replaces counts no more.
    fn keep_in<T>(
        &mut self,
        kept: impl Fn(&mut Self) -> &mut HashMap<Key, Encoded<T>>,
        max: usize,
        key: Key,
        image: T,
        kitty: Option<KittyImage>,
    ) {
        kept(self).remove(&key);
        evict(kept(self), max);
        let used = self.frame;
        kept(self).insert(key, Encoded { image, used, kitty });
    }

    /// Keeps what the pictures sent take in a terminal speaking kitty's
    /// protocol within [`KITTY_BUDGET`]: after each frame, once the
    /// pictures it drew are sent (one not sent takes nothing there). The
    /// pictures of articles' text go first, those drawn longest ago first:
    /// each takes several MB, against a card's one or two, and the reader
    /// shows a few at a time, while the cards behind it are what closing it
    /// shows. Never one the frame drew or asked for, which the next would
    /// build again, pushing out another, frame after frame: those may go
    /// past it.
    fn keep_within_budget(&mut self) {
        if self.picker.protocol_type() != ProtocolType::Kitty {
            return;
        }
        let mut taken = self.sent_bytes();
        if taken <= self.kitty_budget {
            return;
        }
        let in_use = self.frame.saturating_sub(1);
        // In the order they go: the text's (`false`) first.
        let mut order: Vec<(bool, u64, Key)> = droppable(&self.sliced, in_use)
            .map(|(used, key)| (false, used, key))
            .chain(droppable(&self.ready, in_use).map(|(used, key)| (true, used, key)))
            .collect();
        order.sort_unstable_by_key(|&(whole, used, _)| (whole, used));
        for (whole, _, key) in order {
            if taken <= self.kitty_budget {
                break;
            }
            let freed = if whole {
                self.ready.remove(&key).map(|p| p.sent_bytes())
            } else {
                self.sliced.remove(&key).map(|p| p.sent_bytes())
            };
            taken -= freed.unwrap_or(0);
        }
    }

    /// What the pictures sent take in a terminal speaking kitty's protocol.
    fn sent_bytes(&self) -> usize {
        let whole = self.ready.values().map(Encoded::sent_bytes);
        let sliced = self.sliced.values().map(Encoded::sent_bytes);
        whole.chain(sliced).sum()
    }

    /// A number for the next picture built with kitty's protocol, and where
    /// it goes once that picture is dropped.
    fn next_kitty(&mut self) -> Option<(u32, Dropped)> {
        (self.picker.protocol_type() == ProtocolType::Kitty).then(|| {
            // Never 0, which kitty's protocol takes as no number.
            self.kitty_id = self.kitty_id.wrapping_add(1).max(1);
            (self.kitty_id, self.dropped.clone())
        })
    }

    /// Drops every picture kept, as tuinews quits: a terminal speaking
    /// kitty's protocol would keep them after it's gone.
    pub fn drop_all(&mut self) {
        self.ready.clear();
        self.sliced.clear();
    }

    /// The code that has a terminal speaking kitty's protocol delete the
    /// pictures dropped since it was last told, wherever that happened
    /// (`KittyImage`), freeing what they took there; empty when there are
    /// none. Written before each frame, ahead of any picture it sends, and
    /// on quitting (`App::send_deletions`).
    pub fn deletions(&self) -> String {
        let ids = std::mem::take(&mut *self.dropped.lock().unwrap_or_else(PoisonError::into_inner));
        let tmux = self.picker.tmux_detected();
        ids.into_iter().map(|id| kitty_delete(id, tmux)).collect()
    }

    /// Forgets failures, so a refresh tries those pictures again.
    pub fn retry_failed(&mut self) {
        self.failed.clear();
    }
}

/// Makes room for one more in `kept`, at most `max`: the one drawn longest
/// ago goes.
fn evict<T>(kept: &mut HashMap<Key, Encoded<T>>, max: usize) {
    if kept.len() >= max
        && let Some(key) = kept
            .iter()
            .min_by_key(|(_, p)| p.used)
            .map(|(key, _)| key.clone())
    {
        kept.remove(&key);
    }
}

/// The pictures in `kept` that may go to make room in the terminal: sent,
/// and last drawn before frame `in_use`; with when each was.
fn droppable<T>(
    kept: &HashMap<Key, Encoded<T>>,
    in_use: u64,
) -> impl Iterator<Item = (u64, Key)> + '_ {
    kept.iter()
        .filter(move |(_, p)| p.sent() && p.used < in_use)
        .map(|(key, p)| (p.used, key.clone()))
}

/// Decodes `data` and encodes it for the terminal at `key`'s size; and its
/// size in pixels. A picture of the text `laid_at_shape` is never sent back
/// to be laid out again.
fn build(
    picker: &Picker,
    font: FontSize,
    key: &Key,
    data: &[u8],
    laid_at_shape: bool,
    kitty: Option<(u32, Dropped)>,
) -> Result<(Built, (u32, u32))> {
    let photo = decode(data)?;
    let shape = (photo.width(), photo.height());
    let (width, height) = (
        u32::from(key.cols) * u32::from(font.width),
        u32::from(key.rows) * u32::from(font.height),
    );
    let built = match key.subject {
        Subject::Article(_) => encode(picker, key, &photo, width, height, kitty)?,
        Subject::Inline(_) => {
            let wanted = width as f32 / height.max(1) as f32;
            let own = shape.0 as f32 / shape.1.max(1) as f32;
            if !laid_at_shape && (wanted / own - 1.0).abs() > SHAPE_SLACK {
                Built::Shape
            } else {
                encode(picker, key, &photo, width, height, kitty)?
            }
        }
    };
    Ok((built, shape))
}

/// Encodes a decoded picture for the terminal at `key`'s size, cropped to
/// fill it (`width`×`height` pixels): whole for an article's own, in rows
/// for one of its text's. With kitty's protocol it's sent as the picture
/// numbered by `kitty`, picked here rather than at random by the picker, so
/// it can be deleted from the terminal once it's dropped.
fn encode(
    picker: &Picker,
    key: &Key,
    photo: &DynamicImage,
    width: u32,
    height: u32,
    kitty: Option<(u32, Dropped)>,
) -> Result<Built> {
    let image = fill(photo, width, height);
    let size = Size::new(key.cols, key.rows);
    let sliced = matches!(key.subject, Subject::Inline(_));
    // The picker's `new_protocol` with `Resize::Fit` uses a picture as it
    // is when its own size in cells is its box's and a side is whole
    // cells, as `fill` makes it: then the same `Kitty` is made here, but
    // numbered by tuinews. `SlicedProtocol::new_with_resize` makes its
    // kitty pictures with `new_protocol` too.
    let font = picker.font_size();
    let as_is = Resize::natural_size(&image, font) == size
        && (image.width() == u32::from(size.width) * u32::from(font.width)
            || image.height() == u32::from(size.height) * u32::from(font.height));
    if let Some((id, dropped)) = kitty
        && as_is
    {
        let bytes = image.width() as usize * image.height() as usize * 4;
        let compress = picker
            .capabilities()
            .contains(&Capability::KittyCompression);
        let image = Kitty::new(image, size, id, picker.tmux_detected(), compress)?;
        let kitty = Some(KittyImage {
            id,
            bytes,
            sent: false,
            dropped,
        });
        return Ok(if sliced {
            Built::Sliced(SlicedProtocol::Kitty(image), kitty)
        } else {
            Built::Whole(Protocol::Kitty(image), kitty)
        });
    }
    // Anything else as the picker makes it, numbered at random: tuinews
    // can't name it to delete it.
    Ok(if sliced {
        let image = SlicedProtocol::new_with_resize(picker, image, size, Resize::Fit(None))?;
        Built::Sliced(image, None)
    } else {
        Built::Whole(picker.new_protocol(image, size, Resize::Fit(None))?, None)
    })
}

/// Where numbers for kitty's pictures start: at random, as the picker's
/// own are, since every program drawing in the terminal's window shares
/// them (tuinews and tuitube in panes of one tmux), and one's deletions
/// mustn't take the other's pictures.
fn kitty_id_base() -> u32 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(std::process::id()) as u32
}

/// The code that has the terminal delete picture number `id` of kitty's
/// protocol. `I` deletes the picture itself, not only where it's placed;
/// `q=2` asks for no answer, which would come in as keys.
fn kitty_delete(id: u32, tmux: bool) -> String {
    let (start, escape, end) = Parser::tmux_start_escape_end(tmux);
    format!("{start}{escape}_Ga=d,d=I,i={id},q=2{escape}\\{end}")
}

/// The character kitty's protocol draws a picture's cells with, ratatui-image
/// and the terminal both.
const PLACEHOLDER: char = '\u{10EEEE}';

/// Takes the code that sends a kitty picture out of `cell`, the first one a
/// picture's first drawing draws, where ratatui-image puts it before the
/// placeholder (wrapped for tmux, if need be), onto `out`, and leaves the
/// placeholder. Whether there was one.
fn take_transmit(cell: &mut ratatui::buffer::Cell, out: &mut String) -> bool {
    let symbol = cell.symbol();
    if !symbol.starts_with('\x1b') {
        return false;
    }
    let Some(at) = symbol.rfind(PLACEHOLDER) else {
        return false;
    };
    out.push_str(&symbol[..at]);
    let placeholder = symbol[at..].to_string();
    cell.set_symbol(&placeholder);
    true
}

/// The cache file for a picture: named by its checked id only.
fn cache_name(subject: &Subject) -> String {
    match subject {
        Subject::Article(id) => format!("a-{id}"),
        Subject::Inline(id) => format!("i-{id}"),
    }
}

/// How long a cached picture is kept after it was fetched.
const CACHE_FOR: Duration = Duration::from_secs(14 * 24 * 3600);

/// Deletes cached pictures fetched more than `CACHE_FOR` ago, and leftovers
/// of interrupted writes, so the cache doesn't grow without end.
fn tidy_cache(dir: &std::path::Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_none_or(|age| age > CACHE_FOR);
        if name.ends_with(".new") || old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The picture's bytes, and whether they came from the cache: from there if
/// they're in it, else from `url`.
async fn load(
    http: &reqwest::Client,
    downloads: &Semaphore,
    url: &str,
    cache: Option<&std::path::Path>,
) -> Result<(Vec<u8>, bool)> {
    if let Some(path) = cache
        && let Ok(meta) = tokio::fs::metadata(path).await
        && meta.len() as usize <= MAX_BYTES
        && let Ok(data) = tokio::fs::read(path).await
    {
        return Ok((data, true));
    }
    let _permit = downloads.acquire().await?;
    Ok((download(http, url).await?, false))
}

async fn download(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let mut response = http
        .get(url)
        .header(
            reqwest::header::ACCEPT,
            "image/webp, image/png, image/jpeg, image/gif",
        )
        .send()
        .await?;
    if !response.status().is_success() {
        bail!("HTTP {}", response.status().as_u16());
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if data.len() + chunk.len() > MAX_BYTES {
            bail!("the picture is too big");
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

/// Writes a cache file readable only by you, through a rename.
async fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    // A name of its own, made new: two writers never share one, and an
    // existing file (or link) is never written through.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let new = path.with_extension(format!("{}-{n}.new", std::process::id()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&new).await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, data).await?;
    drop(file);
    tokio::fs::rename(&new, path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;
    use ratatui_image::Image;
    use ratatui_image::sliced::{SignedPosition, SlicedImage};

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut data = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(width, height)
            .write_to(&mut data, image::ImageFormat::Png)
            .unwrap();
        data.into_inner()
    }

    #[test]
    fn pictures_fill_their_card() {
        let photo = image::RgbImage::new(600, 315).into();
        let filled = fill(&photo, 320, 180).to_rgb8();
        assert_eq!(filled.dimensions(), (320, 180));
    }

    #[test]
    fn pictures_far_bigger_than_news_sites_send_are_refused() {
        assert!(decode(&png(512, 512)).is_ok());
        assert!(decode(&png(5000, 1)).is_err());
    }

    #[test]
    fn only_pictures_over_https_from_named_hosts_are_fetched() {
        assert!(allowed(
            "https://i1-vnexpress.vnecdn.net/2026/10/23/a.jpg?w=1200"
        ));
        for bad in [
            "http://i1-vnexpress.vnecdn.net/a.jpg",
            "https://127.0.0.1/a.jpg",
            "https://localhost/a.jpg",
            "file:///etc/passwd",
        ] {
            assert!(!allowed(bad), "{bad}");
        }
    }

    /// A RIFF/WEBP file made of these chunks.
    fn webp(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = b"WEBP".to_vec();
        for (fourcc, payload) in chunks {
            body.extend_from_slice(*fourcc);
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                body.push(0);
            }
        }
        let mut file = b"RIFF".to_vec();
        file.extend_from_slice(&(body.len() as u32).to_le_bytes());
        file.extend(body);
        file
    }

    /// A lossy key frame's first bytes, saying it's `w`×`h`.
    fn vp8(w: u16, h: u16) -> Vec<u8> {
        let mut p = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a];
        p.extend_from_slice(&w.to_le_bytes());
        p.extend_from_slice(&h.to_le_bytes());
        p.extend_from_slice(&[0; 8]);
        p
    }

    fn vp8x(w: u32, h: u32) -> Vec<u8> {
        let mut p = vec![0; 4];
        p.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
        p.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
        p
    }

    #[test]
    fn a_webp_whose_frame_is_bigger_than_it_says_is_refused_before_decoding() {
        // 1×1 outside, 16383×16383 inside: about 384 MB if decoded.
        let sneaky = webp(&[(b"VP8X", vp8x(1, 1)), (b"VP8 ", vp8(16383, 16383))]);
        assert!(sneaky.len() < 64);
        let error = decode(&sneaky).unwrap_err().to_string();
        assert!(error.contains("bigger than its picture"), "{error}");
        let huge = webp(&[(b"VP8 ", vp8(5000, 5000))]);
        assert!(decode(&huge).is_err());
        // Sizes that fit are let through to the decoder.
        assert!(
            check_webp(&webp(&[
                (b"VP8X", vp8x(1280, 720)),
                (b"VP8 ", vp8(1280, 720))
            ]))
            .is_ok()
        );
        let mut real = Vec::new();
        DynamicImage::new_rgba8(4, 3)
            .write_to(
                &mut std::io::Cursor::new(&mut real),
                image::ImageFormat::WebP,
            )
            .unwrap();
        assert_eq!(
            decode(&real).unwrap().width(),
            4,
            "a real WebP still decodes"
        );
    }

    #[test]
    fn only_jpeg_png_webp_and_gif_are_decoded() {
        // A TIFF header: arboard compiles the decoder in, tuinews won't use it.
        let mut fake = b"II*\0".to_vec();
        fake.extend_from_slice(&[8, 0, 0, 0, 0, 0]);
        assert!(decode(&fake).is_err());
        let mut gif = Vec::new();
        DynamicImage::new_rgba8(4, 3)
            .write_to(&mut std::io::Cursor::new(&mut gif), image::ImageFormat::Gif)
            .unwrap();
        assert_eq!(decode(&gif).unwrap().height(), 3, "a GIF's first frame");
        assert!(decode(&png(4, 4)).is_ok(), "PNG still works");
    }

    #[tokio::test]
    async fn a_decoder_panic_marks_the_picture_broken_instead_of_ending_the_app() {
        let result: Result<()> = contained(|| panic!("a decoder bug"));
        assert!(result.is_err());
        assert!(!panic_is_contained(), "only while decoding");
    }

    fn env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn ghostty_gets_kitty_images_even_when_its_answer_went_missing() {
        let cell = Some(FontSize::new(13, 30));
        let ghostty = env(&[("TERM_PROGRAM", "ghostty"), ("TERM", "xterm-256color")]);
        let picker = choose(Picker::halfblocks(), ImageMode::Auto, ghostty, cell);
        assert_eq!(picker.protocol_type(), ProtocolType::Kitty);
        let font = picker.font_size();
        assert_eq!((font.width, font.height), (13, 30));

        // Not a kitty terminal, or no cell size to lay images out by: blocks.
        let plain = env(&[("TERM_PROGRAM", "Apple_Terminal")]);
        let picker = choose(Picker::halfblocks(), ImageMode::Auto, plain, cell);
        assert_eq!(picker.protocol_type(), ProtocolType::Halfblocks);
        let ghostty = env(&[("TERM_PROGRAM", "ghostty")]);
        let picker = choose(Picker::halfblocks(), ImageMode::Auto, ghostty, None);
        assert_eq!(picker.protocol_type(), ProtocolType::Halfblocks);

        // The settings win.
        let plain = env(&[]);
        let picker = choose(Picker::halfblocks(), ImageMode::Sixel, plain, cell);
        assert_eq!(picker.protocol_type(), ProtocolType::Sixel);
        assert_eq!(ImageMode::parse(" Kitty "), Some(ImageMode::Kitty));
        assert_eq!(ImageMode::parse("nope"), None);
    }

    #[test]
    fn pictures_from_where_they_may_not_come_are_never_fetched() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut images = Images::new(Picker::halfblocks(), tx, crate::feed::client(), None);
        let key = Key {
            subject: Subject::Article(ArticleId::of("https://e.vn/a")),
            cols: 4,
            rows: 2,
        };
        let url = "http://192.168.1.1/x.jpg";
        assert!(images.get(&key, url).is_none());
        images.fetch();
        assert!(images.is_broken(&key));
    }

    #[test]
    fn held_pictures_are_made_ready_but_not_drawn() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut images = Images::new(Picker::halfblocks(), tx, crate::feed::client(), None);
        images.go_offline();
        // Sixel's: painted over anything drawn after, as the splash is.
        images.set_protocol(ProtocolType::Sixel);
        let key = Key {
            subject: Subject::Article(ArticleId::of("https://e.vn/a")),
            cols: 4,
            rows: 2,
        };
        let url = "https://e.vn/a.jpg";
        assert_eq!(images.missing(), None, "no frame yet");
        images.hold(true);
        images.new_frame();
        assert!(images.get(&key, url).is_none());
        assert_eq!(images.missing(), Some(1));
        let (built, shape) = build(
            &images.picker,
            images.font_size(),
            &key,
            &png(64, 36),
            false,
            None,
        )
        .unwrap();
        images.on_built(ImageEvent {
            key: key.clone(),
            result: Ok(built),
            shape: Some(shape),
        });
        images.new_frame();
        assert!(images.get(&key, url).is_none(), "held");
        assert_eq!(images.missing(), Some(0), "but in");
        images.hold(false);
        assert!(images.get(&key, url).is_some());
        // Kitty's are sent apart from the frame, so they're drawn under it.
        images.set_protocol(ProtocolType::Kitty);
        images.hold(true);
        assert!(images.get(&key, url).is_some(), "kitty's aren't held");
        images.hold(false);

        // One that can't come isn't waited for.
        let broken = Key {
            subject: Subject::Article(ArticleId::of("https://e.vn/b")),
            ..key
        };
        images.get(&broken, url);
        images.fetch();
        images.new_frame();
        images.get(&broken, url);
        assert_eq!(images.missing(), Some(0));
    }

    /// Pictures for a terminal speaking kitty's protocol, numbered from 1.
    fn kitty() -> Images {
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut images = Images::new(picker, tx, crate::feed::client(), None);
        images.go_offline();
        images.kitty_id = 0;
        images
    }

    /// A card's picture: 4 × 2 cells of 10 × 20 px, 40 × 40.
    fn card(n: u8) -> Key {
        Key {
            subject: Subject::Article(ArticleId::of(&format!("https://e.vn/{n}"))),
            cols: 4,
            rows: 2,
        }
    }

    /// A picture in an article's text: 8 × 4 cells, 80 × 80 px.
    fn inline(n: u8) -> Key {
        Key {
            subject: Subject::Inline(ArticleId::of(&format!("https://e.vn/{n}.jpg"))),
            cols: 8,
            rows: 4,
        }
    }

    /// Where `key`'s picture goes: at the top left.
    fn at(key: &Key) -> Rect {
        Rect::new(0, 0, key.cols, key.rows)
    }

    /// Builds `key`'s picture and keeps it, as one that came would be.
    fn put(images: &mut Images, key: &Key) {
        let kitty = images.next_kitty();
        let font = images.font_size();
        let (built, _) = build(&images.picker, font, key, &png(40, 40), true, kitty).unwrap();
        images.keep(key.clone(), built);
    }

    /// Draws `key`'s picture as a frame does, if it's ready: the frame.
    fn draw(images: &mut Images, key: &Key) -> Option<Buffer> {
        let area = at(key);
        let mut buf = Buffer::empty(area);
        let drawn = match key.subject {
            Subject::Article(_) => images
                .get(key, URL)
                .map(|image| Image::new(image).render(area, &mut buf)),
            Subject::Inline(_) => images
                .get_sliced(key, URL)
                .map(|image| SlicedImage::new(image, TOP_LEFT).render(area, &mut buf)),
        };
        drawn?;
        images.place(key, area, area, &mut buf);
        Some(buf)
    }

    const URL: &str = "https://e.vn/a.jpg";
    const TOP_LEFT: SignedPosition = SignedPosition { x: 0, y: 0 };

    #[test]
    fn a_kitty_picture_is_the_one_the_picker_would_make_but_numbered_by_tuinews() {
        let mut images = kitty();
        let picker = images.picker.clone();
        let photo = decode(&png(64, 36)).unwrap();
        // The code in its first cell that sends it, without its number.
        let code = |buf: &Buffer| {
            let first = buf[(0, 0)].symbol();
            let code = &first[..first.find('\u{10EEEE}').unwrap()];
            let at = code.find("i=").unwrap();
            let end = at + code[at..].find(',').unwrap();
            format!("{}{}", &code[..at], &code[end..])
        };
        for key in [card(1), inline(1)] {
            let (width, height) = (u32::from(key.cols) * 10, u32::from(key.rows) * 20);
            let size = Size::new(key.cols, key.rows);
            let area = at(&key);
            let (mut ours, mut theirs) = (Buffer::empty(area), Buffer::empty(area));
            let filled = fill(&photo, width, height);
            let built = encode(&picker, &key, &photo, width, height, images.next_kitty());
            let number = format!("i={},", images.kitty_id);
            let sizes = match built.unwrap() {
                Built::Whole(image, Some(_)) => {
                    let made = picker
                        .new_protocol(filled, size, Resize::Fit(None))
                        .unwrap();
                    Image::new(&image).render(area, &mut ours);
                    Image::new(&made).render(area, &mut theirs);
                    (image.size(), made.size())
                }
                Built::Sliced(image, Some(_)) => {
                    let made =
                        SlicedProtocol::new_with_resize(&picker, filled, size, Resize::Fit(None))
                            .unwrap();
                    SlicedImage::new(&image, TOP_LEFT).render(area, &mut ours);
                    SlicedImage::new(&made, TOP_LEFT).render(area, &mut theirs);
                    (image.size(), made.size())
                }
                _ => panic!("made by tuinews, to be deleted"),
            };
            assert_eq!(sizes.0, sizes.1, "{key:?}");
            assert_eq!(code(&ours), code(&theirs), "{key:?}");
            assert!(
                ours[(0, 0)].symbol().contains(&number),
                "numbered by tuinews"
            );
        }
    }

    #[test]
    fn a_new_kitty_picture_is_sent_after_the_frame_and_not_from_its_first_cell() {
        let mut images = kitty();
        for (n, key) in [card(1), inline(1)].iter().enumerate() {
            put(&mut images, key);
            let mut frame = draw(&mut images, key).unwrap();
            let first = frame[(0, 0)].symbol();
            assert!(first.starts_with(PLACEHOLDER), "{first:?}");
            assert!(!first.contains('\x1b'), "the code is out of it: {first:?}");
            // Whatever the frame draws there later (the reader, a popup, the
            // corner video's box) covers only the placeholder.
            frame[(0, 0)].set_symbol("─");
            let code = images.take_transmits();
            assert_eq!(code.matches("a=T").count(), 1, "{key:?}");
            assert!(code.contains(&format!("i={},a=T", n + 1)), "{code:?}");
            assert_eq!(images.take_transmits(), "", "queued once");
            // The next frame draws only its placeholders.
            draw(&mut images, key).unwrap();
            assert_eq!(images.take_transmits(), "", "a second frame sends nothing");
        }
    }

    #[test]
    fn a_kitty_terminal_is_told_to_delete_the_pictures_it_was_sent_by_their_numbers() {
        let mut images = kitty();
        let tmux = images.picker.tmux_detected();
        let delete = |id| kitty_delete(id, tmux);
        put(&mut images, &card(1));
        draw(&mut images, &card(1));
        assert!(images.take_transmits().contains("i=1,a=T"), "sent as 1");
        assert_eq!(images.deletions(), "", "nothing dropped yet");

        // Built again, it replaces the one sent.
        put(&mut images, &card(1));
        assert_eq!(images.deletions(), delete(1));
        assert_eq!(images.deletions(), "", "told once");
        // One never drawn was never sent: there's nothing to delete.
        put(&mut images, &card(1));
        assert_eq!(images.deletions(), "", "never sent");
        draw(&mut images, &card(1));

        // Past the most kept, the one drawn longest ago goes.
        for n in 0..MAX_SLICED as u8 {
            images.frame += 1;
            put(&mut images, &inline(n));
            draw(&mut images, &inline(n));
        }
        assert_eq!(images.deletions(), "", "all kept");
        images.frame += 1;
        put(&mut images, &inline(200));
        assert_eq!(images.deletions(), delete(4), "the first of them");

        // Quitting drops the rest, and those sent are deleted.
        images.drop_all();
        let code = images.deletions();
        assert_eq!(code.matches("a=d").count(), MAX_SLICED, "{code}");
        assert!(code.contains(&delete(3)), "the card");
        assert!(!code.contains(&delete(84)), "the last, never drawn");
    }

    #[test]
    fn after_each_frame_the_pictures_sent_go_back_under_the_quota_but_not_those_on_screen() {
        let mut images = kitty();
        let tmux = images.picker.tmux_detected();
        images.kitty_budget = 2 * 40 * 40 * 4;
        // A frame draws three cards: all on screen, past the quota.
        for n in 1..=3 {
            put(&mut images, &card(n));
            draw(&mut images, &card(n));
        }
        images.fetch();
        assert_eq!(images.ready.len(), 3);
        assert_eq!(images.deletions(), "");

        // The next shows the first and the third.
        draw(&mut images, &card(1));
        draw(&mut images, &card(3));
        images.fetch();
        assert!(!images.ready.contains_key(&card(2)));
        assert_eq!(images.deletions(), kitty_delete(2, tmux));

        // One not sent yet (say, just come) takes nothing in the terminal.
        put(&mut images, &card(4));
        draw(&mut images, &card(1));
        draw(&mut images, &card(3));
        images.fetch();
        assert_eq!(images.ready.len(), 3);
        assert_eq!(images.deletions(), "");
    }

    #[test]
    fn the_pictures_of_articles_go_before_the_cards_behind_the_reader() {
        let mut images = kitty();
        // A card and one picture of an article's text.
        images.kitty_budget = 40 * 40 * 4 + 80 * 80 * 4;
        // Drawn before the reader opened.
        put(&mut images, &card(1));
        draw(&mut images, &card(1));
        images.frame = 2;
        put(&mut images, &inline(1));
        draw(&mut images, &inline(1));
        // The next article's.
        images.frame = 4;
        put(&mut images, &inline(2));
        draw(&mut images, &inline(2));
        images.fetch();
        assert!(
            images.ready.contains_key(&card(1)),
            "though drawn longest ago"
        );
        assert!(!images.sliced.contains_key(&inline(1)));
        assert!(images.sliced.contains_key(&inline(2)));
    }

    #[test]
    fn inside_tmux_the_code_that_deletes_pictures_goes_through_it() {
        assert_eq!(kitty_delete(1, false), "\x1b_Ga=d,d=I,i=1,q=2\x1b\\");
        assert_eq!(
            kitty_delete(1, true),
            "\x1bPtmux;\x1b\x1b_Ga=d,d=I,i=1,q=2\x1b\x1b\\\x1b\\"
        );
    }

    #[test]
    fn terminals_that_keep_no_pictures_are_told_nothing() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut images = Images::new(Picker::halfblocks(), tx, crate::feed::client(), None);
        for key in [card(1), card(1), inline(1)] {
            put(&mut images, &key);
            draw(&mut images, &key);
        }
        images.drop_all();
        assert_eq!(images.take_transmits(), "");
        assert_eq!(images.deletions(), "");
    }
}
