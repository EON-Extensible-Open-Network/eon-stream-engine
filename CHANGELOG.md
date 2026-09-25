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

### Open decisions blocking implementation
- Torrent engine selection; librqbit is the leading candidate (madde 1).
- Whether the local server should also expose a seekable endpoint for non-torrent HTTP
  sources, or proxy them unchanged.

[Unreleased]: https://github.com/EON-Extensible-Open-Network/eon-stream-engine/compare/main...HEAD
