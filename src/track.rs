// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Reading mpv's track list.
//!
//! mpv replies with one flat array of flat objects, so it is scanned rather than
//! parsed. The reasoning is the same as for the error type: a procedural macro
//! and a JSON parser are more dependency than this job needs, and this crate is
//! the one that keeps an empty dependency graph.
//!
//! Anything unrecognised is skipped. A track we cannot describe is better
//! omitted from a list than guessed at and shown wrongly.

use crate::player::Track;

/// Pull the tracks out of a `track-list` reply line.
#[must_use]
pub(crate) fn parse_track_list(line: &str) -> Vec<Track> {
    let mut tracks = Vec::new();
    let Some(start) = line.find("\"data\":[") else {
        return tracks;
    };
    let Some(body) = line.get(start + "\"data\":[".len()..) else {
        return tracks;
    };

    // The objects are flat, so a closing brace always ends one.
    for chunk in body.split('{').skip(1) {
        let object = chunk.split('}').next().unwrap_or_default();
        let Some(id) = number_field(object, "id").and_then(|v| v.parse::<f64>().ok()) else {
            continue;
        };
        let Some(kind) = string_field(object, "type") else {
            continue;
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        tracks.push(Track {
            id: id as u32,
            kind,
            language: string_field(object, "lang"),
            title: string_field(object, "title"),
            selected: object.contains("\"selected\":true"),
        });
    }
    tracks
}

/// A quoted string field of a flat JSON object.
fn string_field(object: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\":\"");
    let start = object.find(&key)? + key.len();
    let rest = object.get(start..)?;
    let end = rest.find('"')?;
    Some(rest.get(..end)?.to_owned())
}

/// A bare numeric field of a flat JSON object.
fn number_field(object: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\":");
    let start = object.find(&key)? + key.len();
    let rest = object.get(start..)?.trim_start();
    if rest.starts_with('"') {
        return None;
    }
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest.get(..end)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape taken from a real mpv reply.
    const REPLY: &str = r#"{"data":[{"id":1,"type":"video","selected":true,"codec":"h264"},{"id":1,"type":"audio","lang":"eng","title":"Stereo","selected":true},{"id":2,"type":"audio","lang":"tur","selected":false},{"id":1,"type":"sub","lang":"tur","external":true,"selected":false}],"request_id":7,"error":"success"}"#;

    #[test]
    fn reads_every_track_with_its_kind_and_language() {
        let tracks = parse_track_list(REPLY);
        assert_eq!(tracks.len(), 4);

        assert_eq!(tracks[0].kind, "video");
        assert!(tracks[0].selected);
        assert_eq!(tracks[0].language, None);

        assert_eq!(tracks[1].kind, "audio");
        assert_eq!(tracks[1].language.as_deref(), Some("eng"));
        assert_eq!(tracks[1].title.as_deref(), Some("Stereo"));
        assert!(tracks[1].selected);

        assert_eq!(tracks[2].language.as_deref(), Some("tur"));
        assert!(!tracks[2].selected);

        assert_eq!(tracks[3].kind, "sub");
    }

    #[test]
    fn ids_repeat_across_kinds_and_that_is_fine() {
        // mpv numbers each kind from 1, so audio 1 and sub 1 both exist.
        let tracks = parse_track_list(REPLY);
        let audio: Vec<u32> = tracks
            .iter()
            .filter(|t| t.kind == "audio")
            .map(|t| t.id)
            .collect();
        assert_eq!(audio, [1, 2]);
    }

    #[test]
    fn an_empty_list_is_empty_not_an_error() {
        assert!(parse_track_list(r#"{"data":[],"request_id":1,"error":"success"}"#).is_empty());
    }

    #[test]
    fn a_reply_without_data_yields_nothing() {
        assert!(parse_track_list(r#"{"event":"file-loaded"}"#).is_empty());
    }

    #[test]
    fn a_track_missing_its_kind_is_skipped_not_guessed() {
        let line =
            r#"{"data":[{"id":1,"codec":"h264"},{"id":2,"type":"audio"}],"error":"success"}"#;
        let tracks = parse_track_list(line);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].kind, "audio");
    }

    #[test]
    fn display_lines_mark_the_selected_track() {
        let tracks = parse_track_list(REPLY);
        assert!(tracks[1].display_line().starts_with('*'));
        assert!(tracks[2].display_line().starts_with(' '));
        assert!(tracks[1].display_line().contains("Stereo"));
    }
}
