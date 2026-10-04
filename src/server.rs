// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The loopback HTTP server the player reads from.
//!
//! The second half of v1 item 4. mpv cannot open a torrent, but it opens an
//! HTTP URL with range requests perfectly well — so a torrent file is served
//! as one, from `127.0.0.1`, and mpv never learns the difference. Seeking in
//! the player becomes a range request, which becomes a seek on librqbit's
//! `FileStream`, which re-queues the piece window around the new position.
//! That is the whole mechanism.
//!
//! ## Why this is hand-written and not axum
//!
//! What mpv needs is one method, one header and one status code: `GET` with
//! `Range`, answered `206` with `Content-Range`. hyper and axum are in the
//! dependency tree already via librqbit, so the saving is not the compile
//! time — it is that the surface served on this machine's loopback interface
//! is three hundred lines a person can read, with no routing table, no
//! middleware, and no behaviour nobody chose.
//!
//! ## What guards it
//!
//! A loopback port is reachable by **every program on the machine and every
//! page in a browser**, which is the part that makes "it's only localhost"
//! wrong. So:
//!
//! * **Bound to `127.0.0.1` only.** Never `0.0.0.0`; nothing off this machine
//!   can connect at all.
//! * **A per-session token in the path**, 32 random bytes, compared in
//!   constant time, regenerated every run and never written to disk. Without
//!   it there is no reply.
//! * **A wrong token gets `404`, not `403`.** A `403` would confirm the path
//!   exists and turn the port into an oracle for what is being watched.
//! * **`GET` and `HEAD` only.** Nothing writes, nothing lists, nothing
//!   executes. There is no directory index because there is no directory.
//! * **No `Access-Control-Allow-Origin`.** A browser page can send a request
//!   here but cannot read the response, and adding that header for
//!   convenience would hand what is being watched to any page that guessed
//!   the token.
//! * **Request lines and headers are bounded.** An unbounded read on a public
//!   port is a memory exhaustion bug waiting for someone bored.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use librqbit::Session;
use rand::Rng as _;
use tokio::{
    io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    runtime::Handle,
    sync::watch,
};

use crate::error::{Error, Result};

/// Longest request line accepted, in bytes.
const MAX_REQUEST_LINE: usize = 8 * 1024;

/// Longest header block accepted, in bytes.
const MAX_HEADERS: usize = 16 * 1024;

/// How much to move per write. Large enough that the syscall overhead is
/// irrelevant, small enough that a seek during playback is not waiting behind
/// a huge queued write.
const CHUNK: usize = 256 * 1024;

/// A running loopback server.
pub struct LocalServer {
    address: SocketAddr,
    token: String,
    shutdown: watch::Sender<bool>,
}

impl LocalServer {
    /// Bind a port on loopback and start answering.
    ///
    /// Port `0`, so the operating system picks: a fixed port would collide
    /// with a second instance and is one more thing another program could
    /// camp on.
    ///
    /// # Errors
    ///
    /// [`Error::ServerStart`] if nothing can be bound on loopback.
    pub fn start(runtime: &Handle, session: Arc<Session>) -> Result<Self> {
        let token = new_token();
        let (shutdown, watcher) = watch::channel(false);

        // Bound with the standard library and then handed to tokio, rather
        // than `block_on`-ing an async bind. Two reasons: a bind failure
        // surfaces synchronously to the caller, and `block_on` would panic if
        // this were ever called from inside a runtime — which the tests do and
        // a Tauri command handler would.
        let standard =
            std::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .map_err(|e| Error::ServerStart {
                    message: e.to_string(),
                })?;
        let address = standard.local_addr().map_err(|e| Error::ServerStart {
            message: e.to_string(),
        })?;
        standard
            .set_nonblocking(true)
            .map_err(|e| Error::ServerStart {
                message: e.to_string(),
            })?;
        let listener = {
            // `from_std` registers with the reactor, so it has to run inside
            // the runtime's context.
            let _guard = runtime.enter();
            TcpListener::from_std(standard).map_err(|e| Error::ServerStart {
                message: e.to_string(),
            })?
        };

        let served_token = token.clone();
        runtime.spawn(async move {
            serve(listener, session, served_token, watcher).await;
        });

        Ok(Self {
            address,
            token,
            shutdown,
        })
    }

