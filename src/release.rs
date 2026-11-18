// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Giving disk space back behind the playhead.
//!
//! A 4K episode can be sixty gigabytes. Kept whole, three of them fill a
//! laptop's disk, and someone without that much free space cannot watch one
//! to the end at all. What has been watched is mostly not needed again, so
//! the part of the file well behind the playhead is handed back to the file
//! system while the rest keeps playing.
//!
//! This works on a *sparse* file, where only the ranges actually written
//! occupy the disk: a range of one can be deallocated in place
//! (`FSCTL_SET_ZERO_DATA`), which frees its clusters without touching the
//! rest of the file.
//!
//! librqbit means to make its files sparse on Windows and does not manage
//! to: it calls `DeviceIoControl(FSCTL_SET_SPARSE)` with both the
//! bytes-returned and the overlapped pointer null, which Windows refuses for
//! a synchronous handle, and the failure is only logged. The file is then
//! grown to its full length with `set_len` and NTFS reserves every byte of
//! it -- which is why each torrent took its whole size on disk the moment it
//! started, sixty gigabytes for a 4K episode before a minute of it had
//! arrived. The guard below marks every file sparse itself, correctly, after
//! the file is created and before its length is set.
//!
//! The torrent engine still believes it has those pieces. Two things follow,
//! and both are handled here rather than hoped about:
//!
//! * A released range must never be *read*: a peer asking for those pieces
//!   would be sent zeros, and a player seeking back would decode them. Every
//!   torrent is therefore opened through [`GuardedFactory`], which refuses a
//!   read that touches a released range. librqbit treats that like any other
//!   storage error for an upload; for playback, the host never lets a seek
//!   reach a released range -- it restarts the stream at the target instead.
//! * Which ranges are released is the caller's decision, from where the
//!   player has really read, not from a guess at the byte a timestamp falls
//!   on (see the host's keeper).

use std::collections::HashMap;
use std::io::IoSlice;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, Mutex};

use librqbit::storage::filesystem::FilesystemStorageFactory;
use librqbit::storage::{BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage};
use librqbit::{ManagedTorrentShared, TorrentMetadata};

/// The ranges of each file that have been given back, by file index.
#[derive(Clone, Default)]
pub(crate) struct Released(Arc<Mutex<HashMap<usize, Vec<Range<u64>>>>>);

impl Released {
    /// Whether `len` bytes at `offset` in file `file` touch a released range.
    fn touches(&self, file: usize, offset: u64, len: u64) -> bool {
        let end = offset.saturating_add(len);
        self.0.lock().map_or(true, |released| {
            released
                .get(&file)
                .is_some_and(|ranges| ranges.iter().any(|r| r.start < end && offset < r.end))
        })
    }

    /// Record a range as given back.
    pub(crate) fn add(&self, file: usize, range: Range<u64>) {
        if let Ok(mut released) = self.0.lock() {
            released.entry(file).or_default().push(range);
        }
    }

    /// Where the released part of a file ends, if any of it is released.
    pub(crate) fn end(&self, file: usize) -> Option<u64> {
        self.0
            .lock()
            .ok()?
            .get(&file)
            .and_then(|ranges| ranges.iter().map(|r| r.end).max())
    }
}

/// librqbit's own file storage, with released ranges made unreadable.
#[derive(Clone)]
pub(crate) struct GuardedFactory {
    inner: FilesystemStorageFactory,
    released: Released,
    /// The torrent's own folder, which the engine chooses -- so the guard
    /// knows where each file is without asking librqbit, which does not say.
    folder: std::path::PathBuf,
}

impl GuardedFactory {
    pub(crate) fn boxed_with(released: Released, folder: std::path::PathBuf) -> BoxStorageFactory {
        Self {
            inner: FilesystemStorageFactory::default(),
            released,
            folder,
        }
        .boxed()
    }
}

impl StorageFactory for GuardedFactory {
    type Storage = Guarded;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Guarded> {
        Ok(Guarded {
            inner: Box::new(self.inner.create(shared, metadata)?),
            released: self.released.clone(),
            folder: self.folder.clone(),
        })
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

/// One torrent's storage, guarded.
pub(crate) struct Guarded {
    inner: Box<dyn TorrentStorage>,
    released: Released,
    folder: std::path::PathBuf,
}

impl TorrentStorage for Guarded {
    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        self.inner.init(shared, metadata)?;
        // The files exist now and have not been grown yet: this is the moment
        // a sparse flag keeps them from being allocated whole.
        for file in metadata
            .file_infos
            .iter()
            .filter(|file| !file.attrs.padding)
        {
            let path = self.folder.join(&file.relative_filename);
            if let Err(e) = make_sparse(&path) {
                tracing_free_warning(&path, &e);
            }
        }
        Ok(())
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        if self.released.touches(file_id, offset, buf.len() as u64) {
            anyhow::bail!("that part of the file was given back to save disk space");
        }
        self.inner.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.inner.pwrite_all(file_id, offset, buf)
    }

    fn pwrite_all_vectored(
        &self,
        file_id: usize,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        self.inner.pwrite_all_vectored(file_id, offset, bufs)
    }

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        self.inner.remove_file(file_id, filename)
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        self.inner.remove_directory_if_empty(path)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.inner.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(Self {
            inner: self.inner.take()?,
            released: self.released.clone(),
            folder: self.folder.clone(),
        }))
    }

    // `on_piece_completed` keeps its default: librqbit's file storage does
    // nothing there either.
}

/// Say once, on standard error, that a file could not be made sparse: it
/// will still play, it just takes its full size on disk.
fn tracing_free_warning(path: &Path, error: &std::io::Error) {
    eprintln!(
        "could not make {} sparse, so it takes its whole size on disk: {error}",
        path.display()
    );
}

