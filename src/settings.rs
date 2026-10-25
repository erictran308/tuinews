//! User settings, kept in `settings.toml` in the data directory.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::article::{self, Article};
use crate::icons::IconMode;
use crate::images::ImageMode;
use crate::{config, theme};

/// Entries of `blocked_words` read, at most.
const MAX_BLOCKED: usize = 1000;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The theme in use: one of the built-in ones (picked in Settings: `O`).
    pub theme: String,
    /// Animations: the logo while tuinews starts (`ui::splash`), then the
    /// screen comes out of the background, popups open out (the reader
    /// fades in) with the screen dimmed behind them. Settings (`O`) turns
    /// them on and off.
    pub animations: bool,
    /// Show the start of each article's summary on its card.
    pub summaries: bool,
    /// Show every view as a list, two rows per article, instead of cards
    /// with pictures: for terminals that don't draw images well. `v`
    /// switches.
    pub list: bool,
    /// Show only the articles you haven't read. `u` switches.
    pub unread_only: bool,
    /// Read an article's own text from its page when you open it, as a
    /// browser's reader view does: one request to the site, as opening it
    /// in the browser would be. Off: the reader shows the feed's summary.
    pub full_text: bool,
    /// Remember the articles you open here, in History. Kept only on this
    /// computer.
    pub history: bool,
    /// How old a feed may get before it's fetched again, in minutes: at
    /// least 5, at most a week. `R` fetches them all now.
    pub refresh_minutes: u32,
    /// How long articles are kept, in days, unless saved or in History.
    pub keep_days: u32,
    /// Words whose articles never show, outside Saved and History: a title
    /// holding one, as a whole word and in any case, hides its article. A
    /// phrase counts as one word.
    pub blocked_words: Vec<String>,
    /// How images are drawn: "auto" (what the terminal says it can do),
    /// "kitty", "sixel", "iterm2" or "blocks". `TN_IMAGES` overrides it.
    pub images: ImageMode,
    /// Which icons: "auto" (Nerd Font icons in Ghostty, kitty and WezTerm,
    /// which have them built in), "nerd" or "plain". `TN_ICONS` overrides it.
    pub icons: IconMode,
    /// The narrowest an article card may be, in columns.
    pub card_width: u16,
    /// The sidebar's width, in columns.
    pub sidebar_width: u16,
    /// The widest the reader's text goes, in columns.
    pub reader_width: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: theme::DEFAULT.into(),
            animations: true,
            summaries: true,
            list: false,
            unread_only: false,
            full_text: true,
            history: true,
            refresh_minutes: 15,
            keep_days: 30,
            blocked_words: Vec::new(),
            images: ImageMode::Auto,
            icons: IconMode::Auto,
            card_width: 32,
            sidebar_width: 28,
            reader_width: 90,
        }
    }
}

/// The settings tuinews reads.
const KNOWN: [&str; 15] = [
    "theme",
    "animations",
    "summaries",
    "list",
    "unread_only",
    "full_text",
    "history",
    "refresh_minutes",
    "keep_days",
    "blocked_words",
    "images",
    "icons",
    "card_width",
    "sidebar_width",
    "reader_width",
];