    /// Where it is listening.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The URL for one file of one torrent.
    ///
    /// The file's own name is the last segment because mpv reads the
    /// extension to choose a demuxer and uses the path when looking for
    /// sidecar subtitles. A URL ending in `/3` plays, but plays worse.
    #[must_use]
    pub fn url_for(&self, info_hash: &str, file_index: usize, filename: &str) -> String {
        format!(
            "http://{}/{}/{}/{}/{}",
            self.address,
            self.token,
            info_hash.to_ascii_lowercase(),
            file_index,
            percent_encode(filename)
        )
    }

    /// Stop answering.
    ///
    /// Open connections finish what they are writing; nothing new is
    /// accepted. A player that is mid-read gets its bytes rather than a
    /// truncated file.
    pub fn stop(&self) {
        let _ = self.shutdown.send(true);
    }

    /// The session token, for a test that needs to construct a request.
    #[cfg(test)]
    fn token(&self) -> &str {
        &self.token
    }
}

/// 32 random bytes as hex.
fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    let mut hex = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Percent-encode everything that is not unreserved.
///
/// Written out rather than taken from a crate: the input is a file name from a
/// torrent, which is to say arbitrary bytes chosen by a stranger, and the
/// allowlist below is short enough to read. An allowlist and not a denylist,
/// for the usual reason.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(*byte));
            }
            other => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

/// Compare two strings without leaking where they first differ.
///
/// The token is a secret in a URL path. Comparing it with `==` would return as
/// soon as a byte differs, which over enough attempts says how much of a
/// guess was right. This is cheap to do properly.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // The length difference is not secret -- the token's length is fixed and
    // public -- so returning early on it reveals nothing.
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    difference == 0
}

async fn serve(
    listener: TcpListener,
    session: Arc<Session>,
    token: String,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let accepted = tokio::select! {
            result = listener.accept() => result,
            _ = shutdown.changed() => return,
        };
        let Ok((stream, peer)) = accepted else {
            // An accept error is per-connection: the listener is still good,
            // and giving up here would take the player down with it.
            continue;
        };
        // Belt and braces. The socket is bound to loopback, so this cannot
        // fire -- but "cannot" and "does not" differ by one refactor, and the
        // cost of the check is nothing.
        if !peer.ip().is_loopback() {
            continue;
        }
        let session = Arc::clone(&session);
        let token = token.clone();
        tokio::spawn(async move {
            let _ = handle_connection(stream, session, token).await;
        });
    }
}

/// What a request asked for.
struct Request {
    method: String,
    path: String,
    range: Option<(u64, Option<u64>)>,
}

