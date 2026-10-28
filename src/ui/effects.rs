//! Animations (`animations` in settings, on by default), drawn with
//! tachyonfx over each frame: the screen comes out of the background at
//! start (`under`, before the popups are drawn), and a popup opens out from
//! its middle (the reader fades in) while the screen behind it dims (`over`,
//! after them, from the popups `ui::clear_popup` noted).
//!
//! Only colors change, never characters (a wide title would shift a column),
//! and never over a picture: kitty's are placeholder characters whose color
//! is the picture's id, sixel's and iTerm2's are drawn once over cells the
//! frame then skips. Those cells are put back as they were drawn once the
//! animations ran.
//!
//! Frames come only while something moves (`Effects::moving`, `App::run`),
//! every `EVERY`. The dimming then stays as it ended, drawn with each frame,
//! until the popups close. Never in tests (`Effects.live`), whose frames
//! mustn't change with the time they took to draw.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use ratatui::buffer::{Buffer, Cell as Spot};
use ratatui::layout::Rect;
use ratatui::style::Color;
use tachyonfx::pattern::DiagonalPattern;
use tachyonfx::{ColorSpace, Effect, Interpolation, fx};

use crate::app::App;
use crate::images::Images;

/// What the setting turns on, for the settings tab.
pub const ABOUT: &str = "Animations: the screen and the reader fade in, popups open out";

/// How often a frame is drawn while something moves.
const EVERY: Duration = Duration::from_millis(16);
/// At most this often over ssh (`Effects.remote`): every color of the
/// screen changes as a popup opens.
const REMOTE_EVERY: Duration = Duration::from_millis(33);

/// How long each takes, in milliseconds.
const INTRO_MS: u32 = 900;
const POPUP_MS: u32 = 260;
const DIM_MS: u32 = 200;
/// How long the reader takes to fade in.
const FADE_MS: u32 = 200;
/// How far the screen behind a popup goes toward the background.
const DIM: f32 = 0.6;
/// How soft the edge of a popup opening is, in columns.
const POPUP_EDGE: f32 = 6.0;

/// What an animation is for: a new one takes the place of the one of its
/// kind. In the order they're drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Intro,
    /// The screen behind the popups, dimming, or coming back once they
    /// closed.
    Dim,
    /// The popup on top opening.
    Popup,
}

struct Running {
    kind: Kind,
    fx: Effect,
    /// Not drawn yet: it starts from its beginning, whenever the frame
    /// before was.
    fresh: bool,
    /// The dimming's: how far it starts and how far it goes.
    dims: Option<(f32, f32)>,
}

impl Running {
    /// Drawn as it ended until it's replaced: the dimming behind popups,
    /// not the brightening once they closed.
    fn held(&self) -> bool {
        self.dims.is_some_and(|(_, to)| to > 0.0)
    }
}

/// The theme's colors the animations blend with, as they are in this
/// frame: a theme picked behind a popup dims toward its own background.
#[derive(Clone, Copy, Default)]
struct Palette {
    bg: Color,
    panel: Color,
    text: Color,
}

/// A popup, as `ui::clear_popup` notes it. One that opens on top opens out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Popup {
    /// The reader: it fades in, its pictures shown as they come.
    Reader,
    Keys,
    Categories,
    Question,
}

#[derive(Default)]
pub struct Effects {
    /// Drawn to a terminal (`main`), never in tests.
    pub live: bool,
    /// Over ssh (`main`): fewer frames, `REMOTE_EVERY`.
    pub remote: bool,
    /// The popups this frame drew, the one on top last, and where
    /// (`ui::clear_popup`).
    pub popups: RefCell<Vec<(Popup, Rect)>>,
    running: Vec<Running>,
    palette: Rc<Cell<Palette>>,
    /// A frame was drawn before.
    seen: bool,
    /// The popups the frame before drew.
    seen_popups: Vec<Popup>,
    /// When the frame before was drawn, and how long it's been since, while
    /// something moved (an idle wait isn't skipped through).
    last: Option<Instant>,
    elapsed: Duration,
}

