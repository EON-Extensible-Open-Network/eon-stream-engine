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

## Engine choice: librqbit, decided

**librqbit** (Apache-2.0, actively maintained, usable as a library) is the dependency. It
was the leading candidate and madde 1 is now closed.

The deciding factor was not BitTorrent in general. It was the single behaviour this project
actually needed: `ManagedTorrent::stream` hands back a reader that keeps a window of pieces
queued **ahead of wherever the stream has been read to**, and re-queues that window around
a seek. Nothing here reimplements piece selection; what this crate adds is a synchronous
surface over it, a prebuffer the player will not starve on, and progress a person can read.
Reaching that behaviour by hand would have meant DHT, uTP, web seeds and tracker protocols
first, to arrive at the same place.

It is configured `default-features = false, features = ["rust-tls"]`: rustls rather than
native TLS, so a Windows build needs no OpenSSL and the pinned toolchain is the whole
toolchain (madde 22).

## The loopback server, and why "it's only localhost" is wrong

A loopback port is reachable by **every program on the machine and every page in a
browser**. So the server mpv reads from is deliberately small and deliberately unhelpful to
anything else:

- Bound to `127.0.0.1`, never `0.0.0.0`.
- A **per-session 32-byte token** in the path, compared in constant time, regenerated every
  run and never written to disk.
- A wrong token gets **`404`, not `403`** — a `403` would confirm the path exists and turn
  the port into an oracle for what is being watched.
- `GET` and `HEAD` only. Nothing writes, nothing lists, no directory index, because there is
  no directory.
- **No `Access-Control-Allow-Origin`.** A browser page can send a request here but cannot
  read the answer, and adding that header for convenience would hand what is being watched
  to any page that guessed the token.
- Request lines and headers are bounded: an unbounded read on a reachable port is a
  memory-exhaustion bug waiting for someone bored.

It is hand-written rather than axum. hyper is already in the tree through librqbit, so the
saving is not compile time — it is that the surface served on this machine's loopback
interface is a few hundred lines a person can read, with no routing table, no middleware,
and no behaviour nobody chose.

## Testing the chain for real

The gating suite is offline and deterministic: a suite that goes red because strangers
stopped seeding stops being trusted, and an untrusted suite is worse than none (madde 1).

The tests that prove a magnet link becomes bytes over HTTP therefore live in
`tests/torrent_stream.rs` and are `#[ignore]`d. Run them deliberately:

```bash
cargo test --test torrent_stream -- --ignored --nocapture
```

They use Big Buck Bunny, which is Creative Commons Attribution and the standard fixture for
exactly this. Nothing in this project ships with a content source (madde 9) — that is a
fixture named in a test file, and the application never comes with it.

## Not implemented, deliberately

- **Private trackers.** A passkey in a tracker URL is a credential, and handling it
  properly — never logged, never shown, never in an error — is a slice of its own.
- **Seeding past the session.** Uploading continues while the application runs, because a
  client that leeches by default breaks the swarm everyone depends on, and stops when it
  exits unless asked otherwise.
- **BitTorrent v2.** A `btmh` magnet is refused by name rather than timing out: "no
  metadata after 60s" is a confusing way to say "this build does not read v2".
- **Embedded libmpv.** Behind the `embedded-mpv` feature gate, so the deferred decision
  stays visible (madde 2).

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
