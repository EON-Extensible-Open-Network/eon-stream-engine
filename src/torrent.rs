// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! BitTorrent streaming.
//!
//! v1 item 4 of the definition-of-done: sequential download plus a local HTTP
//! server feeding the player. This half adds the torrent to a session and
//! tracks it; [`crate::server`] is the half mpv talks to.
//!
//! ## Sequential download is librqbit's, and that is the point
//!
//! The decision in madde 1 was librqbit over a client written here, and the
//! reason is exactly this module's job. `ManagedTorrent::stream` hands back a
//! `FileStream` that keeps a window of pieces queued **ahead of wherever the
//! stream has been read to**, and re-queues around a seek. That is the one
//! piece of BitTorrent behaviour this project actually needed, and getting to
//! it by hand would have meant DHT, uTP, web seeds and tracker protocols
//! first.
//!
//! So nothing here reimplements piece selection. What it does is give that
//! machinery a synchronous surface, a prebuffer the player will not starve on,
//! and progress a person can read.
//!
//! ## Why this crate owns a runtime
//!
//! librqbit is async; the callers are a terminal loop and, later, a Tauri
//! command handler. Neither should have to own a tokio runtime or colour its
//! own functions async to play a video. So the runtime lives in
//! [`TorrentEngine`] and every method here blocks. The cost is one runtime per
//! engine, which is one per application.
//!
//! ## What is deliberately not here
//!
//! * **No private-tracker support.** Passkeys in tracker URLs are credentials,
//!   and the handling they need (never log, never show, never put in an error)
//!   is a slice of its own.
//! * **No seeding beyond the session**, unless asked for. The default keeps
//!   uploading while the application runs — leeching by default breaks the
//!   swarm everyone depends on — and stops when it exits.
//! * **No content of any kind.** Nothing here ships with a tracker list, an
//!   index, or a torrent (madde 9). Every tracker used comes from the magnet
//!   link the user supplied.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use librqbit::{
    api::TorrentIdOrHash, AddTorrent, AddTorrentOptions, ManagedTorrent, Session, SessionOptions,
};

use crate::{
    error::{Error, Result},
    server::LocalServer,
};

/// File extensions treated as video.
///
/// Used only to pick a default file out of a multi-file torrent, which is why
/// a wrong guess is recoverable: the caller can always name an index. The list
/// is what mpv opens without argument, not every container that exists.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "mov", "m4v", "webm", "ts", "m2ts", "mpg", "mpeg", "wmv", "flv", "ogv",
    "mts", "vob", "divx", "rmvb", "3gp",
];

/// Extensions that are plausibly a video file but are usually something else.
///
/// Kept apart so a sample or an advert does not win the "largest video" pick.
const JUNK_MARKERS: &[&str] = &["sample", "trailer", "advert", "rarbg", "screens"];

/// How a torrent was identified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TorrentRequest {
    /// A full magnet link, trackers and all.
    Magnet(String),
    /// A bare info hash, with whatever trackers the caller knows.
    ///
    /// This is the shape an addon's stream object gives: `infoHash` plus
    /// `sources`. With no trackers it still works through the DHT, slowly.
    InfoHash {
        /// 40 hex characters.
        hex: String,
        /// Tracker URLs to announce to.
        trackers: Vec<String>,
    },
}

impl TorrentRequest {
    /// Build a request from an addon's `infoHash` and `sources`.
    ///
    /// Stremio-compatible addons put trackers in `sources` as
    /// `tracker:<url>` and peers as `dht:<hash>`; anything else is ignored
    /// rather than guessed at.
    #[must_use]
    pub fn from_addon(info_hash: &str, sources: &[String]) -> Self {
        let trackers = sources
            .iter()
            .filter_map(|source| source.strip_prefix("tracker:"))
            .map(ToOwned::to_owned)
            .collect();
        Self::InfoHash {
            hex: info_hash.to_owned(),
            trackers,
        }
    }

