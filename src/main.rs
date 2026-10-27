mod app;
mod article;
mod config;
mod defaults;
mod feed;
#[cfg(test)]
mod hero;
mod html;
mod icons;
mod images;
mod opml;
mod pepper;
mod player;
mod proxy;
mod screen;
mod settings;
mod store;
mod text;
mod theme;
mod tmux;
mod tools;
mod typed;
mod ui;

use std::io::{Write, stdout};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// What tuinews was asked to do.
enum Command {
    Run,
    Check,
    /// Your feeds as OPML: into a new file, or else to stdout.
    Export(Option<PathBuf>),
}

fn main() -> Result<()> {
    // Before any thread exists: PATH keeps only its absolute folders, as
    // `tools::on_path` does. Code tuinews uses starts some programs by name
    // (the image library's `tmux`, crossterm's `tput`), and so does
    // xdg-open: an empty or relative entry would let one planted in the
    // folder tuinews was started from run.
    #[cfg(unix)]
    match tools::absolute_path_entries(std::env::var_os("PATH")) {
        // SAFETY: first thing in main, before the runtime or any thread.
        Some(path) => unsafe { std::env::set_var("PATH", path) },
        // SAFETY: as above.
        None => unsafe { std::env::remove_var("PATH") },
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    config::load_dotenv();
    // Not `args`, which panics on a name that isn't UTF-8: a file's may not be.
    let mut args = std::env::args_os().skip(1);
    let first = args.next().map(|a| a.to_string_lossy().into_owned());
    let command = match first.as_deref() {
        None => Command::Run,
        Some("--check") => Command::Check,
        Some("--export") => match args.next() {
            Some(file) if file.to_string_lossy().starts_with('-') => {
                bail!("--export takes a file's name, or nothing; see tuinews --help")
            }
            file => Command::Export(file.map(PathBuf::from)),
        },
        Some("-h" | "--help") => return print(&help()?),
        Some("-V" | "--version") => {
            return print(&format!("tuinews {}\n", env!("CARGO_PKG_VERSION")));
        }
        Some(other) => bail!(
            "unknown argument {:?}; see tuinews --help",
            text::clean(other)
        ),
    };
    if let Some(extra) = args.next() {
        bail!(
            "unexpected argument {:?}; see tuinews --help",
            text::clean(&extra.to_string_lossy())
        );
    }

    // Everything that can fail does so before the terminal is taken over,
    // so errors print as normal text.
    proxy::init()?;
    let config = config::Config::load()?;
    let settings_path = settings::path(&config.data_dir);
    let (settings, unknown_settings) = settings::Settings::load_checked(&settings_path)?;
    let mut store = store::Store::open(&config.data_dir.join("tuinews.db"))?;
    // A new database waits for what to start with (`App.welcome`); an older
    // one gets what's new in the defaults since: the categories.
    if !store.is_new()? {
        store.seed_defaults()?;
    }
    store.prune(settings.keep_days(), app::now())?;
    match command {
        Command::Check => return print_check(&config, &settings).await,
        Command::Export(to) => return export(&store, to.as_deref()),
        Command::Run => {}
    }
    let image_cache = config.data_dir.join("cache").join("images");
    config::private_dir(&image_cache)?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let http = feed::client();

    let mut terminal = screen::take()?;
    // With animations on, the logo from now on, while the rest loads behind
    // it (`ui::splash`).
    let splash = settings
        .animations
        .then(|| ui::splash::show(&mut terminal, &settings.theme));
    // Ask the terminal which image protocol it speaks and its cell size in
    // pixels. Must happen before key reading starts. Inside tmux this turns
    // the pane's allow-passthrough on: its old value is put back at the end.
    tmux::save();
    let picker = images::picker(settings.image_mode());
    let images = images::Images::new(picker, tx.clone(), http.clone(), Some(image_cache));
    let mut app = app::App::new(store, settings, Some(settings_path), http, tx, images);
    // Videos play in mpv's window, or inside Pepper through pepper-play.
    app.tools = tools::Tools::find();
    app.pepper = pepper::port();
    app.pepper_holes = pepper::takes_holes();
    if !unknown_settings.is_empty() {
        app.error(format!(
            "settings.toml: tuinews doesn't know {}, so it's ignored",
            unknown_settings.join(", ")
        ));
    }
    // Drawn to a terminal: the animations run (`animations` in settings),
    // with fewer frames over ssh.
    app.effects.live = true;
    app.effects.remote = ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some());
    if let Some(since) = splash {
        app.keep_splash(since);
    }
    let result = app.run(&mut terminal, rx).await;
    drop(app);

    screen::hand_back(terminal);
    result
}

