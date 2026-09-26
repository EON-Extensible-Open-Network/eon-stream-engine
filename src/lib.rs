// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! # EON Stream engine
//!
//! Turns a stream source into a playing video.
//!
//! ```text
//! source (HTTP URL | infoHash + fileIdx | local file)
//!   -> [planned] sequential download, piece window ahead of the playhead
//!   -> [planned] local HTTP server on 127.0.0.1 with a per-session token
//!   -> mpv (separate process) over JSON IPC        <-- implemented
//! ```
//!
//! ## What works today
//!
//! Playback of anything mpv can open directly: an HTTP(S) URL or a local file.
//!
//! ```no_run
//! use eon_stream_engine::{MpvPlayer, PlaybackSource, PlayerOptions};
//!
//! let source = PlaybackSource::Url("https://example.org/video.mp4".into());
//! let mut player = MpvPlayer::launch(&source, &PlayerOptions::default())?;
//!
//! if let Some(position) = player.position()? {
//!     println!("at {position:.1}s");
//! }
//! player.set_paused(true)?;
//! player.quit()?;
//! # Ok::<(), eon_stream_engine::Error>(())
//! ```
//!
//! ## Two invariants
//!
//! **No transcoding.** mpv plays what a browser-based player would need
//! converted, so the entire transcode/HLS layer that Stremio's closed streaming
//! server provides is simply absent here. That saved layer is what pays for
//! mpv's integration cost (madde 1).
//!
//! **mpv is a separate process.** Embedding libmpv means managing a native video
//! surface under the system webview and compositing the interface onto it; that
//! is deferred behind the `embedded-mpv` feature and is not implemented
//! (madde 2). The `PlayerBackend` boundary is meant to accommodate a second
//! implementation for mobile (media3/ExoPlayer) without touching callers.
//!
//! ## Security posture
//!
//! Torrent peers and addon-supplied URLs are untrusted input. mpv is launched
//! with `--no-config`, so a stray user configuration cannot change behaviour the
//! caller depends on. A stream URL can carry a signed token, so it is never
//! logged and never shown — [`PlaybackSource::display_hint`] returns only a file
//! name.
//!
//! Nothing here ships with a content source, and the application never comes with
//! one (madde 9).
//!
//! ## Before distributing
//!
//! Read `NOTICE`. mpv and FFmpeg are LGPL *or* GPL depending on build
//! configuration, and getting that wrong is the most likely licensing mistake in
//! this project.

#![deny(unsafe_code)]

pub mod error;
pub mod player;
pub(crate) mod track;

pub use error::{Error, Result};
pub use player::{MpvPlayer, PlaybackSource, PlayerOptions, SeekMode, Track};

/// Torrent engine: sequential piece selection around the playhead, web seeds
/// (BEP 19), private-mode restrictions for institutional distribution.
///
/// Not implemented. The engine choice (librqbit) is recorded in the plan under
/// madde 1; the work is the next slice after playback.
pub mod torrent {}

/// Local HTTP server that feeds the player. Loopback only, token guarded,
/// range requests.
///
/// Not implemented. Needed only once a source is a torrent rather than a URL
/// mpv can open by itself.
pub mod server {}