/// Bytes a file actually occupies on disk, which for a sparse file is less
/// than its length -- the only honest measure of what the cache costs.
///
/// # Errors
///
/// Whatever the file system reports.
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn on_disk(path: &Path) -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut high = 0u32;
    // SAFETY: `wide` is NUL-terminated and outlives the call; `high` is a
    // valid u32 for the upper half of the size.
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &raw mut high) };
    if low == u32::MAX {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(0) {
            return Err(error);
        }
    }
    Ok((u64::from(high) << 32) | u64::from(low))
}

#[cfg(not(windows))]
pub(crate) fn on_disk(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.blocks() * 512)
}

/// Mark a file sparse, so that only what is written to it takes space.
///
/// # Errors
///
/// Whatever the file system reports; FAT32 and exFAT have no sparse files.
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn make_sparse(path: &Path) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    let mut returned = 0u32;
    // SAFETY: `file` is open for writing and outlives the call; no input or
    // output buffer is passed (a null input sets the flag); `returned` is a
    // valid u32 -- it must not be null for a synchronous call, which is
    // librqbit's bug -- and there is no OVERLAPPED.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn make_sparse(_path: &Path) -> std::io::Result<()> {
    // `set_len` already leaves a hole on the file systems this would run on.
    Ok(())
}

/// Deallocate `range` of the file at `path`, which must be sparse for the
/// space to come back (librqbit makes it so on its first write).
///
/// # Errors
///
/// Whatever the file system reports.
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn give_back(path: &Path, range: Range<u64>) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::{FILE_ZERO_DATA_INFORMATION, FSCTL_SET_ZERO_DATA};
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let to_i64 = |n: u64| {
        i64::try_from(n).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    };
    // Opened again rather than borrowed from librqbit: its handles are
    // private, and Rust opens files on Windows sharing read, write and
    // delete, so a second handle is allowed while the first is in use.
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    let info = FILE_ZERO_DATA_INFORMATION {
        FileOffset: to_i64(range.start)?,
        BeyondFinalZero: to_i64(range.end)?,
    };
    let mut returned = 0u32;
    #[allow(clippy::cast_possible_truncation)]
    let size = std::mem::size_of::<FILE_ZERO_DATA_INFORMATION>() as u32;
    // SAFETY: `file` is open for writing and outlives the call, so the handle
    // is valid; `info` is a fully initialised FILE_ZERO_DATA_INFORMATION that
    // lives on this stack frame for the whole (synchronous: no OVERLAPPED)
    // call, and `size` is its exact size; there is no output buffer, and
    // `returned` is a valid u32 to write the byte count into.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_ZERO_DATA,
            std::ptr::from_ref(&info).cast(),
            size,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Bytes free for this user on the disk holding `path`.
///
/// # Errors
///
/// Whatever the file system reports, for example a path that does not exist.
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn free_space(path: &Path) -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call;
    // `free` is a valid u64 for the call to write; the two other outputs are
    // optional and passed as null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(free)
}

/// Not measured elsewhere yet: the limit the viewer set is the only bound.
#[cfg(not(windows))]
pub(crate) fn free_space(_path: &Path) -> std::io::Result<u64> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "free space is only measured on Windows in this alpha",
    ))
}

/// Elsewhere the range is only recorded, not deallocated: punching holes
/// there is `fallocate` on Linux and `F_PUNCHHOLE` on macOS, neither of which
/// this alpha ships for yet. Recording it still keeps the reads guarded.
#[cfg(not(windows))]
pub(crate) fn give_back(_path: &Path, _range: Range<u64>) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn the_free_space_of_a_real_disk_is_measured() -> std::io::Result<()> {
        assert!(free_space(&std::env::temp_dir())? > 0);
        Ok(())
    }

    #[test]
    fn a_read_that_touches_a_released_range_is_refused() {
        let released = Released::default();
        released.add(0, 100..200);
        assert!(released.touches(0, 150, 10));
        assert!(released.touches(0, 90, 20));
        assert!(released.touches(0, 199, 5));
        assert!(!released.touches(0, 200, 50));
        assert!(!released.touches(0, 0, 100));
        assert!(!released.touches(1, 150, 10));
        assert_eq!(released.end(0), Some(200));
        assert_eq!(released.end(1), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_released_range_of_a_sparse_file_reads_back_as_zeros() -> std::io::Result<()> {
        use std::io::{Read, Seek, SeekFrom, Write};
        let dir = std::env::temp_dir().join(format!("eon-release-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("film.bin");
        std::fs::File::create(&path)?;
        make_sparse(&path)?;
        {
            let file = std::fs::OpenOptions::new().write(true).open(&path)?;
            // Grown to a gigabyte, the way librqbit grows a file: sparse, so
            // nothing is reserved.
            file.set_len(1 << 30)?;
        }
        assert!(
            on_disk(&path)? < 1 << 20,
            "a grown sparse file reserves nothing"
        );
        {
            let mut file = std::fs::OpenOptions::new().write(true).open(&path)?;
            file.write_all(&vec![7u8; 1 << 20])?;
        }
        give_back(&path, 4096..(1 << 19))?;
        let mut file = std::fs::File::open(&path)?;
        let mut head = [0u8; 16];
        file.read_exact(&mut head)?;
        assert_eq!(head, [7u8; 16]);
        file.seek(SeekFrom::Start(8192))?;
        let mut middle = [1u8; 16];
        file.read_exact(&mut middle)?;
        assert_eq!(middle, [0u8; 16]);
        drop(file);
        assert!(on_disk(&path)? < 1 << 20, "the given-back range was freed");
        std::fs::remove_dir_all(dir)
    }
}