    /// The info hash, lowercase hex.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTorrentSource`] if it is not 40 hex characters, or if a
    /// magnet link carries no v1 info hash.
    pub fn info_hash(&self) -> Result<String> {
        match self {
            Self::InfoHash { hex, .. } => validate_info_hash(hex),
            Self::Magnet(magnet) => {
                let lower = magnet.to_ascii_lowercase();
                if !lower.starts_with("magnet:") {
                    return Err(Error::InvalidTorrentSource {
                        reason: "a magnet link starts with 'magnet:'".to_owned(),
                    });
                }
                // `xt=urn:btih:<hash>`. A v2-only magnet (`btmh`) is refused by
                // name: librqbit is BitTorrent v1, and "no metadata" would be a
                // confusing way to say so.
                let hash = lower
                    .split(&['?', '&'][..])
                    .find_map(|part| part.strip_prefix("xt=urn:btih:"));
                match hash {
                    Some(hash) => validate_info_hash(hash),
                    None if lower.contains("xt=urn:btmh:") => Err(Error::InvalidTorrentSource {
                        reason: "this is a BitTorrent v2 magnet link, which this build \
                                 does not read"
                            .to_owned(),
                    }),
                    None => Err(Error::InvalidTorrentSource {
                        reason: "the magnet link carries no btih info hash".to_owned(),
                    }),
                }
            }
        }
    }

    /// What to hand librqbit.
    fn as_url(&self) -> Result<String> {
        match self {
            Self::Magnet(magnet) => Ok(magnet.clone()),
            Self::InfoHash { hex, .. } => validate_info_hash(hex),
        }
    }

    /// Trackers to announce to, beyond anything in a magnet link.
    fn trackers(&self) -> Vec<String> {
        match self {
            Self::Magnet(_) => Vec::new(),
            Self::InfoHash { trackers, .. } => trackers
                .iter()
                // Only the schemes a tracker actually speaks. An addon can put
                // anything in `sources`, and this list is what gets dialled.
                .filter(|url| {
                    let lower = url.to_ascii_lowercase();
                    lower.starts_with("udp://")
                        || lower.starts_with("http://")
                        || lower.starts_with("https://")
                        || lower.starts_with("ws://")
                        || lower.starts_with("wss://")
                })
                .cloned()
                .collect(),
        }
    }
}

fn validate_info_hash(hex: &str) -> Result<String> {
    let hex = hex.trim();
    if hex.len() != 40 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::InvalidTorrentSource {
            reason: format!(
                "an info hash is 40 hexadecimal characters; this is {} character(s)",
                hex.len()
            ),
        });
    }
    Ok(hex.to_ascii_lowercase())
}

/// How the engine behaves.
#[derive(Debug, Clone)]
pub struct TorrentOptions {
    /// Where pieces are written.
    pub download_dir: PathBuf,
    /// Most peers per torrent.
    pub peer_limit: Option<usize>,
    /// Download ceiling in bytes per second.
    pub download_limit: Option<u32>,
    /// Upload ceiling in bytes per second.
    pub upload_limit: Option<u32>,
    /// How long to wait for metadata before giving up.
    pub metadata_timeout: Duration,
    /// How much to have in hand before handing the file to the player.
    pub prebuffer_bytes: u64,
    /// Keep the pieces on disk when the engine shuts down.
    pub keep_files: bool,
    /// Announce to trackers at all.
    ///
    /// Off means DHT and local discovery only. Not a privacy feature — peers
    /// still see an address — but it is the switch an institution asks for.
    pub use_trackers: bool,
}

impl Default for TorrentOptions {
    fn default() -> Self {
        Self {
            download_dir: PathBuf::from("."),
            peer_limit: Some(100),
            download_limit: None,
            upload_limit: None,
            metadata_timeout: Duration::from_secs(60),
            prebuffer_bytes: 16 * 1024 * 1024,
            keep_files: false,
            use_trackers: true,
        }
    }
}

/// One file inside a torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentFile {
    /// Its index, which is what a caller names to pick it.
    pub index: usize,
    /// Its path inside the torrent, as the torrent spells it.
    pub path: String,
    /// Its length in bytes.
    pub length: u64,
    /// Whether the extension says video.
    pub is_video: bool,
}

impl TorrentFile {
    /// The file name without its directories.
    #[must_use]
    pub fn name(&self) -> &str {
        self.path.rsplit(['/', '\\']).next().unwrap_or(&self.path)
    }

    /// Its extension, lowercased, without the dot.
    #[must_use]
    pub fn extension(&self) -> String {
        self.name()
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default()
    }

