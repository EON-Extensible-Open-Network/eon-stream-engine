// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Driving mpv as a separate process over JSON IPC.
//!
//! **Why a separate process** (madde 2): embedding libmpv is not the hard part.
//! The hard part is managing a native video surface — a child HWND on Windows,
//! an NSView on macOS, X11/Wayland on Linux — underneath the system webview and
//! compositing the interface onto it. Z-order and transparency there are a known
//! ordeal, and getting it wrong on one platform blocks the release on all of
//! them.
//!
//! So mpv gets its own window and we talk to it. Low risk, and it keeps the
//! licensing coupling weak too (see `NOTICE`). Embedding is a later, independent
//! slice; if it never lands, this behaviour stands.
//!
//! The channel is mpv's `--input-ipc-server`: newline-delimited JSON, a named
//! pipe on Windows and a Unix socket elsewhere. No extra dependency for either.

use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use crate::{Error, Result};

/// What to hand to the player.
#[derive(Debug, Clone)]
pub enum PlaybackSource {
    /// A URL mpv can open directly.
    Url(String),
    /// A file on this machine.
    File(PathBuf),
}

impl PlaybackSource {
    /// The argument mpv is given.
    fn as_argument(&self) -> String {
        match self {
            Self::Url(url) => url.clone(),
            Self::File(path) => path.to_string_lossy().into_owned(),
        }
    }

    /// Something short enough to show a person, with no credentials in it.
    ///
    /// A stream URL can carry a signed token, so only the last path segment is
    /// shown — the same discipline addon addresses get.
    #[must_use]
    pub fn display_hint(&self) -> String {
        match self {
            Self::Url(url) => url
                .split('?')
                .next()
                .and_then(|u| u.rsplit('/').next())
                .filter(|s| !s.is_empty())
                .unwrap_or("stream")
                .to_owned(),
            Self::File(path) => path
                .file_name()
                .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned()),
        }
    }
}

/// How to launch mpv.
#[derive(Debug, Clone)]
pub struct PlayerOptions {
    /// Path to the mpv executable. `None` means look it up on `PATH` and in the
    /// usual install locations.
    pub mpv_path: Option<PathBuf>,
    /// Start paused, so the caller can seek before the first frame.
    pub start_paused: bool,
    /// Open fullscreen.
    pub fullscreen: bool,
    /// Window title mpv shows.
    pub title: Option<String>,
    /// Subtitle files to load alongside the source.
    pub subtitle_urls: Vec<String>,
    /// Extra arguments, appended last.
    pub extra_args: Vec<String>,
    /// How long to wait for mpv to open its IPC channel.
    pub ipc_timeout: Duration,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        Self {
            mpv_path: None,
            start_paused: false,
            fullscreen: false,
            title: None,
            subtitle_urls: Vec::new(),
            extra_args: Vec::new(),
            ipc_timeout: Duration::from_secs(10),
        }
    }
}

/// Where to seek from.
#[derive(Debug, Clone, Copy)]
pub enum SeekMode {
    /// Relative to the current position.
    Relative,
    /// From the start of the file.
    Absolute,
}

impl SeekMode {
    fn as_flag(self) -> &'static str {
        match self {
            Self::Relative => "relative",
            Self::Absolute => "absolute",
        }
    }
}

/// A running mpv process.
///
/// Dropping this does **not** kill mpv: a person who opened a video should keep
/// watching it even if the thing that launched it goes away. Call
/// [`MpvPlayer::quit`] to close it deliberately.
#[derive(Debug)]
pub struct MpvPlayer {
    child: Child,
    ipc: ipc::Connection,
    next_request_id: u64,
}

