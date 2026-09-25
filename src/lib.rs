// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! # EON Stream stream
//!
//! Turns a stream source into a playing video.
//!
//! ```text
//! source (HTTP URL | infoHash + fileIdx | local file)
//!   -> sequential download, piece window ahead of the playhead
//!   -> local HTTP server on 127.0.0.1 with a per-session token
//!   -> mpv (separate process) over JSON IPC
//! ```
//!
//! ## Two invariants
//!
//! **No transcoding.** mpv plays what a browser-based player would need converted, so
//! the entire transcode/HLS layer that Stremio's closed streaming server provides is
//! simply absent here. That saved layer is what pays for mpv's integration cost
//! (madde 1).
//!
//! **mpv is a separate process in v1** (madde 2). Embedding libmpv means managing a
//! native video surface under the system webview and compositing the interface onto
//! it; that is deferred behind the `embedded-mpv` feature and is not implemented.
//!
//! ## Security posture
//!
//! Torrent peers and addon-supplied URLs are untrusted input. The local HTTP server
//! binds loopback only, requires an unguessable per-session token, serves no
//! directory listing, and cannot reach a path outside the active torrent's file set.
//!
//! Nothing here ships with a content source, and the application never comes with one
//! (madde 9).
//!
//! ## Before distributing
//!
//! Read `NOTICE`. mpv and `FFmpeg` are LGPL *or* GPL depending on build configuration,
//! and getting that wrong is the most likely licensing mistake in this project.

#![deny(unsafe_code)]

/// Torrent engine: sequential piece selection around the playhead, web seeds
/// (BEP 19), private-mode restrictions for institutional distribution.
pub mod torrent {}

/// Local HTTP server that feeds the player. Loopback only, token guarded,
/// range requests.
pub mod server {}

/// mpv control over JSON IPC: load, play, pause, seek, track selection,
/// position reporting. The `PlayerBackend` boundary is meant to accommodate a
/// second implementation for mobile (media3/ExoPlayer) without touching callers.
pub mod player {}

/// Resolving a stream source into something playable.
pub mod source {}

/// Errors surfaced to the host application.
pub mod error {}