    /// Whether the name suggests a sample, trailer or release-group filler.
    #[must_use]
    pub fn looks_like_filler(&self) -> bool {
        let lower = self.path.to_ascii_lowercase();
        JUNK_MARKERS.iter().any(|marker| lower.contains(marker))
    }
}

/// A torrent this engine is managing.
#[derive(Debug, Clone)]
pub struct TorrentHandle {
    info_hash: String,
    name: Option<String>,
    files: Vec<TorrentFile>,
    total_bytes: u64,
}

impl TorrentHandle {
    /// Its info hash, lowercase hex.
    #[must_use]
    pub fn info_hash(&self) -> &str {
        &self.info_hash
    }

    /// The torrent's own name, once metadata resolved.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Every file in it.
    #[must_use]
    pub fn files(&self) -> &[TorrentFile] {
        &self.files
    }

    /// Total size of every file.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// The file a viewer most likely meant.
    ///
    /// The largest video file that does not look like filler, because that is
    /// what is right nearly every time: a season pack's episodes are similar
    /// sizes and a movie's extras are much smaller, while `sample.mkv` is
    /// small and named. If the only videos look like filler the largest one
    /// still wins — a wrong pick the caller can override beats refusing to
    /// guess.
    ///
    /// # Errors
    ///
    /// [`Error::NoVideoInTorrent`] when nothing in it is video at all.
    pub fn best_video(&self) -> Result<&TorrentFile> {
        let pick = |filler_allowed: bool| {
            self.files
                .iter()
                .filter(|f| f.is_video && (filler_allowed || !f.looks_like_filler()))
                .max_by_key(|f| f.length)
        };
        pick(false)
            .or_else(|| pick(true))
            .ok_or(Error::NoVideoInTorrent {
                count: self.files.len(),
            })
    }

    /// One file by index.
    ///
    /// # Errors
    ///
    /// [`Error::FileIndexOutOfRange`] if there is no such file.
    pub fn file(&self, index: usize) -> Result<&TorrentFile> {
        self.files.get(index).ok_or(Error::FileIndexOutOfRange {
            index,
            count: self.files.len(),
        })
    }
}

/// How a torrent is getting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TorrentProgress {
    /// Bytes verified so far.
    pub downloaded: u64,
    /// Bytes in the files being downloaded.
    pub total: u64,
    /// Download rate in bytes per second.
    pub download_bps: u64,
    /// Upload rate in bytes per second.
    pub upload_bps: u64,
    /// Bytes uploaded this session.
    pub uploaded: u64,
    /// Peers currently connected.
    pub peers: u32,
    /// Whether every wanted piece is in hand.
    pub finished: bool,
}

impl TorrentProgress {
    /// Progress as a fraction between 0.0 and 1.0.
    #[must_use]
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let fraction = self.downloaded as f64 / self.total as f64;
        fraction.clamp(0.0, 1.0)
    }

    /// Seconds until done at the current rate, or `None` when that cannot be
    /// said.
    ///
    /// `None` rather than a huge number when nothing is arriving: an estimate
    /// of four thousand hours is not information, and showing it makes every
    /// other estimate less believable.
    #[must_use]
    pub fn eta_seconds(&self) -> Option<u64> {
        if self.finished {
            return Some(0);
        }
        if self.download_bps == 0 || self.total <= self.downloaded {
            return None;
        }
        Some((self.total - self.downloaded) / self.download_bps)
    }
}

/// A torrent session with a loopback server in front of it.
pub struct TorrentEngine {
    runtime: tokio::runtime::Runtime,
    session: Arc<Session>,
    server: LocalServer,
    options: TorrentOptions,
}

