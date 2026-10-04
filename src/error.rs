// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Errors surfaced to the host application.
//!
//! `Display` is written by hand rather than derived. The variant count does not
//! justify a procedural macro, and the messages are the ones a person reads when
//! something goes wrong — writing them out keeps them readable as prose instead
//! of as attributes.
//!
//! This crate's dependency graph was zero until the torrent slice; it is now
//! librqbit, tokio and `rand`, each justified where it is declared. The hand-written
//! `Display` stays for the reason above, not for the dependency count.

use std::fmt;

/// The result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Anything that can go wrong moving bytes or driving the player.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// No mpv executable could be found.
    ///
    /// The message is deliberately actionable: "not found" without a next step is
    /// a dead end for whoever hits it.
    MpvNotFound,

    /// The player process would not start.
    PlayerLaunch {
        /// What the operating system reported.
        message: String,
    },

    /// The control channel failed.
    PlayerIpc {
        /// What went wrong on the channel.
        message: String,
    },

    /// mpv rejected a command.
    PlayerCommand {
        /// The error mpv reported.
        message: String,
    },

    /// The source is not something the player can be handed.
    ///
    /// An external link belongs in a browser — that is not a player failure.
    UnplayableSource {
        /// Why not, in terms a person can act on.
        reason: String,
    },

    // ------------------------------------------------------- torrents
    /// The torrent session would not start.
    TorrentEngine {
        /// What went wrong.
        message: String,
    },

    /// What was handed in is not a magnet link or an info hash.
    InvalidTorrentSource {
        /// Why not.
        reason: String,
    },

    /// The swarm did not produce the torrent's metadata in time.
    ///
    /// Distinct from a transport error on purpose: the usual cause is that
    /// nobody is sharing the torrent any more, and "it timed out" sends people
    /// to check their firewall instead.
    MetadataTimeout {
        /// How long was waited, in seconds.
        seconds: u64,
    },

    /// No torrent in this session carries that info hash.
    TorrentNotFound {
        /// The info hash that was asked for, hex.
        info_hash: String,
    },

    /// The torrent carries no file at that index.
    FileIndexOutOfRange {
        /// The index that was asked for.
        index: usize,
        /// How many files the torrent has.
        count: usize,
    },

    /// The torrent carries nothing that looks like video.
    NoVideoInTorrent {
        /// How many files it does carry, so the caller can offer the list.
        count: usize,
    },

    /// Reading from the torrent failed.
    TorrentIo {
        /// What went wrong.
        message: String,
    },

    /// The loopback server would not start.
    ServerStart {
        /// What the operating system reported.
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MpvNotFound => f.write_str(
                "mpv was not found. Install it (winget install shinchiro.mpv on \
                 Windows, or your package manager elsewhere), or set the mpv path \
                 explicitly",
            ),
            Self::PlayerLaunch { message } => write!(f, "could not start mpv: {message}"),
            Self::PlayerIpc { message } => write!(f, "lost contact with mpv: {message}"),
            Self::PlayerCommand { message } => write!(f, "mpv rejected the command: {message}"),
            Self::UnplayableSource { reason } => {
                write!(f, "this source cannot be played directly: {reason}")
            }
            Self::TorrentEngine { message } => {
                write!(f, "the torrent engine could not start: {message}")
            }
            Self::InvalidTorrentSource { reason } => {
                write!(f, "not a usable torrent source: {reason}")
            }
            Self::MetadataTimeout { seconds } => write!(
                f,
                "no torrent metadata after {seconds}s. Either nobody in the swarm \
                 answered -- which usually means the torrent has no seeders rather \
                 than that anything here is wrong -- or this was a bare info hash \
                 with no trackers, which has to find peers through the DHT. The DHT \
                 deliberately keeps nothing between runs, so that path is slowest on \
                 a fresh start; a magnet link with trackers in it does not wait for \
                 the DHT at all"
            ),
            Self::TorrentNotFound { info_hash } => {
                write!(f, "no torrent in this session has info hash {info_hash}")
            }
            Self::FileIndexOutOfRange { index, count } => write!(
                f,
                "this torrent has {count} file(s), so there is no file {index}"
            ),
            Self::NoVideoInTorrent { count } => write!(
                f,
                "none of the {count} file(s) in this torrent looks like video; \
                 choose one by index"
            ),
            Self::TorrentIo { message } => {
                write!(f, "could not read from the torrent: {message}")
            }
            Self::ServerStart { message } => write!(
                f,
                "could not start the loopback server the player reads from: {message}"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn mpv_not_found_tells_you_what_to_do() {
        let rendered = Error::MpvNotFound.to_string();
        assert!(rendered.contains("winget"), "not actionable: {rendered}");
    }

    #[test]
    fn messages_carry_their_detail() {
        let rendered = Error::PlayerIpc {
            message: "pipe closed".into(),
        }
        .to_string();
        assert!(rendered.contains("pipe closed"));
    }
}