impl Effects {
    /// Something moves: the next frame is drawn in `every`. The dimming
    /// held behind a popup doesn't move.
    pub fn moving(&self) -> bool {
        self.running.iter().any(|r| r.fx.running())
    }

    /// How long until the next frame, while something moves.
    pub fn every(&self) -> Duration {
        if self.remote { REMOTE_EVERY } else { EVERY }
    }

    /// Dims the screen while `popups` are open, and opens out the one on
    /// top if it wasn't open in the frame before (not one uncovered as
    /// the one over it closed).
    fn start_over(&mut self, popups: &[(Popup, Rect)]) {
        let goal = if popups.is_empty() { 0.0 } else { DIM };
        let (now, going) = self.dimming();
        if going != goal && (goal > 0.0 || now > 0.0) {
            // From where it is: popups closed before the screen was all
            // dimmed brighten it from there.
            let dimming = dim(self.palette.clone(), now, goal);
            self.start(Kind::Dim, dimming).dims = Some((now, goal));
        }
        match popups.last() {
            Some(&(top, place)) if !self.seen_popups.contains(&top) => {
                let opening = match top {
                    Popup::Reader => fade_in(self.palette.get().bg),
                    _ => popup(self.palette.clone()),
                };
                self.start(Kind::Popup, opening.with_area(place));
            }
            Some(&(top, place)) if self.seen_popups.last() == Some(&top) => {
                self.move_to(Kind::Popup, place);
            }
            // Uncovered, or closed: what opened over it stops.
            _ => self.stop(Kind::Popup),
        }
    }

    /// How far the screen is dimmed now, and how far it's going.
    fn dimming(&self) -> (f32, f32) {
        let Some((r, (from, to))) = self
            .running
            .iter()
            .find_map(|r| r.dims.map(|dims| (r, dims)))
        else {
            return (0.0, 0.0);
        };
        let alpha = r.fx.timer().map_or(1.0, |t| t.alpha());
        (from + (to - from) * alpha, to)
    }

    fn start(&mut self, kind: Kind, fx: Effect) -> &mut Running {
        self.stop(kind);
        let at = self.running.partition_point(|r| r.kind < kind);
        let fx = Running {
            kind,
            fx,
            fresh: true,
            dims: None,
        };
        self.running.insert(at, fx);
        &mut self.running[at]
    }

    fn stop(&mut self, kind: Kind) {
        self.running.retain(|r| r.kind != kind);
    }

    /// What `kind` animates moved, or changed size.
    fn move_to(&mut self, kind: Kind, area: Rect) {
        if let Some(r) = self.running.iter_mut().find(|r| r.kind == kind) {
            r.fx.set_area(area);
        }
    }

    /// The time since the frame before, if something moved since then.
    fn tick(&mut self, now: Instant) {
        self.elapsed = match self.last {
            Some(last) if self.moving() => now.saturating_duration_since(last),
            _ => Duration::ZERO,
        };
        self.last = Some(now);
    }

    /// Draws the animations of the kinds `here` takes, then puts back the
    /// cells in `keep` as they were. Each first sees the colors its cells
    /// show (`settle`).
    fn draw(
        &mut self,
        here: impl Fn(Kind) -> bool,
        buf: &mut Buffer,
        keep: impl FnOnce() -> Vec<Rect>,
    ) {
        if !self.running.iter().any(|r| here(r.kind)) {
            return;
        }
        let screen = buf.area;
        // Only what an animation here would touch is kept.
        let touched: Vec<Rect> = self
            .running
            .iter()
            .filter(|r| here(r.kind))
            .map(|r| r.fx.area().unwrap_or(screen))
            .collect();
        let kept: Vec<(Rect, Vec<Spot>)> = keep()
            .into_iter()
            .map(|r| r.intersection(screen))
            .filter(|r| touched.iter().any(|t| t.intersects(*r)))
            .map(|r| (r, r.positions().map(|p| buf[p].clone()).collect()))
            .collect();
        let palette = self.palette.get();
        for r in self.running.iter_mut().filter(|r| here(r.kind)) {
            // Within the window, should it have shrunk.
            if let Some(area) = r.fx.area() {
                r.fx.set_area(area.intersection(screen));
            }
            settle(buf, r.fx.area().unwrap_or(screen), palette);
            let elapsed = if r.fresh {
                Duration::ZERO
            } else {
                self.elapsed
            };
            r.fresh = false;
            r.fx.process(elapsed, buf, screen);
        }
        self.running
            .retain(|r| !here(r.kind) || r.held() || r.fx.running());
        for (area, cells) in kept {
            for (p, cell) in area.positions().zip(cells) {
                buf[p] = cell;
            }
        }
    }
}