impl TorrentEngine {
    /// Start a session and the loopback server.
    ///
    /// # Errors
    ///
    /// [`Error::TorrentEngine`] if the runtime or session will not start, or
    /// [`Error::ServerStart`] if nothing can be bound on loopback.
    pub fn start(options: TorrentOptions) -> Result<Self> {
        std::fs::create_dir_all(&options.download_dir).map_err(|e| Error::TorrentEngine {
            message: format!(
                "could not create the download directory {}: {e}",
                options.download_dir.display()
            ),
        })?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            // Enough for disk, peers and the loopback server without making
            // one torrent look like a reason to own every core.
            .worker_threads(4)
            .enable_all()
            .thread_name("eon-torrent")
            .build()
            .map_err(|e| Error::TorrentEngine {
                message: format!("could not start the async runtime: {e}"),
            })?;

        let session_options = SessionOptions {
            disable_trackers: !options.use_trackers,
            // Nothing is remembered between runs. A session file would be a
            // record of what was watched, sitting on disk, which this project
            // does not keep (madde 36 in spirit: absent beats off).
            persistence: None,
            fastresume: false,
            peer_limit: options.peer_limit,
            ratelimits: librqbit::limits::LimitsConfig {
                // A limit of zero would stall rather than mean unlimited, so
                // the type is `NonZeroU32` and a zero here becomes `None`.
                // Settings refuse a zero too; this is the second line.
                upload_bps: options.upload_limit.and_then(std::num::NonZeroU32::new),
                download_bps: options.download_limit.and_then(std::num::NonZeroU32::new),
            },
            ..Default::default()
        };

        let download_dir = options.download_dir.clone();
        let session = runtime
            .block_on(Session::new_with_opts(download_dir, session_options))
            .map_err(|e| Error::TorrentEngine {
                message: format!("{e:#}"),
            })?;

        let server = LocalServer::start(runtime.handle(), Arc::clone(&session))?;

        Ok(Self {
            runtime,
            session,
            server,
            options,
        })
    }

    /// The options this engine was started with.
    #[must_use]
    pub fn options(&self) -> &TorrentOptions {
        &self.options
    }

    /// Where the loopback server is listening.
    #[must_use]
    pub fn server_address(&self) -> SocketAddr {
        self.server.address()
    }

    /// Add a torrent and wait for its metadata.
    ///
    /// Blocks until the file list is known or [`TorrentOptions::metadata_timeout`]
    /// passes. There is nothing useful to return before then: a torrent whose
    /// file list is unknown cannot be played from, and reporting "added" at
    /// that point invites a caller to act on nothing.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTorrentSource`] for a malformed request,
    /// [`Error::MetadataTimeout`] when the swarm does not answer, or
    /// [`Error::TorrentEngine`] for anything librqbit reports.
    pub fn add(&self, request: &TorrentRequest) -> Result<TorrentHandle> {
        let url = request.as_url()?;
        let expected = request.info_hash()?;
        let trackers = request.trackers();
        let timeout = self.options.metadata_timeout;

        let session = Arc::clone(&self.session);
        let handle = self.runtime.block_on(async move {
            let options = AddTorrentOptions {
                // Resuming over files already on disk needs this; without it a
                // second run of the same torrent refuses rather than reusing
                // what it already fetched.
                overwrite: true,
                trackers: if trackers.is_empty() {
                    None
                } else {
                    Some(trackers)
                },
                ..Default::default()
            };
            let response = session
                .add_torrent(AddTorrent::from_url(url), Some(options))
                .await
                .map_err(|e| Error::TorrentEngine {
                    message: format!("{e:#}"),
                })?;
            let handle = response.into_handle().ok_or_else(|| Error::TorrentEngine {
                message: "the session accepted the torrent but returned no handle".to_owned(),
            })?;

            // Metadata arrives from the swarm for a magnet link, so this is
            // the wait that can genuinely fail.
            match tokio::time::timeout(timeout, handle.wait_until_initialized()).await {
                Err(_elapsed) => Err(Error::MetadataTimeout {
                    seconds: timeout.as_secs(),
                }),
                Ok(Err(e)) => Err(Error::TorrentEngine {
                    message: format!("{e:#}"),
                }),
                Ok(Ok(())) => Ok(handle),
            }
        })?;

        Ok(describe(&handle, &expected))
    }

    /// The handle for a torrent already in this session.
    ///
    /// # Errors
    ///
    /// [`Error::TorrentNotFound`] if nothing carries that info hash, or
    /// [`Error::InvalidTorrentSource`] if the hash is malformed.
    pub fn get(&self, info_hash: &str) -> Result<TorrentHandle> {
        let hex = validate_info_hash(info_hash)?;
        let managed = self.lookup(&hex)?;
        Ok(describe(&managed, &hex))
    }

