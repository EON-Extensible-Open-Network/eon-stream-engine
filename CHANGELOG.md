# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Nothing is released yet.

## [Unreleased]

### Added
- Crate skeleton: `torrent`, `server`, `player`, `source`, `error`.
- `embedded-mpv` feature gate, unimplemented, so the deferred decision stays visible
  (madde 2).
- `NOTICE` documenting the mpv/FFmpeg LGPL-vs-GPL distinction and what it requires before
  distribution - the project's most likely licensing mistake, written down where it will be
  read.
- CI: fmt, clippy, test on Linux and Windows; cargo-deny; REUSE; DCO check.

### Added - torrent streaming, end to end

v1 definition-of-done item 4: sequential download plus a local HTTP server feeding the
player. A torrent now plays.

- `torrent` - a session with a synchronous surface over librqbit. Add a magnet link or a
  bare info hash with trackers, wait for metadata, list the files, pick one, prebuffer
  ahead of the playhead, read progress, shut down. The runtime lives here so callers -- a
  terminal loop today, a Tauri command handler later -- do not have to own one or colour
  their own functions async to play a video.
- `server` - the loopback HTTP server mpv reads from. `GET` and `HEAD` only, HTTP range
  requests answered `206` with `Content-Range`, `416` with the real length for an
  unsatisfiable range. A seek in the player becomes a range request, which becomes a seek
  on librqbit's `FileStream`, which re-queues the piece window around the new position.
  That is the whole mechanism.
- File selection: the largest video that does not look like filler, because nearly every
  release torrent carries a sample and picking it looks like the engine is broken. A
  caller can always name an index instead, and does -- `fileIdx` from the addon, or
  `play <n> file <i>` from the user.
- `tests/torrent_stream.rs` - the chain against a real swarm, `#[ignore]`d. It proves what
  no unit test can: that a magnet link becomes bytes over HTTP with correct range
  semantics. Not in the gating suite, because a suite that goes red when strangers stop
  seeding stops being trusted (madde 1).

### Decided
- **madde 1: the torrent engine is librqbit.** It was the leading candidate and it is now
  the dependency. The deciding factor was not BitTorrent in general but the one behaviour
  this project actually needed: `ManagedTorrent::stream` keeps a window of pieces queued
  ahead of wherever the stream has been read to and re-queues around a seek. Reaching that
  by hand would have meant DHT, uTP, web seeds and tracker protocols first, to arrive at
  the same place.
- **The local server serves torrents only.** The other half of the open question is
  answered by not answering it: an HTTP source is already seekable, so proxying one through
  loopback would add a hop, a buffer and a second thing that can fail, to replace something
  mpv does natively. If a source ever needs rewriting mid-stream that decision can be
  revisited; nothing needs it now.

### Security posture of the loopback server
A loopback port is reachable by every program on the machine and every page in a browser,
which is the part that makes "it's only localhost" wrong. So: bound to `127.0.0.1` and
never `0.0.0.0`; a per-session 32-byte token in the path, compared in constant time,
regenerated every run and never written to disk; a wrong token gets `404` and not `403`,
because a `403` would confirm the path exists and turn the port into an oracle for what is
being watched; no `Access-Control-Allow-Origin`, so a browser page can send a request but
cannot read the answer; request lines and headers bounded, because an unbounded read on a
reachable port is a memory-exhaustion bug waiting for someone bored.

### Added dependencies
Each is a supply-chain commitment (madde 37), justified where it is declared: `librqbit`
with `default-features = false, features = ["rust-tls"]` -- rustls rather than native TLS,
so a Windows build needs no OpenSSL and the pinned toolchain is the whole toolchain
(madde 22); `tokio`, because librqbit is async and this crate's boundary is not; `rand`,
for the session token.

The HTTP server is hand-written rather than axum. hyper is already in the tree through
librqbit, so the saving is not compile time -- it is that the surface served on this
machine's loopback interface is a few hundred lines a person can read, with no routing
table, no middleware and no behaviour nobody chose.

### Not implemented, deliberately
- **Private trackers.** A passkey in a tracker URL is a credential, and handling it
  properly -- never logged, never shown, never in an error -- is a slice of its own.
- **Seeding past the session.** Uploading continues while the application runs, because a
  client that leeches by default breaks the swarm everyone depends on, and stops when it
  exits unless asked otherwise.
- **BitTorrent v2.** A `btmh` magnet is refused by name rather than timing out, because
  "no metadata after 60s" is a confusing way to say "this build does not read v2".
- **Any content at all.** No tracker list, no index, no torrent ships with this (madde 9).
  Every tracker used comes from the link the user supplied.

[Unreleased]: https://github.com/EON-Extensible-Open-Network/eon-stream-engine/compare/main...HEAD