impl Settings {
    /// Defaults if the file doesn't exist yet; an error if it can't be read.
    #[cfg(test)]
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self::load_checked(path)?.0)
    }

    /// The settings, and the names in the file tuinews doesn't know: a typo
    /// (`histroy = false`) would otherwise be ignored without a word.
    pub fn load_checked(path: &Path) -> Result<(Self, Vec<String>)> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::default(), Vec::new()));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("cannot read {}", config::shown(path)));
            }
        };
        let shown = config::shown(path);
        let table: toml::Table =
            toml::from_str(&text).with_context(|| format!("{shown} is invalid"))?;
        let unknown: Vec<String> = table
            .keys()
            .filter(|key| !KNOWN.contains(&key.as_str()))
            .map(|key| crate::text::clean(key))
            .collect();
        let settings: Self =
            toml::from_str(&text).with_context(|| format!("{shown} is invalid"))?;
        Ok((settings, unknown))
    }

    /// The card width as used: wide enough for a title, whatever the file
    /// says.
    pub fn card_width(&self) -> u16 {
        self.card_width.clamp(20, 80)
    }

    pub fn sidebar_width(&self) -> u16 {
        self.sidebar_width.clamp(16, 50)
    }

    pub fn reader_width(&self) -> u16 {
        self.reader_width.clamp(40, 200)
    }

    /// How old a feed may get, in seconds.
    pub fn refresh_every(&self) -> i64 {
        i64::from(self.refresh_minutes.clamp(5, 7 * 24 * 60)) * 60
    }

    /// How long articles are kept, in days.
    pub fn keep_days(&self) -> u32 {
        self.keep_days.clamp(1, 3650)
    }

    /// The image mode in use: `TN_IMAGES` if it names one, else the file's.
    pub fn image_mode(&self) -> ImageMode {
        config::var("TN_IMAGES")
            .and_then(|v| ImageMode::parse(&v))
            .unwrap_or(self.images)
    }

    /// The icon mode in use: `TN_ICONS` if it names one, else the file's.
    pub fn icon_mode(&self) -> IconMode {
        config::var("TN_ICONS")
            .and_then(|v| IconMode::parse(&v))
            .unwrap_or(self.icons)
    }

    /// `blocked_words`, read once for a whole list: in lower case, none
    /// empty (which would match everything).
    pub fn blocks(&self) -> Blocks {
        Blocks {
            words: self
                .blocked_words
                .iter()
                .take(MAX_BLOCKED)
                .map(|word| article::one_line(word, article::MAX_TITLE).to_lowercase())
                .filter(|word| !word.is_empty())
                .collect(),
        }
    }

    /// How many words are blocked, as the settings tab says.
    pub fn blocked(&self) -> usize {
        self.blocks().words.len()
    }

    /// Sets one setting in settings.toml, as the file is now: whatever was
    /// written there since tuinews started (by hand, or by another tuinews)
    /// stays, so a `history = false` added meanwhile isn't undone. A file that
    /// no longer reads is left alone. Returns the settings the file now holds.
    pub fn save_one(path: &Path, key: &str, value: toml::Value) -> Result<Self> {
        let shown = config::shown(path);
        let mut table: toml::Table = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("{shown} is invalid, so it wasn't changed"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
            Err(e) => return Err(e).with_context(|| format!("cannot read {shown}")),
        };
        table.insert(key.to_string(), value);
        let text = toml::to_string(&table)?;
        let settings: Self = toml::from_str(&text)?;
        write_private(path, &text).with_context(|| format!("cannot write {shown}"))?;
        Ok(settings)
    }
}

/// `blocked_words`, read once to match many articles against.
#[derive(Default)]
pub struct Blocks {
    words: Vec<String>,
}

impl Blocks {
    /// Whether `article`'s title holds a blocked word.
    pub fn hold(&self, article: &Article) -> bool {
        self.hides(&article.title)
    }

    /// Whether `title` holds a blocked word.
    pub fn hides(&self, title: &str) -> bool {
        if self.words.is_empty() {
            return false;
        }
        let title = title.to_lowercase();
        self.words.iter().any(|word| holds_word(&title, word))
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

/// Whether `text` holds `word` (both in lower case) as a whole word, not
/// inside a longer one: `ai` isn't in "maintain". Scripts written without
/// spaces between words (Chinese, Japanese, Thai) match anywhere.
fn holds_word(text: &str, word: &str) -> bool {
    // Letters and digits of scripts that put spaces between words.
    let runs_on = |c: char| {
        c.is_alphanumeric()
            && !matches!(
                u32::from(c),
                0x0E00..=0x109F | 0x1780..=0x17FF | 0x2E80..=0x9FFF | 0xF900..=0xFAFF | 0xFF66..=0xFF9F
            )
    };
    let (Some(first), Some(last)) = (word.chars().next(), word.chars().next_back()) else {
        return false;
    };
    text.match_indices(word).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + word.len()..].chars().next();
        !(before.is_some_and(runs_on) && runs_on(first))
            && !(after.is_some_and(runs_on) && runs_on(last))
    })
}

/// Where the settings are kept.
pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.toml")
}

/// Writes `text` to `path` readable only by the user, and all at once: a new
/// file of this process's own renamed over the old one.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let new = path.with_extension(format!("toml.{}.new", std::process::id()));
    let _ = std::fs::remove_file(&new);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&new)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&new, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tuinews-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::config::private_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_settings_are_the_defaults_and_unknown_names_are_reported() {
        let dir = dir("settings");
        let path = path(&dir);
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
        std::fs::write(&path, "histroy = false\ntheme = \"nord\"\n").unwrap();
        let (settings, unknown) = Settings::load_checked(&path).unwrap();
        assert_eq!(settings.theme, "nord");
        assert_eq!(unknown, ["histroy"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_one_setting_keeps_what_was_written_meanwhile() {
        let dir = dir("save-one");
        let path = path(&dir);
        std::fs::write(&path, "history = false\n").unwrap();
        let saved = Settings::save_one(&path, "theme", "nord".into()).unwrap();
        assert!(!saved.history, "written by hand meanwhile");
        assert_eq!(saved.theme, "nord");
        assert_eq!(Settings::load(&path).unwrap(), saved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blocked_words_match_whole_words_in_any_case() {
        assert!(holds_word("giá vàng hôm nay", "vàng"));
        assert!(!holds_word("maintain", "ai"));
        assert!(holds_word("an ai story", "ai"));
        let settings = Settings {
            blocked_words: vec!["Showbiz".into(), "  ".into()],
            ..Settings::default()
        };
        assert_eq!(settings.blocked(), 1);
    }
}