    /// How a torrent is getting on.
    ///
    /// # Errors
    ///
    /// [`Error::TorrentNotFound`] if nothing carries that info hash.
    pub fn progress(&self, handle: &TorrentHandle) -> Result<TorrentProgress> {
        let managed = self.lookup(&handle.info_hash)?;
        let stats = managed.stats();
        let live = stats.live.as_ref();
        Ok(TorrentProgress {
            downloaded: stats.progress_bytes,
            total: stats.total_bytes,
            download_bps: live.map_or(0, |l| rate_to_bps(l.download_speed.mbps)),
            upload_bps: live.map_or(0, |l| rate_to_bps(l.upload_speed.mbps)),
            uploaded: stats.uploaded_bytes,
            peers: live.map_or(0, |l| l.snapshot.peer_stats.live),
            finished: stats.finished,
        })
    }

    /// How much of one file is in hand, in bytes.
    ///
    /// # Errors
    ///
    /// [`Error::TorrentNotFound`] if nothing carries that info hash, or
    /// [`Error::FileIndexOutOfRange`] if there is no such file.
    pub fn file_progress(&self, handle: &TorrentHandle, index: usize) -> Result<u64> {
        let _file = handle.file(index)?;
        let managed = self.lookup(&handle.info_hash)?;
        let stats = managed.stats();
        stats
            .file_progress
            .get(index)
            .copied()
            .ok_or(Error::FileIndexOutOfRange {
                index,
                count: stats.file_progress.len(),
            })
    }