/// `tuinews --check`: what tuinews found, to see why something is missing.
async fn print_check(config: &config::Config, settings: &settings::Settings) -> Result<()> {
    let env = |name: &str| text::clean(&std::env::var(name).unwrap_or_default());
    // The image query needs the terminal: it's skipped when stdout isn't one.
    let images = if std::io::IsTerminal::is_terminal(&stdout()) {
        tmux::save();
        let answer = images::check(settings.image_mode());
        tmux::restore();
        answer
    } else {
        "not a terminal".into()
    };
    let store = store::Store::open(&config.data_dir.join("tuinews.db"))?;
    let feeds = store.feeds()?;
    let failing = feeds.iter().filter(|f| f.error.is_some()).count();
    let found = tools::Tools::find();
    let shown = |path: &Option<PathBuf>| path.as_deref().map(config::shown);
    let mpv = shown(&found.mpv).unwrap_or_else(|| {
        format!(
            "not found: videos don't play ({})",
            tools::Tools::mpv_hint()
        )
    });
    let video = match (pepper::port(), shown(&found.pepper_play)) {
        _ if proxy::get().is_some() => {
            "don't play: a proxy is set, and mpv could be led around it".into()
        }
        (Some(_), Some(play)) if settings.inline_video => {
            let holes = if pepper::takes_holes() {
                "a popup over one shows, as it plays on"
            } else {
                "an older Pepper: one is hidden while a popup is open"
            };
            format!("inside Pepper, through {play} ({holes})")
        }
        // The inside case is above: here it'd be mpv's window, with none.
        _ if found.mpv.is_none() => "don't play: mpv isn't installed".into(),
        (Some(_), Some(_)) => "in mpv's window (inline_video is off)".into(),
        (Some(_), None) => "in mpv's window: Pepper's pepper-play wasn't found".into(),
        (None, _) if std::env::var_os("PEPPER_PORT").is_some() => {
            "in mpv's window: Pepper, but through a multiplexer (tmux, screen, zellij)".into()
        }
        (None, _) => "in mpv's window (inside the terminal in Pepper)".into(),
    };
    print(&format!(
        "tuinews {version}
data folder  {data}
feeds        {count} ({failing} failed the last time they were fetched)
proxy        {proxy}
terminal     TERM={term} TERM_PROGRAM={program} {tmux}
images       {images}
mpv          {mpv}
videos       {video}
",
        version = env!("CARGO_PKG_VERSION"),
        data = config::shown(&config.data_dir),
        count = feeds.len(),
        proxy = proxy::shown(),
        term = env("TERM"),
        program = env("TERM_PROGRAM"),
        tmux = if env("TMUX").is_empty() {
            ""
        } else {
            "(in tmux)"
        },
    ))
}

/// `tuinews --export [FILE]`: your feeds and categories as OPML, into FILE
/// (a new file) or else to stdout.
fn export(store: &store::Store, to: Option<&Path>) -> Result<()> {
    let (feeds, categories) = (store.feeds()?, store.categories()?);
    let opml = opml::write(&feeds, &categories, chrono::Utc::now());
    let Some(path) = to else {
        return print(&opml);
    };
    opml::save(path, &opml)?;
    print(&format!(
        "Wrote {} and {} to {}\n",
        ui::plural(feeds.len(), "feed", "feeds"),
        ui::plural(categories.len(), "category", "categories"),
        config::shown(path)
    ))
}

/// Writes to stdout. A reader that stops early (`tuinews --help | head -1`)
/// is fine, where `println!` would panic on the closed pipe.
fn print(text: &str) -> Result<()> {
    match stdout().lock().write_all(text.as_bytes()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => Ok(result?),
    }
}

fn help() -> Result<String> {
    Ok(format!(
        "tuinews {version}
The news in your terminal: your RSS feeds as a grid of article cards with
their pictures, a reader for their text, arrow keys or vim keys.

Usage: tuinews [-h | --help] [-V | --version] [--check] [--export [FILE]]

Inside the app, ? lists the keys and q quits. --check says what tuinews
found: your feeds, the proxy, and how your terminal draws images.

--export writes your feeds and categories as OPML, for another reader or to
keep: into FILE (a new file), or else to the screen. To bring them in, press
a in tuinews and give the file's path.

The first time, it asks whether to start with Vietnam's biggest news sites
(VnExpress, Tuổi Trẻ, Thanh Niên, Dân Trí, VietnamNet and more) or with no
feeds. a adds a feed (its address, or a site's that names one), or those of
an OPML file another reader exported; x on one in the sidebar removes it.

Your feeds, what you've read, saved and opened, and your settings are kept
only on this computer, in:
  {data}

Environment:
  TN_DATA_DIR            keep them somewhere else (a full path)
  TN_IMAGES              how pictures are drawn: auto, kitty, sixel, iterm2
                         or blocks (also `images` in settings.toml)
  TN_ICONS               which icons: auto, nerd (Nerd Font) or plain
                         (also `icons` in settings.toml)
  TN_MPV                 the mpv that plays an article's videos, if not
                         the one on PATH
  TN_PEPPER_PLAY         in Pepper, its pepper-play, if not where Pepper
                         says (PEPPER_PLAY)
  https_proxy, ...       one http:// or https:// proxy for everything: the
                         first set of https_proxy, HTTPS_PROXY, all_proxy,
                         ALL_PROXY, http_proxy, HTTP_PROXY
",
        version = env!("CARGO_PKG_VERSION"),
        data = config::shown(&config::data_dir()?),
    ))
}