impl MpvPlayer {
    /// Launch mpv on a source and connect to its IPC channel.
    ///
    /// # Errors
    ///
    /// [`Error::MpvNotFound`] when no mpv executable can be located,
    /// [`Error::PlayerLaunch`] when the process will not start, and
    /// [`Error::PlayerIpc`] when it starts but never opens the channel.
    pub fn launch(source: &PlaybackSource, options: &PlayerOptions) -> Result<Self> {
        let mpv = resolve_mpv(options.mpv_path.as_deref())?;
        let endpoint = ipc::Endpoint::new();

        let mut command = Command::new(&mpv);
        command
            .arg(format!("--input-ipc-server={}", endpoint.mpv_argument()))
            // Keep mpv alive if the source fails, so its own error is visible
            // rather than a window that vanishes.
            .arg("--idle=once")
            .arg("--force-window=yes")
            // Never let mpv read a user config that could change behaviour we
            // depend on, and never let it write one.
            .arg("--no-config")
            .arg("--msg-level=all=warn")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        if options.start_paused {
            command.arg("--pause");
        }
        if options.fullscreen {
            command.arg("--fullscreen");
        }
        if let Some(title) = &options.title {
            command.arg(format!("--title={title}"));
        }
        for subtitle in &options.subtitle_urls {
            command.arg(format!("--sub-file={subtitle}"));
        }
        for extra in &options.extra_args {
            command.arg(extra);
        }
        command.arg("--").arg(source.as_argument());

        let child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::MpvNotFound
            } else {
                Error::PlayerLaunch {
                    message: e.to_string(),
                }
            }
        })?;

        let ipc = endpoint.connect(options.ipc_timeout)?;
        Ok(Self {
            child,
            ipc,
            next_request_id: 1,
        })
    }

    /// Send a command and wait for its reply.
    ///
    /// # Errors
    ///
    /// [`Error::PlayerIpc`] on a channel failure, and [`Error::PlayerCommand`]
    /// when mpv rejects the command.
    pub fn command(&mut self, args: &[&str]) -> Result<String> {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);

        let mut request = String::from("{\"command\":[");
        for (index, arg) in args.iter().enumerate() {
            if index > 0 {
                request.push(',');
            }
            json::write_string(&mut request, arg);
        }
        request.push_str("],\"request_id\":");
        request.push_str(&id.to_string());
        request.push('}');

        self.ipc.send_line(&request)?;
        self.ipc.await_reply(id)
    }

    /// Read a property.
    ///
    /// # Errors
    ///
    /// As [`Self::command`].
    pub fn get_property(&mut self, name: &str) -> Result<String> {
        self.command(&["get_property", name])
    }

    /// Write a property.
    ///
    /// # Errors
    ///
    /// As [`Self::command`].
    pub fn set_property(&mut self, name: &str, value: &str) -> Result<()> {
        self.command(&["set_property", name, value]).map(|_| ())
    }

    /// Pause or resume.
    ///
    /// # Errors
    ///
    /// As [`Self::command`].
    pub fn set_paused(&mut self, paused: bool) -> Result<()> {
        self.set_property("pause", if paused { "yes" } else { "no" })
    }

    /// Seek.
    ///
    /// # Errors
    ///
    /// As [`Self::command`].
    pub fn seek(&mut self, seconds: f64, mode: SeekMode) -> Result<()> {
        self.command(&["seek", &seconds.to_string(), mode.as_flag()])
            .map(|_| ())
    }

    /// Current position in seconds, if playback has started.
    ///
    /// # Errors
    ///
    /// [`Error::PlayerIpc`] on a channel failure. A property that is simply not
    /// available yet is `Ok(None)`, not an error — it is the normal state while
    /// a file is still loading.
    pub fn position(&mut self) -> Result<Option<f64>> {
        Ok(self
            .get_property("time-pos")
            .ok()
            .and_then(|v| v.parse().ok()))
    }

    /// Total duration in seconds, when mpv knows it.
    ///
    /// # Errors
    ///
    /// As [`Self::position`].
    pub fn duration(&mut self) -> Result<Option<f64>> {
        Ok(self
            .get_property("duration")
            .ok()
            .and_then(|v| v.parse().ok()))
    }

    /// Whether mpv is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Ask mpv to close, then wait for it.
    ///
    /// # Errors
    ///
    /// [`Error::PlayerLaunch`] if the process cannot be waited on.
    pub fn quit(mut self) -> Result<()> {
        let _ = self.command(&["quit"]);
        // Give it a moment to go on its own before insisting.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        self.child
            .wait()
            .map(|_| ())
            .map_err(|e| Error::PlayerLaunch {
                message: e.to_string(),
            })
    }

    /// Block until mpv exits, returning whatever it wrote to stderr.
    ///
    /// # Errors
    ///
    /// [`Error::PlayerLaunch`] if the process cannot be waited on.
    pub fn wait(mut self) -> Result<String> {
        let stderr = self.child.stderr.take();
        self.child.wait().map_err(|e| Error::PlayerLaunch {
            message: e.to_string(),
        })?;
        let Some(stderr) = stderr else {
            return Ok(String::new());
        };
        let mut output = String::new();
        for line in BufReader::new(stderr)
            .lines()
            .map_while(std::result::Result::ok)
        {
            output.push_str(&line);
            output.push('\n');
        }
        Ok(output)
    }
}