    /// Wait until enough of a file is in hand to start playing it.
    ///
    /// `on_progress` is called while waiting so a caller can show something;
    /// returning `false` from it gives up.
    ///
    /// The target is the smaller of [`TorrentOptions::prebuffer_bytes`] and the
    /// file itself, because a four-megabyte clip is never going to produce
    /// sixteen megabytes and waiting for it would hang forever.
    ///
    /// # Errors
    ///
    /// [`Error::MetadataTimeout`] if the wait runs out — the same condition
    /// under a different name, which is "the swarm is not feeding us" —
    /// or whatever lookup reports.
    pub fn wait_for_prebuffer(
        &self,
        handle: &TorrentHandle,
        index: usize,
        timeout: Duration,
        mut on_progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<()> {
        let file = handle.file(index)?;
        let target = self.options.prebuffer_bytes.min(file.length);
        let started = Instant::now();

        // A stream has to exist for librqbit to prioritise this file's pieces
        // at all: the piece queue is built from the registered streams. Opening
        // one and dropping it immediately would un-prioritise the file, so the
        // handle is held for the whole wait.
        let managed = self.lookup(&handle.info_hash)?;
        let _priority = self.runtime.block_on(async {
            Arc::clone(&managed)
                .stream(index)
                .await
                .map_err(|e| Error::TorrentIo {
                    message: format!("{e:#}"),
                })
        })?;

        loop {
            let have = managed
                .stats()
                .file_progress
                .get(index)
                .copied()
                .unwrap_or(0);
            if have >= target {
                return Ok(());
            }
            if !on_progress(have, target) {
                return Ok(());
            }
            if started.elapsed() >= timeout {
                return Err(Error::MetadataTimeout {
                    seconds: timeout.as_secs(),
                });
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// The loopback URL the player should open for a file.
    ///
    /// Carries the session token and ends in the file's own name, because mpv
    /// picks its demuxer and its subtitle search from the path.
    ///
    /// # Errors
    ///
    /// [`Error::FileIndexOutOfRange`] if there is no such file.
    pub fn stream_url(&self, handle: &TorrentHandle, index: usize) -> Result<String> {
        let file = handle.file(index)?;
        Ok(self.server.url_for(&handle.info_hash, index, file.name()))
    }

    /// Stop managing a torrent.
    ///
    /// # Errors
    ///
    /// [`Error::TorrentEngine`] if the session refuses, or
    /// [`Error::TorrentNotFound`] if nothing carries that info hash.
    pub fn remove(&self, handle: &TorrentHandle, delete_files: bool) -> Result<()> {
        let hex = validate_info_hash(&handle.info_hash)?;
        let id = self.id_for(&hex)?;
        let session = Arc::clone(&self.session);
        self.runtime
            .block_on(async move { session.delete(id, delete_files).await })
            .map_err(|e| Error::TorrentEngine {
                message: format!("{e:#}"),
            })
    }

    /// Every torrent in this session.
    #[must_use]
    pub fn list(&self) -> Vec<TorrentHandle> {
        self.session.with_torrents(|torrents| {
            let mut out = Vec::new();
            for (_, managed) in torrents {
                let hex = managed.info_hash().as_string();
                out.push(describe(managed, &hex));
            }
            out
        })
    }

    /// Shut the session down, honouring [`TorrentOptions::keep_files`].
    ///
    /// Takes `self` so an engine cannot be used afterwards, and so the runtime
    /// is dropped here rather than at an unpredictable point.
    ///
    /// Returns the directory the pieces were left in, when they were kept.
    pub fn shutdown(self) -> Option<PathBuf> {
        let keep = self.options.keep_files;
        let directory = self.options.download_dir.clone();

        if !keep {
            // Delete through the session rather than by removing the
            // directory: the session knows which files belong to which torrent,
            // and a recursive delete of a user-chosen directory is not
            // something this code should ever do.
            let handles: Vec<TorrentIdOrHash> = self
                .session
                .with_torrents(|torrents| torrents.map(|(id, _)| id.into()).collect());
            for id in handles {
                let session = Arc::clone(&self.session);
                let _ = self
                    .runtime
                    .block_on(async move { session.delete(id, true).await });
            }
        }

        self.server.stop();
        // Give the session's tasks a moment to finish writing before the
        // runtime goes away, then drop it. `shutdown_timeout` rather than a
        // plain drop: dropping a runtime from inside blocking code can wait
        // forever on a task that is mid-write.
        self.runtime.shutdown_timeout(Duration::from_secs(5));

        if keep {
            Some(directory)
        } else {
            None
        }
    }

    fn lookup(&self, info_hash: &str) -> Result<Arc<ManagedTorrent>> {
        self.session
            .with_torrents(|torrents| {
                // A plain loop: `with_torrents` hands over a `&mut dyn
                // Iterator`, and the adaptor methods are not callable on a
                // trait object.
                for (_, managed) in torrents {
                    if managed
                        .info_hash()
                        .as_string()
                        .eq_ignore_ascii_case(info_hash)
                    {
                        return Some(Arc::clone(managed));
                    }
                }
                None
            })
            .ok_or_else(|| Error::TorrentNotFound {
                info_hash: info_hash.to_owned(),
            })
    }

    fn id_for(&self, info_hash: &str) -> Result<TorrentIdOrHash> {
        self.session
            .with_torrents(|torrents| {
                for (id, managed) in torrents {
                    if managed
                        .info_hash()
                        .as_string()
                        .eq_ignore_ascii_case(info_hash)
                    {
                        return Some(TorrentIdOrHash::from(id));
                    }
                }
                None
            })
            .ok_or_else(|| Error::TorrentNotFound {
                info_hash: info_hash.to_owned(),
            })
    }
}

/// Read a managed torrent's metadata into our own types.
fn describe(managed: &Arc<ManagedTorrent>, info_hash: &str) -> TorrentHandle {
    let mut files = Vec::new();
    let mut total_bytes = 0;
    managed
        .with_metadata(|metadata| {
            for (index, info) in metadata.file_infos.iter().enumerate() {
                let path = info.relative_filename.to_string_lossy().replace('\\', "/");
                let file = TorrentFile {
                    index,
                    is_video: is_video_path(&path),
                    path,
                    length: info.len,
                };
                total_bytes += file.length;
                files.push(file);
            }
        })
        .ok();

    TorrentHandle {
        info_hash: info_hash.to_ascii_lowercase(),
        name: managed.name(),
        files,
        total_bytes,
    }
}

fn is_video_path(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((_, extension)) => {
            let extension = extension.to_ascii_lowercase();
            VIDEO_EXTENSIONS.contains(&extension.as_str())
        }
        None => false,
    }
}

/// librqbit reports rates in megabits per second as a float; this crate's
/// callers want bytes per second as an integer.
fn rate_to_bps(mbps: f64) -> u64 {
    if !mbps.is_finite() || mbps <= 0.0 {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bps = (mbps * 1_000_000.0 / 8.0).clamp(0.0, u64::MAX as f64) as u64;
    bps
}

/// Where to put pieces by default: a directory beside the executable, so the
/// binary stays self-contained and leaves nothing elsewhere.
///
/// # Errors
///
/// [`Error::TorrentEngine`] if the executable's own location cannot be read,
/// which would also mean nothing else about this process is knowable.
pub fn default_download_dir(name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| Error::TorrentEngine {
        message: format!("could not find this executable's directory: {e}"),
    })?;
    let parent: &Path = exe.parent().ok_or_else(|| Error::TorrentEngine {
        message: "this executable appears to have no parent directory".to_owned(),
    })?;
    Ok(parent.join(name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn file(index: usize, path: &str, length: u64) -> TorrentFile {
        TorrentFile {
            index,
            is_video: is_video_path(path),
            path: path.to_owned(),
            length,
        }
    }

    fn handle(files: Vec<TorrentFile>) -> TorrentHandle {
        let total_bytes = files.iter().map(|f| f.length).sum();
        TorrentHandle {
            info_hash: "0".repeat(40),
            name: Some("Test torrent".to_owned()),
            files,
            total_bytes,
        }
    }

    #[test]
    fn a_magnet_link_yields_its_info_hash() {
        let magnet = TorrentRequest::Magnet(
            "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck+Bunny"
                .to_owned(),
        );
        assert_eq!(
            magnet.info_hash().unwrap(),
            "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c"
        );
    }

    #[test]
    fn a_v2_only_magnet_is_refused_by_name() {
        // "No metadata after 60s" would be a confusing way to say "this build
        // does not read v2".
        let magnet = TorrentRequest::Magnet(
            "magnet:?xt=urn:btmh:1220caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e"
                .to_owned(),
        );
        let err = magnet.info_hash().unwrap_err().to_string();
        assert!(err.contains("v2"), "{err}");
    }

    #[test]
    fn malformed_sources_are_refused() {
        for bad in [
            TorrentRequest::Magnet("not a magnet".to_owned()),
            TorrentRequest::Magnet("magnet:?dn=no+hash".to_owned()),
            TorrentRequest::InfoHash {
                hex: "tooshort".to_owned(),
                trackers: Vec::new(),
            },
            TorrentRequest::InfoHash {
                hex: "z".repeat(40),
                trackers: Vec::new(),
            },
        ] {
            assert!(bad.info_hash().is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn an_info_hash_is_normalised_to_lowercase() {
        let request = TorrentRequest::InfoHash {
            hex: "DD8255ECDC7CA55FB0BBF81323D87062DB1F6D1C".to_owned(),
            trackers: Vec::new(),
        };
        assert_eq!(
            request.info_hash().unwrap(),
            "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c"
        );
    }

    #[test]
    fn addon_sources_become_trackers() {
        let request = TorrentRequest::from_addon(
            "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c",
            &[
                "tracker:udp://tracker.example.org:1337/announce".to_owned(),
                "tracker:https://tracker.example.net/announce".to_owned(),
                "dht:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c".to_owned(),
            ],
        );
        let trackers = request.trackers();
        assert_eq!(trackers.len(), 2);
        assert!(trackers[0].starts_with("udp://"));
    }

    #[test]
    fn only_dialable_tracker_schemes_survive() {
        // An addon can put anything in `sources`, and this list gets dialled.
        let request = TorrentRequest::InfoHash {
            hex: "d".repeat(40),
            trackers: vec![
                "udp://ok.example:1337/announce".to_owned(),
                "file:///etc/passwd".to_owned(),
                "javascript:alert(1)".to_owned(),
                "".to_owned(),
                "https://ok.example/announce".to_owned(),
            ],
        };
        let trackers = request.trackers();
        assert_eq!(trackers.len(), 2);
        assert!(!trackers.iter().any(|t| t.starts_with("file:")));
    }

    #[test]
    fn video_extensions_are_recognised_case_insensitively() {
        assert!(is_video_path("Movie/movie.MKV"));
        assert!(is_video_path("movie.mp4"));
        assert!(!is_video_path("movie.nfo"));
        assert!(!is_video_path("readme"));
        assert!(!is_video_path("movie.mkv.txt"));
    }

    #[test]
    fn the_largest_real_video_wins_over_a_sample() {
        // The case this heuristic exists for: nearly every release torrent
        // carries a sample, and picking it looks like the engine is broken.
        let torrent = handle(vec![
            file(0, "Movie.2026/Sample/sample.mkv", 30 * 1024 * 1024),
            file(1, "Movie.2026/Movie.2026.1080p.mkv", 8 * 1024 * 1024 * 1024),
            file(2, "Movie.2026/movie.nfo", 2048),
            file(3, "Movie.2026/poster.jpg", 150 * 1024),
        ]);
        let pick = torrent.best_video().unwrap();
        assert_eq!(pick.index, 1);
        assert_eq!(pick.name(), "Movie.2026.1080p.mkv");
    }

    #[test]
    fn a_sample_still_wins_if_it_is_the_only_video() {
        // A wrong pick the caller can override beats refusing to guess.
        let torrent = handle(vec![
            file(0, "Movie/sample.mkv", 30 * 1024 * 1024),
            file(1, "Movie/movie.nfo", 2048),
        ]);
        assert_eq!(torrent.best_video().unwrap().index, 0);
    }

    #[test]
    fn a_torrent_with_no_video_says_how_many_files_it_has() {
        let torrent = handle(vec![
            file(0, "album/01.flac", 40 * 1024 * 1024),
            file(1, "album/cover.jpg", 100 * 1024),
        ]);
        match torrent.best_video() {
            Err(Error::NoVideoInTorrent { count }) => assert_eq!(count, 2),
            other => panic!("expected NoVideoInTorrent, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_index_is_reported_with_the_count() {
        let torrent = handle(vec![file(0, "a.mkv", 1024)]);
        match torrent.file(7) {
            Err(Error::FileIndexOutOfRange { index, count }) => {
                assert_eq!(index, 7);
                assert_eq!(count, 1);
            }
            other => panic!("expected FileIndexOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn file_names_and_extensions_come_out_of_the_path() {
        let f = file(0, "Season 1/Show.S01E02.1080p.mkv", 1024);
        assert_eq!(f.name(), "Show.S01E02.1080p.mkv");
        assert_eq!(f.extension(), "mkv");
        // Windows separators appear in torrents made on Windows.
        let f = file(0, "Season 1\\Show.mkv", 1024);
        assert_eq!(f.name(), "Show.mkv");
    }

    #[test]
    fn progress_reports_a_fraction_and_a_believable_eta() {
        let halfway = TorrentProgress {
            downloaded: 500,
            total: 1000,
            download_bps: 100,
            upload_bps: 10,
            uploaded: 50,
            peers: 4,
            finished: false,
        };
        assert!((halfway.fraction() - 0.5).abs() < f64::EPSILON);
        assert_eq!(halfway.eta_seconds(), Some(5));

        // Nothing arriving: no estimate at all, rather than a number that
        // makes every other estimate less believable.
        let stalled = TorrentProgress {
            download_bps: 0,
            ..halfway
        };
        assert_eq!(stalled.eta_seconds(), None);

        let done = TorrentProgress {
            downloaded: 1000,
            finished: true,
            download_bps: 0,
            ..halfway
        };
        assert_eq!(done.eta_seconds(), Some(0));
        assert!((done.fraction() - 1.0).abs() < f64::EPSILON);

        // An empty torrent does not divide by zero.
        let empty = TorrentProgress {
            downloaded: 0,
            total: 0,
            ..halfway
        };
        assert!((empty.fraction() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rates_convert_from_megabits_to_bytes() {
        assert_eq!(rate_to_bps(8.0), 1_000_000);
        assert_eq!(rate_to_bps(0.0), 0);
        // A nonsense rate becomes zero rather than an enormous number.
        assert_eq!(rate_to_bps(f64::NAN), 0);
        assert_eq!(rate_to_bps(-1.0), 0);
        assert_eq!(rate_to_bps(f64::INFINITY), 0);
    }

    #[test]
    fn filler_markers_are_matched_anywhere_in_the_path() {
        assert!(file(0, "Movie/Sample/x.mkv", 1).looks_like_filler());
        assert!(file(0, "Movie/movie-trailer.mkv", 1).looks_like_filler());
        assert!(!file(0, "Movie/Movie.2026.mkv", 1).looks_like_filler());
    }

    #[test]
    fn default_options_seed_and_keep_nothing() {
        let options = TorrentOptions::default();
        assert!(!options.keep_files);
        assert!(options.use_trackers);
        assert_eq!(options.prebuffer_bytes, 16 * 1024 * 1024);
    }
}