/// Where the frame drew pictures.
fn pictures(images: &Images) -> Vec<Rect> {
    // Blocks are colored cells like any other: they fade and dim with the
    // rest.
    if !images.draws_photos() {
        return Vec::new();
    }
    images.placed.iter().map(|p| p.shown).collect()
}

/// Before the popups are drawn: the first frame comes out of the
/// background, leaving the pictures be.
pub fn under(app: &mut App, buf: &mut Buffer, now: Instant) {
    let on = app.effects.live && app.settings.animations;
    let c = &app.colors;
    let effects = &mut app.effects;
    effects.palette.set(Palette {
        bg: c.bg,
        panel: c.panel,
        text: c.text,
    });
    effects.tick(now);
    if !on {
        effects.running.clear();
    } else if !effects.seen {
        let bg = effects.palette.get().bg;
        effects.start(Kind::Intro, intro(bg));
    }
    effects.seen = true;
    effects.draw(|kind| kind < Kind::Dim, buf, || pictures(&app.images));
}

/// After the popups are drawn: the screen behind them dims, leaving the
/// pictures, the popups and `status` (the status bar, where tuinews says
/// what a popup's keys did) be, and one that opened on top opens out.
pub fn over(app: &mut App, buf: &mut Buffer, status: Option<Rect>) {
    let on = app.effects.live && app.settings.animations;
    let popups = std::mem::take(app.effects.popups.get_mut());
    let effects = &mut app.effects;
    if on {
        effects.start_over(&popups);
    }
    effects.seen_popups = popups.iter().map(|&(popup, _)| popup).collect();
    // The reader's pictures show as they come; the rest of it fades in.
    let reader = matches!(popups.last(), Some(&(Popup::Reader, _)));
    effects.draw(
        |kind| kind == Kind::Dim,
        buf,
        || {
            let mut keep = pictures(&app.images);
            keep.extend(popups.iter().map(|&(_, place)| place));
            keep.extend(status);
            keep
        },
    );
    effects.draw(
        |kind| kind == Kind::Popup,
        buf,
        || {
            if reader {
                pictures(&app.images)
            } else {
                Vec::new()
            }
        },
    );
}

/// Gives the cells in `area` left to the terminal (`Reset`, as `Clear` leaves
/// them) the theme's colors before an animation blends them: tachyonfx
/// would take them for black. Every other color tuinews draws is exact
/// (`theme::WHITE`), so none is the palette's guess.
fn settle(buf: &mut Buffer, area: Rect, palette: Palette) {
    for p in area.intersection(buf.area).positions() {
        let cell = &mut buf[p];
        if cell.fg == Color::Reset {
            cell.fg = palette.text;
        }
        if cell.bg == Color::Reset {
            cell.bg = palette.bg;
        }
    }
}

/// The whole screen comes out of the background, from its top left corner
/// (a fade from a color runs backwards, so its pattern starts at the other).
fn intro(bg: Color) -> Effect {
    fx::fade_from(bg, bg, (INTRO_MS, Interpolation::QuadOut))
        .with_pattern(DiagonalPattern::bottom_right_to_top_left().with_transition_width(12.0))
        .with_color_space(ColorSpace::Rgb)
}

/// The reader comes out of the background, all of it at once.
fn fade_in(bg: Color) -> Effect {
    fx::fade_from(bg, bg, (FADE_MS, Interpolation::QuadOut)).with_color_space(ColorSpace::Rgb)
}

