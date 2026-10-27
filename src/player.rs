//! Playing an article's video: mpv, run as a separate program, in its own
//! window; or, in Pepper, pepper-play, Pepper's player (`pepper`), inside
//! the terminal, in the video's box. pepper-play takes mpv's options and
//! JSON IPC, so both are driven alike. A trimmed copy of tuitube's
//! (`../tuitube/src/player.rs`): what's here, keep in step with it.
//!
//! mpv starts with `--no-config` (none of the user's mpv.conf, input.conf
//! or scripts), `--terminal=no` (it never touches tuinews's terminal),
//! `--ytdl=no` (it runs no yt-dlp) and `--tls-verify=yes` (mpv's own
//! default accepts any certificate). What it opens is kept to the stream an
//! article named: FFmpeg's demuxers only (`--demuxer=lavf`: mpv's own
//! playlist reader would play what a playlist lists from anywhere), and
//! those only over https (`protocol_whitelist`), an HLS playlist's parts
//! too. On Unix it's controlled over one end of a socket pair handed to it
//! as file descriptor 3 (`--input-ipc-client=fd://3`): there's no socket
//! file another program could connect to, and mpv quits when tuinews's end
//! closes. mpv's control protocol can run programs, so it must never be
//! reachable by anyone else.
//!
//! On Unix, mpv starts idle with nothing about the video on its command
//! line, which every user of the computer can read (`ps`): the title and
//! the URL go over the socket. Windows has no socket yet, so they're
//! arguments there, after `--`; other users can't read another user's
//! command line on Windows, and tuinews can't pause it or make it fill the
//! screen there (mpv's own keys do).

use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::tools;

/// How long mpv has to quit when asked, before it's killed.
const QUIT_GRACE: Duration = Duration::from_secs(3);

/// The protocols FFmpeg may open for a video: https alone (a redirect to
/// http is refused too), and an HLS playlist's encrypted parts (`crypto`,
/// over https too). Not `httpproxy`, which a proxy would need: a playlist
/// could name an `httpproxy://` part, which FFmpeg reaches on its own,
/// around the proxy, in plain text (measured). With a proxy, no video plays
/// (`App::play_video`).
const PROTOCOLS: &str = "https,tls,tcp,crypto";

/// Something mpv said, for the playback numbered `play`.
#[derive(Debug)]
pub struct PlayerEvent {
    pub play: u64,
    pub kind: PlayerEventKind,
}

#[derive(Debug, PartialEq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub enum PlayerEventKind {
    Position(f64),
    Duration(f64),
    Paused(bool),
    /// mpv started playing, or went on after a seek: its first frame is
    /// there.
    Restarted,
    /// The video played to its end: mpv's `end-file` with reason `eof`.
    Ended,
    /// mpv couldn't play the video: `end-file` with reason `error`.
    Failed,
    /// mpv exited: the video ended, its window was closed, or tuinews
    /// stopped it.
    Exited,
}

pub struct Player {
    pub play: u64,
    /// tuinews can pause, seek and follow the playback: over mpv's socket,
    /// which Windows doesn't have yet. A field, not `cfg!(unix)` where it's
    /// asked, so tests check both ways on any system.
    controllable: bool,
    #[cfg(unix)]
    control: Option<UnboundedSender<String>>,
    /// Tells the task that owns mpv's process to end it.
    stop: Option<oneshot::Sender<()>>,
}

pub struct Start<'a> {
    /// mpv, or pepper-play, which takes mpv's options and socket
    /// (`pepper`).
    pub program: &'a Path,
    /// The video's address, checked (`html::video_url`).
    pub url: &'a str,
    /// The article's title, for mpv's window and Now Playing.
    pub title: &'a str,
    /// The picture plays inside Pepper, through pepper-play (`program`).
    pub pepper: Option<Pepper<'a>>,
}

/// What pepper-play needs to play into Pepper (`crate::pepper`). Pepper
/// tells it the box's size itself.
pub struct Pepper<'a> {
    /// Pepper's port, from `PEPPER_PORT`: pepper-play gets it in its
    /// environment, which otherwise has only `tools::player_env`'s.
    pub port: &'a OsStr,
    /// The token the picture's box is placed with: over the socket only.
    pub token: &'a str,
}

