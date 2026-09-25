// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Errors surfaced to the host application.
//!
//! `Display` is written by hand rather than derived. Five variants do not justify
//! a procedural macro, and this crate is the one that will eventually be embedded
//! next to a media player — keeping its dependency graph at **zero** is worth
//! twenty lines of `match`.

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
    /// An external link belongs in a browser, and a torrent needs the streaming
    /// engine first — neither is a player failure.
    UnplayableSource {
        /// Why not, in terms a person can act on.
        reason: String,
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