/// A popup comes out of its own background from its middle out, in an
/// oval as wide as its box, its edge soft (`POPUP_EDGE` columns).
fn popup(palette: Rc<Cell<Palette>>) -> Effect {
    fx::effect_fn(
        (),
        (POPUP_MS, Interpolation::QuadOut),
        move |_, context, cells| {
            let panel = palette.get().panel;
            let area = context.area;
            let middle_x = f32::from(area.x) + f32::from(area.width) / 2.0;
            let middle_y = f32::from(area.y) + f32::from(area.height) / 2.0;
            // A row is about two columns tall.
            let far = (f32::from(area.width) / 2.0).hypot(f32::from(area.height));
            let edge = context.alpha() * (far + POPUP_EDGE);
            for (p, cell) in cells {
                let x = f32::from(p.x) + 0.5 - middle_x;
                let y = 2.0 * (f32::from(p.y) + 0.5 - middle_y);
                let hidden = 1.0 - ((edge - x.hypot(y)) / POPUP_EDGE).clamp(0.0, 1.0);
                if hidden > 0.0 {
                    cell.fg = ColorSpace::Rgb.lerp(&cell.fg, &panel, hidden);
                    cell.bg = ColorSpace::Rgb.lerp(&cell.bg, &panel, hidden);
                }
            }
        },
    )
}