impl Player {
    pub fn start(
        play: u64,
        start: Start,
        tx: UnboundedSender<crate::app::AppEvent>,
    ) -> Result<Self> {
        let mut command = tokio::process::Command::new(start.program);
        command
            .args(args(&start))
            .env_clear()
            .envs(tools::player_env())
            .envs(start.pepper.as_ref().map(|p| ("PEPPER_PORT", p.port)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // If tuinews ends without stopping it, mpv goes too.
            .kill_on_drop(true);
        if let Some(home) = dirs::home_dir() {
            command.current_dir(home);
        }

        #[cfg(unix)]
        let (child, control) = {
            use std::os::fd::AsRawFd;
            let starting = tools::starting();
            let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
            let fd = theirs.as_raw_fd();
            // SAFETY: only dup2 or fcntl runs between fork and exec, both
            // async-signal-safe. dup2 also clears close-on-exec on fd 3.
            unsafe {
                command.pre_exec(move || {
                    let ok = if fd == 3 {
                        libc::fcntl(3, libc::F_SETFD, 0) != -1
                    } else {
                        libc::dup2(fd, 3) != -1
                    };
                    if !ok {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = command.spawn().with_context(|| cannot_start(&start))?;
            drop(starting);
            drop(theirs);
            ours.set_nonblocking(true)?;
            let stream = tokio::net::UnixStream::from_std(ours)?;
            let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
            spawn_ipc(play, stream, control_rx, tx.clone());
            for line in commands(&start) {
                let _ = control_tx.send(line);
            }
            (child, control_tx)
        };
        #[cfg(not(unix))]
        let child = {
            let _starting = tools::starting();
            command.spawn().with_context(|| cannot_start(&start))?
        };

        let (stop_tx, stop_rx) = oneshot::channel();
        tokio::spawn(own(play, child, stop_rx, tx));
        Ok(Self {
            play,
            controllable: cfg!(unix),
            #[cfg(unix)]
            control: Some(control),
            stop: Some(stop_tx),
        })
    }

    /// A player with a pretend mpv behind it, which takes what it's sent
    /// and says nothing, for tests of what's asked of it: controllable as
    /// on Unix, on every system.
    #[cfg(test)]
    pub fn pretend(play: u64) -> Self {
        let (stop, _) = oneshot::channel();
        Self {
            play,
            controllable: true,
            #[cfg(unix)]
            control: None,
            stop: Some(stop),
        }
    }

    /// Plays as on Unix (`true`) or as on Windows (`false`), whichever
    /// system runs the test.
    #[cfg(test)]
    pub fn set_controllable(&mut self, controllable: bool) {
        self.controllable = controllable;
    }

    /// Whether tuinews can pause it, seek and make it fill the screen (not
    /// on Windows yet).
    pub fn controllable(&self) -> bool {
        self.controllable
    }

    fn send(&self, command: serde_json::Value) {
        #[cfg(unix)]
        if let Some(control) = &self.control {
            let _ = control.send(line(command));
        }
        #[cfg(not(unix))]
        let _ = command;
    }

    pub fn toggle_pause(&self) {
        self.send(serde_json::json!(["cycle", "pause"]));
    }

    /// mpv's window fills the screen, or goes back.
    pub fn toggle_fullscreen(&self) {
        self.send(serde_json::json!(["cycle", "fullscreen"]));
    }

    pub fn seek(&self, seconds: i32) {
        self.send(serde_json::json!(["seek", seconds, "relative"]));
    }

    /// Stops playback: mpv is asked to quit, and killed if it hasn't
    /// within `QUIT_GRACE`.
    pub fn stop(&mut self) {
        self.send(serde_json::json!(["quit"]));
        #[cfg(unix)]
        {
            // Closing our end makes mpv quit even if it missed the command.
            self.control = None;
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Owns mpv's process: waits for it to end, or for `stop`, then gives it
/// `QUIT_GRACE` before killing it through its handle (never by process id,
/// which another program may have by then). Says when it's gone.
async fn own(
    play: u64,
    mut child: tokio::process::Child,
    stop: oneshot::Receiver<()>,
    tx: UnboundedSender<crate::app::AppEvent>,
) {
    tokio::select! {
        _ = child.wait() => {}
        // Asked to stop, or the Player is gone.
        _ = stop => {
            // Windows has no way to ask mpv to quit yet.
            let grace = if cfg!(unix) { QUIT_GRACE } else { Duration::ZERO };
            if tokio::time::timeout(grace, child.wait()).await.is_err() {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
    }
    let _ = tx.send(crate::app::AppEvent::Player(PlayerEvent {
        play,
        kind: PlayerEventKind::Exited,
    }));
}

fn cannot_start(start: &Start) -> String {
    let name = if start.pepper.is_some() {
        "pepper-play"
    } else {
        "mpv"
    };
    format!("cannot start {name}")
}

/// pepper-play's own message, as mpv passes any message to the program it
/// runs in: its arguments are strings.
#[cfg_attr(not(unix), allow(dead_code))]
fn video_token_command(token: &str) -> serde_json::Value {
    serde_json::json!(["script-message-to", "main", "pepper-token", token])
}

/// mpv's arguments: how it plays and what it may open, nothing about the
/// video on Unix; on Windows the title in options and the URL after `--`.
fn args(start: &Start) -> Vec<String> {
    let mut args: Vec<String> = [
        "--no-config",
        "--load-scripts=no",
        "--terminal=no",
        "--ytdl=no",
        "--tls-verify=yes",
        "--demuxer=lavf",
        // What the stream is opened with (certificates checked) goes to
        // what an HLS playlist opens too: mpv's default, measured to refuse
        // a part whose certificate doesn't check out, here so it stays so.
        "--demuxer-lavf-propagate-opts=yes",
        "--load-unsafe-playlists=no",
        "--keep-open=no",
        "--save-position-on-quit=no",
        "--resume-playback=no",
        "--hwdec=auto-safe",
        "--force-window=immediate",
    ]
    .map(String::from)
    .into();
    // The stream layer opens the address itself (and follows its
    // redirects); the demuxer, an HLS playlist's parts. A list's commas
    // would split it: in brackets, it's one value.
    for layer in ["stream", "demuxer"] {
        args.push(format!("--{layer}-lavf-o=protocol_whitelist=[{PROTOCOLS}]"));
    }
    // On macOS, mpv's CoreAudio output doesn't open for 48 kHz sound in
    // planar float, and mpv falls back to AVFoundation, which holds 2 s of
    // sound. The same float, interleaved, opens it (tuitube measured it).
    if cfg!(target_os = "macos") {
        args.push("--audio-format=float".into());
    }
    if cfg!(unix) {
        // Idle until the file comes over the socket, then quit after it.
        args.push("--idle=once".into());
        args.push("--input-ipc-client=fd://3".into());
        return args;
    }
    args.push("--idle=no".into());
    for (name, value) in settings(start) {
        args.push(format!("--{name}={value}"));
    }
    args.push("--".into());
    args.push(start.url.to_string());
    args
}

/// The options that describe this playback: its title, in mpv's window
/// and the system's Now Playing.
fn settings(start: &Start) -> Vec<(&'static str, String)> {
    let title = crate::article::one_line(start.title, 150);
    vec![
        ("force-media-title", title.clone()),
        // The window title expands `${…}` properties; `$$` is a plain `$`.
        ("title", format!("{} — tuinews", title.replace('$', "$$"))),
    ]
}

/// One command for mpv's control socket, as a line of JSON: strings are
/// JSON strings, so a title can't break out of its place.
#[cfg_attr(not(unix), allow(dead_code))]
fn line(command: serde_json::Value) -> String {
    format!("{}\n", serde_json::json!({ "command": command }))
}

/// What's sent over the socket to play the video: the settings (with
/// `set`, which takes the value as is), the properties tuinews follows, in
/// Pepper the token, then the video's URL.
#[cfg_attr(not(unix), allow(dead_code))]
fn commands(start: &Start) -> Vec<String> {
    let mut commands: Vec<String> = settings(start)
        .into_iter()
        .map(|(name, value)| line(serde_json::json!(["set", name, value])))
        .collect();
    commands.extend(
        [(1, "time-pos"), (2, "duration"), (3, "pause")]
            .into_iter()
            .map(|(id, property)| line(serde_json::json!(["observe_property", id, property]))),
    );
    // Before the video loads, so its first frame has somewhere to go.
    if let Some(pepper) = &start.pepper {
        commands.push(line(video_token_command(pepper.token)));
    }
    commands.push(line(serde_json::json!(["loadfile", start.url, "replace"])));
    commands
}

#[cfg(unix)]
fn spawn_ipc(
    play: u64,
    stream: tokio::net::UnixStream,
    mut control: tokio::sync::mpsc::UnboundedReceiver<String>,
    tx: UnboundedSender<crate::app::AppEvent>,
) {
    use tokio::io::AsyncWriteExt;
    let (read, mut write) = stream.into_split();
    tokio::spawn(async move {
        while let Some(line) = control.recv().await {
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
        // Dropping the writer closes our end: mpv quits.
    });
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(read);
        while let Some(line) = next_line(&mut reader).await {
            if let Some(kind) = parse_event(&line) {
                let _ = tx.send(crate::app::AppEvent::Player(PlayerEvent { play, kind }));
            }
        }
    });
}

/// The longest line of mpv's read: one longer is skipped.
#[cfg(any(unix, test))]
const MAX_LINE: u64 = 64 * 1024;

/// mpv's next line, as text, what isn't UTF-8 replaced (mpv passes a
/// stream's tags on as they are), so one such line doesn't end the rest;
/// `None` at the end. A line longer than [`MAX_LINE`] is read no further
/// than that at a time, and comes back empty.
#[cfg(any(unix, test))]
async fn next_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Option<String> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut line = Vec::new();
    let read = (&mut *reader)
        .take(MAX_LINE)
        .read_until(b'\n', &mut line)
        .await
        .ok()?;
    if read == 0 {
        return None;
    }
    if line.last() == Some(&b'\n') || (read as u64) < MAX_LINE {
        return Some(String::from_utf8_lossy(&line).into_owned());
    }
    // Too long: the rest of it, to its end, goes unread.
    loop {
        line.clear();
        let read = (&mut *reader)
            .take(MAX_LINE)
            .read_until(b'\n', &mut line)
            .await
            .ok()?;
        if read == 0 {
            return None;
        }
        if line.last() == Some(&b'\n') {
            return Some(String::new());
        }
    }
}

#[derive(serde::Deserialize)]
#[cfg(any(unix, test))]
struct IpcMessage {
    event: Option<String>,
    name: Option<String>,
    data: Option<serde_json::Value>,
    reason: Option<String>,
}

/// One of mpv's messages, if it's one tuinews follows.
#[cfg(any(unix, test))]
fn parse_event(line: &str) -> Option<PlayerEventKind> {
    let message: IpcMessage = serde_json::from_str(line).ok()?;
    match message.event.as_deref()? {
        "end-file" => match message.reason.as_deref() {
            Some("eof") => Some(PlayerEventKind::Ended),
            Some("error") => Some(PlayerEventKind::Failed),
            _ => None,
        },
        "playback-restart" => Some(PlayerEventKind::Restarted),
        "property-change" => {
            let data = message.data?;
            match message.name.as_deref()? {
                "time-pos" => data.as_f64().map(PlayerEventKind::Position),
                "duration" => data.as_f64().map(PlayerEventKind::Duration),
                "pause" => data.as_bool().map(PlayerEventKind::Paused),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start<'a>(url: &'a str, title: &'a str) -> Start<'a> {
        Start {
            program: Path::new("/opt/homebrew/bin/mpv"),
            url,
            title,
            pepper: None,
        }
    }

    const URL: &str = "https://d1.vnecdn.net/vnexpress/video/a/vne/master.m3u8";

    #[cfg(unix)]
    #[test]
    fn nothing_about_the_video_is_on_mpvs_command_line() {
        let args = args(&start(URL, "Một tin bí mật"));
        assert_eq!(args[0], "--no-config");
        for flag in [
            "--terminal=no",
            "--ytdl=no",
            "--tls-verify=yes",
            "--demuxer=lavf",
            "--load-unsafe-playlists=no",
            "--stream-lavf-o=protocol_whitelist=[https,tls,tcp,crypto]",
            "--demuxer-lavf-o=protocol_whitelist=[https,tls,tcp,crypto]",
            "--idle=once",
            "--input-ipc-client=fd://3",
        ] {
            assert!(args.contains(&flag.to_string()), "{flag}");
        }
        let all = args.join(" ");
        assert!(!all.contains("bí mật"), "{all}");
        assert!(!all.contains("vnecdn"), "{all}");
        assert!(
            !all.contains("httpproxy"),
            "a playlist could name a part that goes around a proxy"
        );
    }

    #[tokio::test]
    async fn a_line_mpv_sends_that_isnt_utf8_or_is_huge_ends_nothing() {
        let mut input = b"{\"event\":\"playback-restart\"}\n".to_vec();
        input.extend(b"{\"event\":\"x\",\"data\":\"\xff\xfe\"}\n");
        input.extend(std::iter::repeat_n(b'a', 200_000));
        input.extend(b"\n{\"event\":\"end-file\",\"reason\":\"eof\"}\n");
        let mut reader = tokio::io::BufReader::new(input.as_slice());
        let mut lines = Vec::new();
        while let Some(line) = next_line(&mut reader).await {
            lines.push(line);
        }
        let lengths: Vec<usize> = lines.iter().map(String::len).collect();
        assert_eq!(lines.len(), 4, "{lengths:?}");
        assert_eq!(parse_event(&lines[0]), Some(PlayerEventKind::Restarted));
        assert!(lines[1].contains('\u{fffd}'), "replaced, not the end");
        assert_eq!(lines[2], "", "too long: skipped");
        assert_eq!(parse_event(&lines[3]), Some(PlayerEventKind::Ended));
    }

    #[cfg(unix)]
    #[test]
    fn in_pepper_the_token_goes_over_the_socket_before_the_video() {
        let port = std::ffi::OsString::from("io.github.erictran308.pepper.1");
        let mut start = start(URL, "Một tin");
        start.pepper = Some(Pepper {
            port: &port,
            token: "abc123",
        });
        let commands = commands(&start);
        let at = |needle: &str| commands.iter().position(|c| c.contains(needle)).unwrap();
        assert!(commands[at("pepper-token")].contains("abc123"));
        assert!(at("pepper-token") < at("loadfile"));
        assert!(!args(&start).iter().any(|a| a.contains("abc123")));
        assert_eq!(cannot_start(&start), "cannot start pepper-play");
    }

    #[test]
    fn the_socket_gets_the_video_as_json_strings() {
        let title = "\"]}{\"command\":[\"run\",\"sh\"]} ${path} --script=x";
        let lines = commands(&start(URL, title));
        for line in &lines {
            let parsed: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
            assert!(parsed["command"].is_array(), "{line}");
            assert!(!line.trim_end().contains('\n'));
        }
        let set = |name: &str| {
            lines
                .iter()
                .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                .find(|v| v["command"][0] == "set" && v["command"][1] == name)
                .map(|v| v["command"][2].as_str().unwrap().to_string())
        };
        assert_eq!(set("force-media-title").unwrap(), title, "one string");
        assert!(set("title").unwrap().contains("$${path}"), "no expansion");
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["command"][0], "loadfile");
        assert_eq!(last["command"][1], URL);
        let followed = lines.iter().position(|l| l.contains("observe_property"));
        let last_set = lines.iter().rposition(|l| l.contains("\"set\""));
        assert!(last_set < followed);
    }

    #[test]
    fn mpvs_messages_are_read_leniently() {
        let event = |line: &str| parse_event(line);
        assert_eq!(
            event(r#"{"event":"property-change","id":1,"name":"time-pos","data":12.5}"#),
            Some(PlayerEventKind::Position(12.5))
        );
        assert_eq!(
            event(r#"{"event":"property-change","name":"pause","data":true}"#),
            Some(PlayerEventKind::Paused(true))
        );
        assert_eq!(
            event(r#"{"event":"end-file","reason":"eof"}"#),
            Some(PlayerEventKind::Ended)
        );
        assert_eq!(
            event(r#"{"event":"end-file","reason":"error","file_error":"x"}"#),
            Some(PlayerEventKind::Failed)
        );
        assert_eq!(
            event(r#"{"event":"playback-restart"}"#),
            Some(PlayerEventKind::Restarted)
        );
        assert_eq!(event(r#"{"event":"end-file","reason":"quit"}"#), None);
        assert_eq!(
            event(r#"{"event":"property-change","name":"time-pos"}"#),
            None
        );
        assert_eq!(event(r#"{"request_id":0,"error":"success"}"#), None);
        assert_eq!(event("not json"), None);
    }
}
