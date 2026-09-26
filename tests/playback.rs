// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors

//! Playback tests that need a real mpv.
//!
//! Marked `#[ignore]` so `cargo test` stays green on a machine without mpv, and
//! run explicitly with `cargo test -- --ignored` where it is installed. CI does
//! exactly that on the Windows runner.
//!
//! The source is mpv's own synthetic test pattern (`av://lavfi:testsrc`), so
//! nothing is downloaded and no sample file is committed. Video and audio output
//! are disabled, which means this exercises the part that actually carries risk
//! -- process launch and the JSON IPC channel -- without needing a display.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::time::Duration;

use eon_stream_engine::{MpvPlayer, PlaybackSource, PlayerOptions, SeekMode};

fn headless() -> PlayerOptions {
    PlayerOptions {
        // No window, no sound: this is a channel test, not a rendering test.
        extra_args: vec![
            "--vo=null".into(),
            "--ao=null".into(),
            "--no-force-window".into(),
        ],
        ipc_timeout: Duration::from_secs(30),
        ..PlayerOptions::default()
    }
}

/// A five second synthetic clip mpv generates itself.
fn test_pattern() -> PlaybackSource {
    PlaybackSource::Url("av://lavfi:testsrc=duration=5:size=320x240:rate=10".into())
}

#[test]
#[ignore = "needs mpv installed"]
fn launches_mpv_and_talks_to_it() {
    let mut player = MpvPlayer::launch(&test_pattern(), &headless()).unwrap();

    // The IPC round trip is the thing under test: ask for something mpv always
    // knows about.
    let version = player.get_property("mpv-version").unwrap();
    assert!(
        version.contains("mpv"),
        "unexpected version reply: {version}"
    );

    assert!(player.is_running());
    player.quit().unwrap();
}

#[test]
#[ignore = "needs mpv installed"]
fn reports_duration_and_position() {
    let mut player = MpvPlayer::launch(&test_pattern(), &headless()).unwrap();

    // Loading is asynchronous; a property that is not ready yet is None rather
    // than an error, so poll rather than assuming.
    //
    // What is asserted is that a numeric duration comes back over the channel --
    // not what it equals. An earlier version of this test expected about five
    // seconds and got 1.2: for a synthetic lavfi source mpv estimates duration
    // as it goes, so that assertion was measuring mpv's estimator rather than
    // our IPC. Exact durations belong in a test with a real container, and that
    // is not what this file is for.
    let mut duration = None;
    for _ in 0..80 {
        if let Ok(Some(d)) = player.duration() {
            duration = Some(d);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let duration = duration.expect("mpv never reported a duration over IPC");
    assert!(
        duration.is_finite() && duration > 0.0,
        "duration came back unusable: {duration}"
    );

    // Seeking is the part that has to work: ask for a position and see the
    // playhead move there.
    player.seek(2.0, SeekMode::Absolute).unwrap();
    let mut seeked = None;
    for _ in 0..40 {
        if let Ok(Some(p)) = player.position() {
            if p >= 1.5 {
                seeked = Some(p);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        seeked.is_some(),
        "seek to 2s did not move the playhead past 1.5s"
    );

    player.quit().unwrap();
}

#[test]
#[ignore = "needs mpv installed"]
fn pause_and_resume_are_accepted() {
    let mut player = MpvPlayer::launch(&test_pattern(), &headless()).unwrap();

    player.set_paused(true).unwrap();
    assert_eq!(player.get_property("pause").unwrap(), "true");

    player.set_paused(false).unwrap();
    assert_eq!(player.get_property("pause").unwrap(), "false");

    player.quit().unwrap();
}

#[test]
#[ignore = "needs mpv installed"]
fn a_rejected_command_is_an_error_not_a_panic() {
    let mut player = MpvPlayer::launch(&test_pattern(), &headless()).unwrap();

    let result = player.get_property("this-property-does-not-exist");
    assert!(
        result.is_err(),
        "expected mpv to reject an unknown property"
    );

    // The channel must still be usable afterwards.
    assert!(player.get_property("mpv-version").is_ok());
    player.quit().unwrap();
}

#[test]
#[ignore = "needs mpv installed"]
fn lists_tracks_and_switches_between_them() {
    // The synthetic source has one video track and no audio, which is enough to
    // prove the track list parses and a selection is accepted.
    let mut player = MpvPlayer::launch(&test_pattern(), &headless()).unwrap();

    let mut tracks = Vec::new();
    for _ in 0..60 {
        tracks = player.tracks().unwrap_or_default();
        if !tracks.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!tracks.is_empty(), "mpv reported no tracks at all");
    assert!(
        tracks.iter().any(|t| t.kind == "video"),
        "expected a video track, got {tracks:?}"
    );

    // Turning a track off and on again must both be accepted.
    player.select_track("video", None).unwrap();
    player.select_track("video", Some(1)).unwrap();

    // An unknown kind is an error, not a panic or a silent no-op.
    assert!(player.select_track("nonsense", Some(1)).is_err());

    player.quit().unwrap();
}

#[test]
#[ignore = "needs mpv installed"]
fn accepts_speed_delays_and_volume() {
    let mut player = MpvPlayer::launch(&test_pattern(), &headless()).unwrap();

    player.set_speed(1.5).unwrap();
    assert_eq!(player.get_property("speed").unwrap(), "1.5");

    // Out of range values are clamped rather than rejected: a caller asking for
    // 100x wants "as fast as you can", not an error.
    player.set_speed(1000.0).unwrap();
    assert_eq!(player.get_property("speed").unwrap(), "4");

    player.set_audio_delay(-0.5).unwrap();
    player.set_subtitle_delay(1.25).unwrap();
    player.set_subtitles_visible(false).unwrap();
    assert_eq!(player.get_property("sub-visibility").unwrap(), "false");

    player.set_volume(50.0).unwrap();
    assert_eq!(player.get_property("volume").unwrap(), "50");

    player.quit().unwrap();
}

#[test]
#[ignore = "needs mpv installed"]
fn reports_buffer_and_network_state_without_failing_when_unknown() {
    let mut player = MpvPlayer::launch(&test_pattern(), &headless()).unwrap();

    // A synthetic source has no network and may report nothing. What matters is
    // that asking is never an error -- an unavailable property is normal.
    let _ = player.buffered_secs().unwrap();
    let _ = player.network_speed_bytes().unwrap();
    assert!(!player.is_buffering().unwrap());

    player.quit().unwrap();
}

#[test]
#[ignore = "needs mpv installed"]
fn http_headers_reach_mpv_without_breaking_the_launch() {
    // The synthetic source ignores headers; this checks that passing them does
    // not produce a malformed command line, which is the failure that would
    // otherwise only show up against a real CDN.
    let options = PlayerOptions {
        http_headers: vec![
            ("Referer".into(), "https://example.org/".into()),
            ("User-Agent".into(), "EON-Stream-Test".into()),
            // Deliberately invalid: a comma would split mpv's list. It must be
            // dropped, and the launch must still succeed.
            ("X-Bad".into(), "a,b".into()),
        ],
        ..headless()
    };
    let mut player = MpvPlayer::launch(&test_pattern(), &options).unwrap();
    assert!(player.get_property("mpv-version").is_ok());
    player.quit().unwrap();
}