/// Find an mpv executable.
///
/// # Errors
///
/// [`Error::MpvNotFound`] when nothing is found.
fn resolve_mpv(explicit: Option<&std::path::Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok(path.to_path_buf())
        } else {
            Err(Error::MpvNotFound)
        };
    }

    // An installer that does not touch PATH is the common case on Windows, so
    // looking only at PATH would report "not installed" to someone who just
    // installed it.
    const CANDIDATES: &[&str] = &[
        r"C:\Program Files\MPV Player\mpv.exe",
        r"C:\Program Files\mpv\mpv.exe",
        r"C:\Program Files (x86)\mpv\mpv.exe",
        "/usr/bin/mpv",
        "/usr/local/bin/mpv",
        "/opt/homebrew/bin/mpv",
    ];
    for candidate in CANDIDATES {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }

    // Fall back to letting the OS search PATH.
    let probe = if cfg!(windows) { "mpv.exe" } else { "mpv" };
    if Command::new(probe)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
    {
        return Ok(PathBuf::from(probe));
    }
    Err(Error::MpvNotFound)
}

/// Just enough JSON writing for the commands we send.
mod json {
    /// Append a JSON string literal, escaped.
    pub(super) fn write_string(out: &mut String, value: &str) {
        out.push('"');
        for ch in value.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }
}

/// Platform plumbing for mpv's IPC channel.
mod ipc {
    use std::{
        io::{BufRead, BufReader, Write},
        time::{Duration, Instant},
    };

    use crate::{Error, Result};

    /// A name mpv will listen on, and we will connect to.
    pub(super) struct Endpoint {
        name: String,
    }

    impl Endpoint {
        pub(super) fn new() -> Self {
            // Unique per launch: two players must not share a channel.
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let pid = std::process::id();
            let name = if cfg!(windows) {
                format!(r"\\.\pipe\eon-stream-mpv-{pid}-{stamp}")
            } else {
                std::env::temp_dir()
                    .join(format!("eon-stream-mpv-{pid}-{stamp}.sock"))
                    .to_string_lossy()
                    .into_owned()
            };
            Self { name }
        }

        pub(super) fn mpv_argument(&self) -> &str {
            &self.name
        }

        /// Wait for mpv to create the channel, then connect.
        pub(super) fn connect(&self, timeout: Duration) -> Result<Connection> {
            let deadline = Instant::now() + timeout;
            let mut last = String::from("timed out");
            while Instant::now() < deadline {
                match Connection::open(&self.name) {
                    Ok(connection) => return Ok(connection),
                    Err(e) => last = e,
                }
                std::thread::sleep(Duration::from_millis(60));
            }
            Err(Error::PlayerIpc {
                message: format!("mpv did not open its control channel: {last}"),
            })
        }
    }

    /// An open connection to mpv.
    #[derive(Debug)]
    pub(super) struct Connection {
        #[cfg(windows)]
        write: std::fs::File,
        #[cfg(windows)]
        read: BufReader<std::fs::File>,
        #[cfg(unix)]
        write: std::os::unix::net::UnixStream,
        #[cfg(unix)]
        read: BufReader<std::os::unix::net::UnixStream>,
    }

    impl Connection {
        #[cfg(windows)]
        fn open(name: &str) -> std::result::Result<Self, String> {
            use std::fs::OpenOptions;
            let write = OpenOptions::new()
                .read(true)
                .write(true)
                .open(name)
                .map_err(|e| e.to_string())?;
            let read = write.try_clone().map_err(|e| e.to_string())?;
            Ok(Self {
                write,
                read: BufReader::new(read),
            })
        }

        #[cfg(unix)]
        fn open(name: &str) -> std::result::Result<Self, String> {
            use std::os::unix::net::UnixStream;
            let write = UnixStream::connect(name).map_err(|e| e.to_string())?;
            write
                .set_read_timeout(Some(Duration::from_secs(5)))
                .map_err(|e| e.to_string())?;
            let read = write.try_clone().map_err(|e| e.to_string())?;
            Ok(Self {
                write,
                read: BufReader::new(read),
            })
        }