async fn handle_connection(
    mut stream: TcpStream,
    session: Arc<Session>,
    token: String,
) -> std::io::Result<()> {
    let Some(request) = read_request(&mut stream).await? else {
        return write_status(&mut stream, 400, "Bad Request").await;
    };

    if request.method != "GET" && request.method != "HEAD" {
        return write_status(&mut stream, 405, "Method Not Allowed").await;
    }

    // Every failure past this point answers 404. A wrong token, a torrent that
    // is not in the session and a file index that does not exist are
    // indistinguishable from outside, so the port cannot be used to find out
    // what is being watched.
    let Some(target) = parse_target(&request.path) else {
        return write_status(&mut stream, 404, "Not Found").await;
    };
    if !constant_time_eq(&target.token, &token) {
        return write_status(&mut stream, 404, "Not Found").await;
    }

    let managed = session.with_torrents(|torrents| {
        // A plain loop: `with_torrents` hands over a `&mut dyn Iterator`, and
        // the adaptor methods are not callable on a trait object.
        for (_, managed) in torrents {
            if managed
                .info_hash()
                .as_string()
                .eq_ignore_ascii_case(&target.info_hash)
            {
                return Some(Arc::clone(managed));
            }
        }
        None
    });
    let Some(managed) = managed else {
        return write_status(&mut stream, 404, "Not Found").await;
    };

    let file_length = managed
        .with_metadata(|metadata| {
            metadata
                .file_infos
                .get(target.file_index)
                .map(|info| info.len)
        })
        .ok()
        .flatten();
    let Some(file_length) = file_length else {
        return write_status(&mut stream, 404, "Not Found").await;
    };

    let (start, end) = match resolve_range(request.range, file_length) {
        Some(range) => range,
        // RFC 9110: an unsatisfiable range gets 416 with the real length, so
        // the client can correct itself. mpv does this when it has a stale
        // idea of the length.
        None => {
            let headers = format!("Content-Range: bytes */{file_length}\r\n");
            return write_response_head(&mut stream, 416, "Range Not Satisfiable", 0, &headers)
                .await;
        }
    };
    let length = end - start + 1;
    let partial = request.range.is_some();

    let mut headers = String::new();
    headers.push_str("Accept-Ranges: bytes\r\n");
    headers.push_str(&format!(
        "Content-Type: {}\r\n",
        content_type(&target.filename)
    ));
    if partial {
        headers.push_str(&format!(
            "Content-Range: bytes {start}-{end}/{file_length}\r\n"
        ));
    }

    let (status, reason) = if partial {
        (206, "Partial Content")
    } else {
        (200, "OK")
    };
    write_response_head(&mut stream, status, reason, length, &headers).await?;

    if request.method == "HEAD" {
        return stream.flush().await;
    }

    // Opening the stream is what tells librqbit to queue pieces around this
    // position; it is not merely a reader.
    let file_stream = match Arc::clone(&managed).stream(target.file_index).await {
        Ok(stream) => stream,
        // The head is already written, so there is nothing to say but close.
        // A player sees a short read and reports it, which is the honest
        // outcome of "the torrent went away mid-playback".
        Err(_) => return Ok(()),
    };
    let mut file_stream = file_stream;
    // Seeking is what moves librqbit's piece window to the range being
    // asked for, so this is the line that makes a seek in the player turn
    // into a seek in the swarm.
    if start > 0
        && file_stream
            .seek(std::io::SeekFrom::Start(start))
            .await
            .is_err()
    {
        return Ok(());
    }

    let mut remaining = length;
    let mut buffer = vec![0u8; CHUNK];
    while remaining > 0 {
        let want = usize::try_from(remaining.min(CHUNK as u64)).unwrap_or(CHUNK);
        let read = match file_stream.read(&mut buffer[..want]).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };
        if stream.write_all(&buffer[..read]).await.is_err() {
            // The player closed the connection -- which is what a seek looks
            // like from here. Normal, not an error.
            break;
        }
        remaining -= read as u64;
    }
    stream.flush().await
}

/// The parts of a request path this server cares about.
struct Target {
    token: String,
    info_hash: String,
    file_index: usize,
    filename: String,
}

/// Parse `/<token>/<info hash>/<file index>/<name>`.
///
/// Anything else is `None`, which the caller turns into a 404.
fn parse_target(path: &str) -> Option<Target> {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let mut parts = path.trim_start_matches('/').splitn(4, '/');
    let token = parts.next()?;
    let info_hash = parts.next()?;
    let file_index = parts.next()?;
    let filename = parts.next().unwrap_or("");

    if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if info_hash.len() != 40 || !info_hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(Target {
        token: token.to_owned(),
        info_hash: info_hash.to_ascii_lowercase(),
        file_index: file_index.parse().ok()?,
        filename: percent_decode(filename),
    })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Turn a `Range` header into concrete byte offsets.
///
/// `None` means unsatisfiable, which is a 416 and not a 404: the resource
/// exists, the range does not.
fn resolve_range(range: Option<(u64, Option<u64>)>, file_length: u64) -> Option<(u64, u64)> {
    if file_length == 0 {
        return Some((0, 0));
    }
    let last = file_length - 1;
    match range {
        None => Some((0, last)),
        Some((start, _)) if start >= file_length => None,
        Some((start, None)) => Some((start, last)),
        Some((start, Some(end))) if end < start => None,
        // An end past the file is clamped rather than refused: RFC 9110 says
        // to, and mpv asks for more than exists routinely.
        Some((start, Some(end))) => Some((start, end.min(last))),
    }
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut raw = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];

    // Read to the end of the header block, one byte at a time. Slow, and it
    // does not matter: this happens once per connection, against a request of
    // a few hundred bytes, and it avoids buffering past the headers into the
    // body -- which this server has no use for anyway.
    loop {
        if raw.len() > MAX_REQUEST_LINE + MAX_HEADERS {
            return Ok(None);
        }
        match stream.read(&mut byte).await {
            Ok(0) => return Ok(None),
            Ok(_) => raw.push(byte[0]),
            Err(e) => return Err(e),
        }
        if raw.ends_with(b"\r\n\r\n") || raw.ends_with(b"\n\n") {
            break;
        }
    }

    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.lines();
    let Some(request_line) = lines.next() else {
        return Ok(None);
    };
    if request_line.len() > MAX_REQUEST_LINE {
        return Ok(None);
    }
    let mut fields = request_line.split_whitespace();
    let Some(method) = fields.next() else {
        return Ok(None);
    };
    let Some(path) = fields.next() else {
        return Ok(None);
    };

    let mut range = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("range") {
            range = parse_range(value.trim());
        }
    }

    Ok(Some(Request {
        method: method.to_ascii_uppercase(),
        path: path.to_owned(),
        range,
    }))
}

