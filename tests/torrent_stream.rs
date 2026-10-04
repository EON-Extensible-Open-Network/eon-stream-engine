// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The torrent chain, end to end, against a real swarm.
//!
//! Every test here is `#[ignore]`d, and that is not laziness. The suite that
//! gates `main` has to be deterministic and offline (madde 1: a suite that goes
//! red because somebody else's server had a bad afternoon stops being
//! trusted). These need the internet and strangers who are still seeding, so
//! they are run deliberately:
//!
//! ```text
//! cargo test --test torrent_stream -- --ignored --nocapture
//! ```
//!
//! What they prove is the thing no unit test can: that a magnet link becomes
//! bytes arriving over HTTP with correct range semantics. The unit tests in
//! `src/server.rs` cover the guards and the parsing; this covers the chain.
//!
//! The torrent used is **Big Buck Bunny**, which is Creative Commons
//! Attribution and the standard test fixture for exactly this. Nothing in this
//! project ships with a content source (madde 9) — this is a test fixture,
//! named in a test file, and the application never comes with it.

use std::{
    io::{Read as _, Write as _},
    net::TcpStream,
    time::Duration,
};

use eon_stream_engine::{TorrentEngine, TorrentOptions, TorrentRequest};

/// Big Buck Bunny: Creative Commons Attribution 3.0, Blender Foundation.
const BIG_BUCK_BUNNY: &str = "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c\
    &dn=Big+Buck+Bunny\
    &tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce\
    &tr=udp%3A%2F%2Fopen.demonii.com%3A1337%2Fannounce\
    &tr=udp%3A%2F%2Ftracker.openbittorrent.com%3A6969%2Fannounce\
    &tr=udp%3A%2F%2Fexplodie.org%3A6969\
    &ws=https%3A%2F%2Fwebtorrent.io%2Ftorrents%2F";

fn scratch(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!("eon-stream-itest-{name}"));
    let _ = std::fs::remove_dir_all(&directory);
    directory
}

fn engine(name: &str, prebuffer: u64) -> TorrentEngine {
    TorrentEngine::start(TorrentOptions {
        download_dir: scratch(name),
        metadata_timeout: Duration::from_secs(90),
        prebuffer_bytes: prebuffer,
        ..TorrentOptions::default()
    })
    .expect("the engine starts")
}

/// One HTTP request against the loopback server, returning head and body.
fn http(url: &str, range: Option<&str>) -> (String, Vec<u8>) {
    // Split `http://127.0.0.1:PORT/rest` by hand: pulling in a client to talk
    // to our own socket would be testing the client.
    let after_scheme = url.strip_prefix("http://").expect("a loopback url");
    let (authority, path) = after_scheme.split_once('/').expect("a path");
    let path = format!("/{path}");

    let mut stream = TcpStream::connect(authority).expect("the server is listening");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("a read timeout is settable");
    let range_header = range.map_or_else(String::new, |r| format!("Range: {r}\r\n"));
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\n{range_header}Connection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .expect("the request is written");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("the response is read");

    // Split on the end of the header block.
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a header block");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    (head, raw[split + 4..].to_vec())
}

#[test]
#[ignore = "needs the internet and a live swarm"]
fn a_magnet_link_becomes_bytes_over_http() {
    // One megabyte, so the test is a minute rather than ten.
    let engine = engine("stream", 1024 * 1024);

    let handle = engine
        .add(&TorrentRequest::Magnet(BIG_BUCK_BUNNY.to_owned()))
        .expect("metadata arrives from the swarm");
    println!(
        "resolved {:?}: {} file(s), {} bytes",
        handle.name(),
        handle.files().len(),
        handle.total_bytes()
    );
    assert!(!handle.files().is_empty());

    let video = handle.best_video().expect("there is a video in it");
    println!("picked {} ({} bytes)", video.name(), video.length);
    assert!(video.name().to_ascii_lowercase().ends_with(".mp4"));
    let index = video.index;

    engine
        .wait_for_prebuffer(&handle, index, Duration::from_secs(300), |have, want| {
            println!("  buffering {have} / {want}");
            true
        })
        .expect("the prebuffer fills");

    let url = engine.stream_url(&handle, index).expect("a stream url");
    assert!(url.starts_with("http://127.0.0.1:"));
    assert!(url.to_ascii_lowercase().ends_with(".mp4"));

    // A fresh open: what mpv does first.
    let (head, body) = http(&url, Some("bytes=0-65535"));
    println!("{head}");
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert!(head.contains("Accept-Ranges: bytes"), "{head}");
    assert!(
        head.contains(&format!("/{}", video.length)),
        "Content-Range must carry the real length: {head}"
    );
    assert_eq!(
        body.len(),
        65_536,
        "a closed range returns exactly its size"
    );

    // An MP4 begins with an `ftyp` box, four bytes in. Checking it proves the
    // bytes are the file's own and in the right order, which a length check
    // alone would not.
    assert_eq!(&body[4..8], b"ftyp", "not the start of an MP4");

    // A seek: the part that exercises the piece window moving.
    let midpoint = video.length / 2;
    let (head, body) = http(
        &url,
        Some(&format!("bytes={midpoint}-{}", midpoint + 32_767)),
    );
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert!(
        head.contains(&format!("bytes {midpoint}-")),
        "the response must say which range it answered: {head}"
    );
    assert_eq!(body.len(), 32_768);

    // An unsatisfiable range is a 416 carrying the real length, so a client
    // with a stale idea of the size can correct itself.
    let (head, _) = http(&url, Some(&format!("bytes={}-", video.length + 1)));
    assert!(head.starts_with("HTTP/1.1 416"), "{head}");

    let progress = engine.progress(&handle).expect("progress is readable");
    println!(
        "downloaded {} of {}, {} peer(s)",
        progress.downloaded, progress.total, progress.peers
    );
    assert!(progress.downloaded > 0);

    engine.shutdown();
}

#[test]
#[ignore = "needs the internet and a live swarm"]
fn a_bare_info_hash_resolves_through_the_dht() {
    // The shape an addon's stream object gives: `infoHash` and no trackers.
    // Slower than a magnet with trackers, which is the point of testing it.
    let engine = engine("dht", 256 * 1024);
    let handle = engine
        .add(&TorrentRequest::InfoHash {
            hex: "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c".to_owned(),
            trackers: Vec::new(),
        })
        .expect("the DHT finds it");
    assert!(!handle.files().is_empty());
    engine.shutdown();
}

#[test]
#[ignore = "needs the internet and a live swarm"]
fn the_token_is_required_even_for_a_torrent_that_is_running() {
    // The guard tested against a real, resolvable torrent rather than an empty
    // session: a correct info hash with the wrong token must still be a 404.
    let engine = engine("token", 256 * 1024);
    let handle = engine
        .add(&TorrentRequest::Magnet(BIG_BUCK_BUNNY.to_owned()))
        .expect("metadata arrives");
    let video = handle.best_video().expect("a video");
    let url = engine
        .stream_url(&handle, video.index)
        .expect("a stream url");

    // Swap the token for a different one of the same shape.
    let address = engine.server_address().to_string();
    let tampered = format!(
        "http://{address}/{}/{}/{}/{}",
        "0".repeat(64),
        handle.info_hash(),
        video.index,
        "x.mp4"
    );
    let (head, _) = http(&tampered, None);
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");

    // And the real one still works.
    let (head, _) = http(&url, Some("bytes=0-1023"));
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");

    engine.shutdown();
}