        pub(super) fn send_line(&mut self, line: &str) -> Result<()> {
            writeln!(self.write, "{line}").map_err(|e| Error::PlayerIpc {
                message: e.to_string(),
            })?;
            self.write.flush().map_err(|e| Error::PlayerIpc {
                message: e.to_string(),
            })
        }

        /// Read until the reply carrying `request_id` arrives.
        ///
        /// mpv interleaves asynchronous events with replies, so everything that
        /// is not our reply is skipped rather than treated as a protocol error.
        pub(super) fn await_reply(&mut self, request_id: u64) -> Result<String> {
            let needle = format!("\"request_id\":{request_id}");
            // Bound the loop: a channel that goes quiet must not hang a caller.
            for _ in 0..512 {
                let mut line = String::new();
                let read = self
                    .read
                    .read_line(&mut line)
                    .map_err(|e| Error::PlayerIpc {
                        message: e.to_string(),
                    })?;
                if read == 0 {
                    return Err(Error::PlayerIpc {
                        message: "mpv closed the control channel".into(),
                    });
                }
                if !line.contains(&needle) {
                    continue; // an event, or another request's reply
                }
                if !line.contains("\"error\":\"success\"") {
                    return Err(Error::PlayerCommand {
                        message: extract_field(&line, "error")
                            .unwrap_or_else(|| line.trim().into()),
                    });
                }
                return Ok(extract_field(&line, "data").unwrap_or_default());
            }
            Err(Error::PlayerIpc {
                message: "no reply from mpv".into(),
            })
        }
    }

    /// Pull one top-level field out of an mpv reply.
    ///
    /// mpv's replies are flat and small, so a full parser would be more
    /// dependency than the job needs. Strings are unquoted; everything else is
    /// returned verbatim, which is what the numeric callers want.
    fn extract_field(line: &str, field: &str) -> Option<String> {
        let key = format!("\"{field}\":");
        let start = line.find(&key)? + key.len();
        let rest = line.get(start..)?.trim_start();
        if let Some(inner) = rest.strip_prefix('"') {
            let end = inner.find('"')?;
            return Some(inner.get(..end)?.to_owned());
        }
        let end = rest
            .find([',', '}'])
            .unwrap_or_else(|| rest.trim_end().len());
        Some(rest.get(..end)?.trim().to_owned())
    }

    #[cfg(test)]
    mod tests {
        use super::extract_field;

        #[test]
        fn reads_a_string_field() {
            let line = r#"{"data":"Big Buck Bunny","request_id":3,"error":"success"}"#;
            assert_eq!(
                extract_field(line, "data").as_deref(),
                Some("Big Buck Bunny")
            );
            assert_eq!(extract_field(line, "error").as_deref(), Some("success"));
        }

        #[test]
        fn reads_a_numeric_field() {
            let line = r#"{"data":12.5,"request_id":1,"error":"success"}"#;
            assert_eq!(extract_field(line, "data").as_deref(), Some("12.5"));
        }

        #[test]
        fn reads_a_field_at_the_end() {
            let line = r#"{"request_id":1,"error":"success","data":42}"#;
            assert_eq!(extract_field(line, "data").as_deref(), Some("42"));
        }

        #[test]
        fn absent_field_is_none() {
            let line = r#"{"event":"file-loaded"}"#;
            assert_eq!(extract_field(line, "data"), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PlaybackSource, SeekMode};
    use std::path::PathBuf;

    #[test]
    fn a_display_hint_never_shows_a_query_string() {
        // Stream URLs carry signed tokens; the hint is for a person, not a log.
        let source =
            PlaybackSource::Url("https://cdn.example.org/path/movie.mkv?token=SECRET&e=123".into());
        let hint = source.display_hint();
        assert_eq!(hint, "movie.mkv");
        assert!(!hint.contains("SECRET"));
    }

    #[test]
    fn a_file_hint_is_its_name() {
        let source = PlaybackSource::File(PathBuf::from(r"C:\videos\holiday.mp4"));
        assert!(source.display_hint().ends_with("holiday.mp4"));
    }

    #[test]
    fn seek_modes_map_to_mpv_flags() {
        assert_eq!(SeekMode::Relative.as_flag(), "relative");
        assert_eq!(SeekMode::Absolute.as_flag(), "absolute");
    }
}