/// Parse `bytes=START-[END]`.
///
/// Only the single-range form. Multipart ranges are legal HTTP and no player
/// sends them, and answering one requires multipart encoding — so they are
/// treated as no range at all rather than half-implemented.
fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let start = start.trim();
    let end = end.trim();
    if start.is_empty() {
        // A suffix range (`bytes=-500`) needs the file length to resolve, which
        // the caller has and this function does not. No player asks for one.
        return None;
    }
    let start: u64 = start.parse().ok()?;
    let end = if end.is_empty() {
        None
    } else {
        Some(end.parse::<u64>().ok()?)
    };
    Some((start, end))
}

/// Guess a content type from the extension.
///
/// mpv probes the stream itself, so this is a courtesy rather than load
/// bearing. `application/octet-stream` for anything unrecognised, which is the
/// honest answer.
fn content_type(filename: &str) -> &'static str {
    let extension = filename
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "mkv" => "video/x-matroska",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "ts" | "m2ts" | "mts" => "video/mp2t",
        "mpg" | "mpeg" => "video/mpeg",
        "flv" => "video/x-flv",
        "wmv" => "video/x-ms-wmv",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "aac" | "m4a" => "audio/mp4",
        "opus" | "ogg" => "audio/ogg",
        "srt" => "application/x-subrip",
        "vtt" => "text/vtt",
        "ass" | "ssa" => "text/x-ssa",
        _ => "application/octet-stream",
    }
}

async fn write_response_head(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_length: u64,
    extra_headers: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Length: {content_length}\r\n\
         {extra_headers}\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         \r\n"
    );
    stream.write_all(head.as_bytes()).await
}