/// Every color behind the popups goes from `from` to `to` of the way to the
/// background, so they stand out on any theme.
fn dim(palette: Rc<Cell<Palette>>, from: f32, to: f32) -> Effect {
    fx::effect_fn(
        (),
        (DIM_MS, Interpolation::QuadOut),
        move |_, context, cells| {
            let bg = palette.get().bg;
            let amount = from + (to - from) * context.alpha();
            for (_, cell) in cells {
                cell.fg = ColorSpace::Rgb.lerp(&cell.fg, &bg, amount);
                cell.bg = ColorSpace::Rgb.lerp(&cell.bg, &bg, amount);
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::app;
    use crate::app::{HelpMenu, HelpTab};
    use crate::article::ArticleId;
    use crate::images::{Key, Subject};
    use ratatui_image::picker::ProtocolType;

    /// A screen of text in the theme's colors, with a kitty picture's
    /// placeholders in `picture`, their color its id.
    fn drawn(app: &App, picture: Rect) -> Buffer {
        let c = &app.colors;
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        for p in buf.area.positions() {
            buf[p].set_symbol("x").set_fg(c.subtext).set_bg(c.panel);
        }
        for p in picture.positions() {
            buf[p].set_symbol("\u{10EEEE}").set_fg(Color::Rgb(0, 0, 7));
        }
        buf
    }

    /// One frame at `at`, as `ui::draw` draws it: the animations under the
    /// popups, then `popups` (the one on top last) drawn as they are, then
    /// the animations over them.
    fn frame(app: &mut App, popups: &[(Popup, Rect)], at: Instant) -> Buffer {
        let mut buf = drawn(app, PICTURE);
        let as_drawn = buf.clone();
        app.images.placed.clear();
        let key = Key {
            subject: Subject::Article(ArticleId::of("https://e.vn/a")),
            cols: PICTURE.width,
            rows: PICTURE.height,
        };
        app.images.place(&key, PICTURE, PICTURE, &mut buf);
        app.help = (!popups.is_empty()).then(|| HelpMenu::new(HelpTab::Keys));
        under(app, &mut buf, at);
        for &(popup, place) in popups {
            for p in place.positions() {
                buf[p] = as_drawn[p].clone();
            }
            app.effects.popups.borrow_mut().push((popup, place));
        }
        over(app, &mut buf, None);
        buf
    }

    /// Drawn to a terminal with real pictures (kitty's: placeholders).
    fn live() -> App {
        let mut app = app();
        app.effects.live = true;
        app.images.set_protocol(ProtocolType::Kitty);
        app
    }

    /// Live, and its first frame's animation over by `start + LATER`.
    fn settled(start: Instant) -> App {
        let mut app = live();
        frame(&mut app, NONE, start);
        frame(&mut app, NONE, start + LATER);
        assert!(!app.effects.moving());
        app
    }

    fn dimmed(app: &App) -> Color {
        ColorSpace::Rgb.lerp(&app.colors.subtext, &app.colors.bg, DIM)
    }

    const PICTURE: Rect = Rect::new(2, 6, 6, 3);
    const NONE: &[(Popup, Rect)] = &[];
    const KEYS: (Popup, Rect) = (Popup::Keys, Rect::new(10, 2, 20, 6));
    const QUESTION: (Popup, Rect) = (Popup::Question, Rect::new(14, 3, 12, 4));
    const LATER: Duration = Duration::from_secs(2);

    #[tokio::test]
    async fn the_screen_comes_out_of_the_background_but_pictures_stay() {
        let mut app = live();
        let start = Instant::now();
        let first = frame(&mut app, NONE, start);
        assert_eq!(first[(39, 0)].fg, app.colors.bg, "hidden at first");
        assert!(app.effects.moving());
        let picture = drawn(&app, PICTURE);
        for p in PICTURE.positions() {
            assert_eq!(first[p], picture[p], "a picture's cells as drawn");
        }
        let end = frame(&mut app, NONE, start + LATER);
        assert_eq!(end, drawn(&app, PICTURE), "all as drawn once it's over");
        assert!(!app.effects.moving(), "no more frames");
    }

    #[tokio::test]
    async fn a_popup_dims_the_screen_behind_it_until_it_closes() {
        let start = Instant::now();
        let mut app = settled(start);
        let t = start + LATER;
        let opening = frame(&mut app, &[KEYS], t);
        assert!(app.effects.moving());
        assert_eq!(opening[(20, 5)].fg, app.colors.panel, "it opens out");
        let shown = frame(&mut app, &[KEYS], t + LATER);
        assert_eq!(shown[(0, 0)].fg, dimmed(&app), "dimmed behind it");
        assert_eq!(shown[(12, 3)].fg, app.colors.subtext, "the popup as drawn");
        assert_eq!(shown[(2, 6)], drawn(&app, PICTURE)[(2, 6)], "and pictures");
        assert!(!app.effects.moving(), "held as it ended, with no frames");
        let closed = frame(&mut app, NONE, t + LATER);
        assert_eq!(
            closed[(0, 0)].fg,
            dimmed(&app),
            "comes back from where it was"
        );
        let closed = frame(&mut app, NONE, t + 2 * LATER);
        assert_eq!(closed, drawn(&app, PICTURE));
        assert!(!app.effects.moving());
    }

    #[tokio::test]
    async fn the_reader_fades_in_but_its_pictures_stay() {
        let start = Instant::now();
        let mut app = settled(start);
        let t = start + LATER;
        let reader = (Popup::Reader, Rect::new(0, 0, 30, 10));
        let opening = frame(&mut app, &[reader], t);
        assert!(app.effects.moving());
        assert_eq!(opening[(20, 2)].fg, app.colors.bg, "hidden at first");
        let picture = drawn(&app, PICTURE);
        for p in PICTURE.positions() {
            assert_eq!(opening[p], picture[p], "its picture as drawn");
        }
        let shown = frame(&mut app, &[reader], t + LATER);
        assert_eq!(shown[(20, 2)], picture[(20, 2)], "all of it once it's in");
        assert_eq!(shown[(35, 0)].fg, dimmed(&app), "dimmed behind it");
        assert!(!app.effects.moving());
        let again = frame(&mut app, &[reader], t + LATER);
        assert_eq!(again[(20, 2)], picture[(20, 2)], "it fades in once");
    }

    #[tokio::test]
    async fn a_popup_over_another_opens_out_and_the_one_it_uncovers_stays() {
        let start = Instant::now();
        let mut app = settled(start);
        let t = start + LATER;
        frame(&mut app, &[KEYS], t);
        frame(&mut app, &[KEYS], t + LATER);
        let asked = frame(&mut app, &[KEYS, QUESTION], t + LATER);
        assert!(app.effects.moving(), "the question opens out");
        assert_eq!(asked[(20, 5)].fg, app.colors.panel);
        frame(&mut app, &[KEYS, QUESTION], t + 2 * LATER);
        let answered = frame(&mut app, &[KEYS], t + 2 * LATER);
        assert!(
            !app.effects.moving(),
            "the popup uncovered doesn't open again"
        );
        assert_eq!(answered[(20, 5)].fg, app.colors.subtext);
    }
}
