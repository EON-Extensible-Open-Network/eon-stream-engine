# eon-stream-engine

Streaming and playback: **BitTorrent engine, sequential download, local HTTP server, mpv
control over IPC.**

[![Status](https://img.shields.io/badge/status-Faz%200%20·%20active%20·%20highest%20risk-dc2626)](https://github.com/EON-Extensible-Open-Network/eon-docs/blob/main/plan/eon-plan.md)
[![License](https://img.shields.io/badge/license-GPL--3.0--or--later%20+%20module%20exception-3b82f6)](LICENSE)

---

## What this crate does

Turns "here is a URL or an info hash" into "mpv is playing it".

```
stream source (HTTP URL | infoHash + fileIdx | local file)
        |
   sequential download, piece window ahead of the playhead
        |
   local HTTP server  ->  127.0.0.1:<port>/<token>
        |
   mpv (separate process)  <--  JSON IPC  -->  this crate
```

Four responsibilities:

1. **Torrent engine** — sequential piece selection, prioritised around the playhead, with
   web seed (BEP 19) support so a package survives its publisher going offline (madde 15d).
2. **Local HTTP server** — range requests, one unguessable token per session, bound to
   loopback only.
3. **Player control** — mpv over JSON IPC: load, play, pause, seek, track selection,
   position reporting.
4. **No transcoding, ever.** See below.

## Two design decisions worth reading

### mpv runs as a separate process in v1

Embedding libmpv in-process is not the hard part. The hard part is managing a native video
surface (a child HWND on Windows, an NSView on macOS, X11/Wayland on Linux) underneath or
above the system webview, and compositing the interface onto it. Z-order and transparency
there are a known ordeal.

So v1 runs mpv as a **separate process** over JSON IPC: controls in the EON Stream window, video
in the mpv window. Low risk, quick to working. Embedding is attempted afterwards as its own
slice; if it fails, v1 behaviour stands (madde 2).

It also weakens the licensing coupling to mpv — see [`NOTICE`](NOTICE).

### No transcoding

Stremio's streaming server is closed source and does more than torrenting: it also
transcodes and serves HLS. EON Stream does not need that layer at all, because mpv plays
essentially everything a transcoder would have converted for a browser.

Sequential download plus a local HTTP endpoint is the whole pipeline. **That saved layer is
what pays for mpv's integration cost** (madde 1) — the reason the risk is worth taking.

## Engine choice

**librqbit** (Apache-2.0, actively maintained, usable as a library) is the leading
candidate, decided in Faz 0.

## Read NOTICE before distributing

[`NOTICE`](NOTICE) is not boilerplate here. mpv and FFmpeg are LGPL **or** GPL depending on
build configuration, and a GPL-configured build requires the distributed binary to be
GPL-compatible. This is the most likely licensing mistake in the whole project, which is why
the exact configure flags of any bundled build must be recorded before a release ships.

## Build

```bash
cargo test
cargo clippy -- -D warnings
cargo deny check
```

Integration tests that actually play need mpv on `PATH`. Unit tests do not.

## Security notes

The local HTTP server is an attack surface on the user's machine: loopback bind only,
one unguessable token per session, no directory listing, no path outside the active
torrent's file set. Torrent peers and addon-supplied URLs are untrusted input
([SECURITY.md](https://github.com/EON-Extensible-Open-Network/.github/blob/main/SECURITY.md)).

**Nothing in this crate ships with a content source, and the application never comes with
one** (madde 9).

## License

`GPL-3.0-or-later` **WITH** the EON Module ABI Exception 1.0 — [`LICENSE`](LICENSE),
[`LICENSE-EXCEPTION.md`](LICENSE-EXCEPTION.md).

## Related

[eon-stream-core](https://github.com/EON-Extensible-Open-Network/eon-stream-core) ·
[eon-stream-app](https://github.com/EON-Extensible-Open-Network/eon-stream-app) ·
[eon-stream-spec](https://github.com/EON-Extensible-Open-Network/eon-stream-spec) ·
[eon-docs](https://github.com/EON-Extensible-Open-Network/eon-docs)