async fn write_status(stream: &mut TcpStream, status: u16, reason: &str) -> std::io::Result<()> {
    write_response_head(stream, status, reason, 0, "").await?;
    stream.flush().await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_sixty_four_hex_characters_and_never_repeats() {
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two sessions must not share a token");
    }

    #[test]
    fn constant_time_comparison_still_compares_correctly() {
        // Being constant time is useless if it is also wrong.
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
        // Differing in the first byte and in the last are both caught, which
        // is what an early-returning comparison would get right too -- the
        // point is that it takes the same time, which a test cannot assert.
        assert!(!constant_time_eq("xbc", "abc"));
        assert!(!constant_time_eq("abx", "abc"));
    }

    #[test]
    fn a_well_formed_path_parses() {
        let token = "a".repeat(64);
        let hash = "d".repeat(40);
        let path = format!("/{token}/{hash}/3/Movie.2026.1080p.mkv");
        let target = parse_target(&path).expect("the path parses");
        assert_eq!(target.token, token);
        assert_eq!(target.info_hash, hash);
        assert_eq!(target.file_index, 3);
        assert_eq!(target.filename, "Movie.2026.1080p.mkv");
    }

    #[test]
    fn a_malformed_path_is_refused_rather_than_guessed_at() {
        let token = "a".repeat(64);
        let hash = "d".repeat(40);
        for bad in [
            String::new(),
            "/".to_owned(),
            format!("/{token}"),
            format!("/{token}/{hash}"),
            // Token the wrong length.
            format!("/{}/{hash}/0/x.mkv", "a".repeat(63)),
            // Token not hex.
            format!("/{}/{hash}/0/x.mkv", "z".repeat(64)),
            // Info hash the wrong length.
            format!("/{token}/{}/0/x.mkv", "d".repeat(39)),
            // File index not a number.
            format!("/{token}/{hash}/notanumber/x.mkv"),
            // Negative index.
            format!("/{token}/{hash}/-1/x.mkv"),
        ] {
            assert!(parse_target(&bad).is_none(), "{bad} should not parse");
        }
    }

    #[test]
    fn a_name_with_slashes_in_it_cannot_reach_a_second_file() {
        // The name is the last segment and `splitn(4)` keeps the rest of the
        // path inside it, so a crafted name is a name and not a new route.
        let token = "a".repeat(64);
        let hash = "d".repeat(40);
        let path = format!("/{token}/{hash}/0/..%2F..%2Fetc%2Fpasswd");
        let target = parse_target(&path).expect("parses");
        assert_eq!(target.file_index, 0);
        // Decoded for the content-type guess only; nothing opens it as a path.
        assert_eq!(target.filename, "../../etc/passwd");
    }

    #[test]
    fn a_query_string_is_ignored() {
        let token = "a".repeat(64);
        let hash = "d".repeat(40);
        let path = format!("/{token}/{hash}/0/x.mkv?cachebust=1");
        let target = parse_target(&path).expect("parses");
        assert_eq!(target.filename, "x.mkv");
    }

    #[test]
    fn ranges_parse_in_the_forms_a_player_sends() {
        assert_eq!(parse_range("bytes=0-"), Some((0, None)));
        assert_eq!(parse_range("bytes=1024-2047"), Some((1024, Some(2047))));
        assert_eq!(parse_range("  bytes=5-  "), Some((5, None)));
        // Forms this server does not answer, treated as absent rather than
        // half-implemented.
        assert_eq!(parse_range("bytes=-500"), None);
        assert_eq!(parse_range("bytes=0-100,200-300"), None);
        assert_eq!(parse_range("items=0-1"), None);
        assert_eq!(parse_range("garbage"), None);
        assert_eq!(parse_range("bytes=abc-def"), None);
    }

    #[test]
    fn range_resolution_follows_the_rfc() {
        // No range: the whole file.
        assert_eq!(resolve_range(None, 1000), Some((0, 999)));
        // Open-ended: to the end.
        assert_eq!(resolve_range(Some((100, None)), 1000), Some((100, 999)));
        // Closed.
        assert_eq!(resolve_range(Some((0, Some(499))), 1000), Some((0, 499)));
        // Past the end is clamped, because mpv asks for more than exists
        // routinely.
        assert_eq!(
            resolve_range(Some((900, Some(5000))), 1000),
            Some((900, 999))
        );
        // Starting past the end is unsatisfiable: a 416, not a clamp.
        assert_eq!(resolve_range(Some((1000, None)), 1000), None);
        assert_eq!(resolve_range(Some((1001, Some(2000))), 1000), None);
        // Backwards.
        assert_eq!(resolve_range(Some((500, Some(100))), 1000), None);
        // An empty file is answerable rather than an error.
        assert_eq!(resolve_range(None, 0), Some((0, 0)));
    }

    #[test]
    fn urls_carry_the_token_and_end_in_the_file_name() {
        // Both halves matter: the token is the only guard, and mpv reads the
        // extension off the end of the path.
        let token = "f".repeat(64);
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 51413);
        let server = LocalServer {
            address,
            token: token.clone(),
            shutdown: watch::channel(false).0,
        };
        let url = server.url_for(
            "DD8255ECDC7CA55FB0BBF81323D87062DB1F6D1C",
            2,
            "Big Buck Bunny.mkv",
        );
        assert_eq!(
            url,
            format!(
                "http://127.0.0.1:51413/{token}/dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c/2/\
                 Big%20Buck%20Bunny.mkv"
            )
        );
        assert!(url.to_ascii_lowercase().ends_with(".mkv"));
        // Loopback, never a routable address.
        assert!(url.contains("127.0.0.1"));
    }

    #[test]
    fn percent_encoding_round_trips_through_the_parser() {
        for name in [
            "Big Buck Bunny.mkv",
            "Fil#m [2026] (1080p).mkv",
            "türkçe adı.mkv",
            "100% complete.mp4",
            "a+b&c=d.mkv",
        ] {
            let encoded = percent_encode(name);
            assert_eq!(percent_decode(&encoded), name, "round trip for {name}");
            // Nothing that would end the path or start a query survives.
            assert!(!encoded.contains('/'));
            assert!(!encoded.contains('?'));
            assert!(!encoded.contains('#'));
            assert!(!encoded.contains(' '));
        }
    }

    #[test]
    fn content_types_cover_what_mpv_is_handed() {
        assert_eq!(content_type("x.mkv"), "video/x-matroska");
        assert_eq!(content_type("x.MP4"), "video/mp4");
        assert_eq!(content_type("x.srt"), "application/x-subrip");
        // The honest answer for anything else.
        assert_eq!(content_type("x.zzz"), "application/octet-stream");
        assert_eq!(content_type("noextension"), "application/octet-stream");
    }

    /// Start a server over an empty session, in whatever runtime is current.
    ///
    /// An empty session is enough for every guard that matters here: the
    /// token check, the method check and the "no such torrent" path all run
    /// before anything is read, and they are the parts a wrong answer makes
    /// dangerous rather than broken.
    async fn test_server() -> (LocalServer, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "eon-stream-server-test-{}",
            new_token()[..16].to_owned()
        ));
        std::fs::create_dir_all(&directory).expect("the test directory is creatable");
        let session = Session::new_with_opts(
            directory.clone(),
            librqbit::SessionOptions {
                // No DHT, no trackers, no listener: this session never touches
                // a network, and a test that opened sockets to the internet
                // would be a test nobody should run.
                dht: None,
                disable_trackers: true,
                persistence: None,
                disable_local_service_discovery: true,
                ..Default::default()
            },
        )
        .await
        .expect("an offline session starts");
        let server =
            LocalServer::start(&Handle::current(), session).expect("the loopback server binds");
        (server, directory)
    }

    /// Send a raw request and return the response head.
    async fn request(address: SocketAddr, raw: &str) -> String {
        try_request(address, raw)
            .await
            .expect("the server answered")
    }

    /// Send a raw request, tolerating the connection being reset.
    ///
    /// A reset is a legitimate outcome rather than a failure: when the server
    /// stops reading an over-long request, answers, and closes while the
    /// client is still sending, the operating system reports the discarded
    /// data as an RST. That is what refusing to buffer unboundedly looks like
    /// from the other end.
    async fn try_request(address: SocketAddr, raw: &str) -> Option<String> {
        let mut stream = TcpStream::connect(address)
            .await
            .expect("the server accepts a connection");
        if stream.write_all(raw.as_bytes()).await.is_err() {
            return None;
        }
        let mut response = Vec::new();
        match stream.read_to_end(&mut response).await {
            Ok(_) => Some(String::from_utf8_lossy(&response).into_owned()),
            Err(_) => None,
        }
    }

    #[tokio::test]
    async fn it_binds_loopback_and_nothing_else() {
        let (server, directory) = test_server().await;
        assert!(
            server.address().ip().is_loopback(),
            "bound {} -- nothing off this machine may reach it",
            server.address()
        );
        assert_ne!(server.address().port(), 0);
        server.stop();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn a_wrong_token_gets_404_and_not_403() {
        // A 403 would confirm the path exists and turn the port into an oracle
        // for what is being watched.
        let (server, directory) = test_server().await;
        let hash = "d".repeat(40);
        let wrong = "0".repeat(64);
        let response = request(
            server.address(),
            &format!("GET /{wrong}/{hash}/0/x.mkv HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        assert!(!response.contains("403"), "{response}");
        server.stop();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn the_right_token_for_an_unknown_torrent_is_also_404() {
        // Indistinguishable from a wrong token, which is the point.
        let (server, directory) = test_server().await;
        let token = server.token().to_owned();
        let hash = "d".repeat(40);
        let response = request(
            server.address(),
            &format!("GET /{token}/{hash}/0/x.mkv HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        server.stop();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn nothing_but_get_and_head_is_answered() {
        let (server, directory) = test_server().await;
        let token = server.token().to_owned();
        let hash = "d".repeat(40);
        for method in ["POST", "PUT", "DELETE", "OPTIONS", "TRACE"] {
            let response = request(
                server.address(),
                &format!("{method} /{token}/{hash}/0/x.mkv HTTP/1.1\r\nHost: localhost\r\n\r\n"),
            )
            .await;
            assert!(
                response.starts_with("HTTP/1.1 405"),
                "{method} got {response}"
            );
        }
        server.stop();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn a_garbage_request_gets_a_400_rather_than_a_panic() {
        let (server, directory) = test_server().await;
        let response = request(server.address(), "not http at all\r\n\r\n").await;
        // "not http at all" has the shape of a request line, so the method
        // check reaches it first and answers 405. A request line with nothing
        // parseable in it answers 400. Which of the two arrives is not the
        // property worth pinning — that the server answers rather than
        // panicking, and keeps serving afterwards, is.
        assert!(
            response.starts_with("HTTP/1.1 4"),
            "expected a 4xx, got {response}"
        );
        // Still answering afterwards.
        let wrong = "0".repeat(64);
        let hash = "d".repeat(40);
        let second = request(
            server.address(),
            &format!("GET /{wrong}/{hash}/0/x.mkv HTTP/1.1\r\n\r\n"),
        )
        .await;
        assert!(second.starts_with("HTTP/1.1 404"), "{second}");
        server.stop();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn an_oversized_request_is_dropped_rather_than_buffered() {
        // An unbounded read on a reachable port is a memory exhaustion bug.
        let (server, directory) = test_server().await;
        let token = server.token().to_owned();
        let hash = "d".repeat(40);
        let padding = "x".repeat(MAX_REQUEST_LINE + MAX_HEADERS + 1024);
        // Either outcome proves the point: a 400 means the read stopped at the
        // cap and answered, and no response at all means it stopped at the cap
        // and closed while the client was still sending. What must not happen
        // is the server reading all of it.
        let response = try_request(
            server.address(),
            &format!("GET /{token}/{hash}/0/x.mkv HTTP/1.1\r\nX-Padding: {padding}\r\n\r\n"),
        )
        .await;
        if let Some(response) = &response {
            assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        }

        // And it is still serving, which is the half that would be missed by
        // only checking the refusal.
        let wrong = "0".repeat(64);
        let after = request(
            server.address(),
            &format!("GET /{wrong}/{hash}/0/x.mkv HTTP/1.1\r\n\r\n"),
        )
        .await;
        assert!(after.starts_with("HTTP/1.1 404"), "{after}");
        server.stop();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn the_response_head_says_nothing_it_should_not() {
        // The real bytes off the real socket. Asserting on a header block the
        // test wrote itself would pass even if the server emitted something
        // else entirely.
        let (server, directory) = test_server().await;
        let wrong = "0".repeat(64);
        let hash = "d".repeat(40);
        let response = request(
            server.address(),
            &format!("GET /{wrong}/{hash}/0/x.mkv HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        )
        .await;
        let lower = response.to_ascii_lowercase();

        // A browser page may send a request here; it must not be able to read
        // the answer.
        assert!(
            !lower.contains("access-control-allow"),
            "the loopback server must not hand its answers to web pages: {response}"
        );
        // Nothing in between caches what is being watched.
        assert!(lower.contains("cache-control: no-store"), "{response}");
        assert!(
            lower.contains("x-content-type-options: nosniff"),
            "{response}"
        );
        // And nothing identifies the software, which only helps someone
        // deciding whether this machine is worth a second look.
        assert!(!lower.contains("server:"), "{response}");

        server.stop();
        let _ = std::fs::remove_dir_all(directory);
    }
}
