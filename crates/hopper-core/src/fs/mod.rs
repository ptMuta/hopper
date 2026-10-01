//! Filesystem primitives with durability and attribution.
//!
//! Two rules hold throughout hopper and are enforced here rather than remembered at call sites:
//!
//! 1. **No bare `io::Error` crosses a module boundary.** Every operation attaches what it was
//!    doing and which path it was doing it to. This is the single change that turns
//!    "No such file or directory (os error 2)" into a message someone can act on.
//! 2. **A file is never observed half-written.** Writes go to a temporary file in the same
//!    directory, are flushed and fsynced, and are then renamed into place — atomic on POSIX.
//!    Same-directory matters: a rename across filesystems is a copy, and copies are not atomic.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::model::{Digest, Hashes, MultiHasher};

/// An I/O failure, with the operation and path that caused it.
#[derive(Debug, thiserror::Error)]
#[error("failed to {op} {}", path.display())]
pub struct IoPath {
    pub op: &'static str,
    pub path: PathBuf,
    #[source]
    pub source: io::Error,
}

impl IoPath {
    pub fn new(op: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self {
            op,
            path: path.into(),
            source,
        }
    }

    pub fn kind(&self) -> io::ErrorKind {
        self.source.kind()
    }

    pub fn is_not_found(&self) -> bool {
        self.source.kind() == io::ErrorKind::NotFound
    }
}

type Result<T> = std::result::Result<T, IoPath>;

fn at<T>(op: &'static str, path: &Path, r: io::Result<T>) -> Result<T> {
    r.map_err(|e| IoPath::new(op, path, e))
}

pub fn read(path: &Path) -> Result<Vec<u8>> {
    at("read", path, fs::read(path))
}

/// Read a file that may legitimately be absent.
pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(IoPath::new("read", path, e)),
    }
}

pub fn create_dir_all(path: &Path) -> Result<()> {
    at("create directory", path, fs::create_dir_all(path))
}

pub fn rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).map_err(|e| IoPath::new("rename", from, e))
}

pub fn remove_file(path: &Path) -> Result<()> {
    at("remove", path, fs::remove_file(path))
}

/// Remove a file, treating "already gone" as success.
///
/// Used by recovery, where an operation may have completed before the crash.
pub fn remove_file_if_present(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(IoPath::new("remove", path, e)),
    }
}

/// Flush a directory entry so a rename into it survives a power loss.
///
/// Renaming is atomic with respect to readers, but the *directory entry* is not durable until
/// the directory itself is synced. Skipping this is the classic way to lose a file that every
/// API call claimed had been written.
pub fn fsync_dir(path: &Path) -> Result<()> {
    // Opening a directory read-only and syncing it is the portable POSIX idiom. Windows has no
    // equivalent and does not need one.
    #[cfg(unix)]
    {
        let f = at("open directory", path, fs::File::open(path))?;
        at("sync directory", path, f.sync_all())?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Write bytes so that readers see either the old file or the new one, never a partial write.
pub fn write_atomic(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    write_atomic_from(path, &mut &bytes[..], executable)
}

/// [`write_atomic`], streaming from a reader rather than holding the content in memory.
pub fn write_atomic_from<R: io::Read>(path: &Path, reader: &mut R, executable: bool) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    create_dir_all(parent)?;

    // Same directory, so the rename below is a true rename and not a cross-device copy.
    let tmp = temp_sibling(path);
    {
        let mut f = at("create", &tmp, fs::File::create(&tmp))?;
        at("write", &tmp, io::copy(reader, &mut f).map(drop))?;
        at("flush", &tmp, f.flush())?;
        set_executable(&f, &tmp, executable)?;
        // Durable before it is visible: a rename of an unsynced file can survive while its
        // contents do not.
        at("sync", &tmp, f.sync_all())?;
    }

    if let Err(e) = rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fsync_dir(parent)
}

#[cfg(unix)]
fn set_executable(f: &fs::File, path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    at(
        "set permissions on",
        path,
        f.set_permissions(fs::Permissions::from_mode(mode)),
    )
}

#[cfg(not(unix))]
fn set_executable(_f: &fs::File, _path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}

/// A unique sibling path for staging a write.
fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("hopper");
    // Process id plus a counter: unique within and across processes without pulling in a
    // random-number dependency for something this small.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!(".{name}.hopper-{}-{n}.tmp", std::process::id()))
}

/// Hash a file's contents, computing sha512 and sha1 in one pass.
pub fn hash_file(path: &Path) -> Result<(Hashes, Digest)> {
    let mut f = at("open", path, fs::File::open(path))?;
    let mut hasher = MultiHasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = at("read", path, f.read(&mut buf))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish())
}

/// What a path currently is, without following symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Missing,
    File,
    Dir,
    Symlink,
    Other,
}

/// Inspect a path **without** following symlinks.
///
/// Using `symlink_metadata` is deliberate: a symlink at a path we manage must be reported as a
/// symlink so reconcile can refuse it, not silently resolved to whatever it points at.
pub fn kind_of(path: &Path) -> Result<FileKind> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_symlink() => Ok(FileKind::Symlink),
        Ok(m) if m.is_file() => Ok(FileKind::File),
        Ok(m) if m.is_dir() => Ok(FileKind::Dir),
        Ok(_) => Ok(FileKind::Other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(FileKind::Missing),
        Err(e) => Err(IoPath::new("inspect", path, e)),
    }
}

/// Size and modification time, for the fast path that avoids re-hashing unchanged files.
pub fn stat_fast(path: &Path) -> Result<Option<(u64, u64)>> {
    match fs::metadata(path) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            Ok(Some((m.len(), mtime)))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(IoPath::new("stat", path, e)),
    }
}

/// Remove a directory only if it is empty. Used to tidy directories a removal emptied.
pub fn remove_dir_if_empty(path: &Path) -> Result<bool> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(true),
        // Not empty, or already gone: both mean "leave it", not "fail".
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                || e.kind() == io::ErrorKind::DirectoryNotEmpty =>
        {
            Ok(false)
        }
        // Older toolchains surface "not empty" as an unnamed OS error.
        Err(e) if e.raw_os_error() == Some(39) || e.raw_os_error() == Some(66) => Ok(false),
        Err(e) => Err(IoPath::new("remove directory", path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn errors_name_the_operation_and_the_path() {
        // The whole reason IoPath exists: a bare io::Error here would say only "os error 2".
        let dir = tmp();
        let missing = dir.path().join("nope.txt");
        let err = read(&missing).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("read"), "got {msg}");
        assert!(msg.contains("nope.txt"), "got {msg}");
        assert!(err.is_not_found());
    }

    #[test]
    fn atomic_write_creates_parent_directories() {
        let dir = tmp();
        let path = dir.path().join("a/b/c.txt");
        write_atomic(&path, b"hello", false).unwrap();
        assert_eq!(read(&path).unwrap(), b"hello");
    }

    #[test]
    fn atomic_write_replaces_existing_content() {
        let dir = tmp();
        let path = dir.path().join("x.txt");
        write_atomic(&path, b"first", false).unwrap();
        write_atomic(&path, b"second", false).unwrap();
        assert_eq!(read(&path).unwrap(), b"second");
    }

    #[test]
    fn atomic_write_leaves_no_temporary_files_behind() {
        let dir = tmp();
        write_atomic(&dir.path().join("x.txt"), b"data", false).unwrap();
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp") || n.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    fn the_temporary_file_is_a_sibling_so_rename_stays_atomic() {
        // A temp file in /tmp would make the rename a cross-device copy, which is not atomic.
        let path = Path::new("/srv/mc/mods/a.jar");
        assert_eq!(temp_sibling(path).parent(), path.parent());
    }

    #[cfg(unix)]
    #[test]
    fn executable_bit_is_applied() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp();
        let script = dir.path().join("start.sh");
        write_atomic(&script, b"#!/bin/sh\n", true).unwrap();
        let mode = fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "should be executable");

        let plain = dir.path().join("plain.txt");
        write_atomic(&plain, b"x", false).unwrap();
        let mode = fs::metadata(&plain).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0, "should not be executable");
    }

    #[test]
    fn hashing_matches_hashing_the_same_bytes_in_memory() {
        let dir = tmp();
        let path = dir.path().join("data.bin");
        // Larger than the read buffer, so the chunked loop is actually exercised.
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        write_atomic(&path, &data, false).unwrap();

        let (_, from_disk) = hash_file(&path).unwrap();
        let mut h = MultiHasher::new();
        h.update(&data);
        assert_eq!(from_disk, h.finish().1);
    }

    #[test]
    fn hashing_an_empty_file_works() {
        let dir = tmp();
        let path = dir.path().join("empty");
        write_atomic(&path, b"", false).unwrap();
        let (all, _) = hash_file(&path).unwrap();
        assert!(all.sha1.is_some() && all.sha512.is_some());
    }

    #[test]
    fn read_optional_distinguishes_absent_from_broken() {
        let dir = tmp();
        assert_eq!(read_optional(&dir.path().join("nope")).unwrap(), None);
        let path = dir.path().join("there");
        write_atomic(&path, b"x", false).unwrap();
        assert_eq!(read_optional(&path).unwrap(), Some(b"x".to_vec()));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_reported_as_a_symlink_not_its_target() {
        // Reconcile refuses symlinks; resolving them here would defeat that.
        let dir = tmp();
        let target = dir.path().join("real.jar");
        write_atomic(&target, b"x", false).unwrap();
        let link = dir.path().join("link.jar");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(kind_of(&link).unwrap(), FileKind::Symlink);
        assert_eq!(kind_of(&target).unwrap(), FileKind::File);
    }

    #[test]
    fn kind_of_reports_missing_rather_than_failing() {
        let dir = tmp();
        assert_eq!(
            kind_of(&dir.path().join("absent")).unwrap(),
            FileKind::Missing
        );
        assert_eq!(kind_of(dir.path()).unwrap(), FileKind::Dir);
    }

    #[test]
    fn remove_if_present_is_idempotent() {
        let dir = tmp();
        let path = dir.path().join("x");
        write_atomic(&path, b"x", false).unwrap();
        assert!(remove_file_if_present(&path).unwrap());
        assert!(
            !remove_file_if_present(&path).unwrap(),
            "second call is a no-op"
        );
    }

    #[test]
    fn empty_directories_are_pruned_and_full_ones_are_not() {
        let dir = tmp();
        let empty = dir.path().join("empty");
        create_dir_all(&empty).unwrap();
        assert!(remove_dir_if_empty(&empty).unwrap());

        let full = dir.path().join("full");
        write_atomic(&full.join("f.txt"), b"x", false).unwrap();
        assert!(
            !remove_dir_if_empty(&full).unwrap(),
            "a directory with content must survive"
        );
        assert!(full.exists());
    }

    #[test]
    fn stat_fast_reports_size_and_absence() {
        let dir = tmp();
        assert_eq!(stat_fast(&dir.path().join("nope")).unwrap(), None);
        let path = dir.path().join("x");
        write_atomic(&path, b"12345", false).unwrap();
        let (size, _mtime) = stat_fast(&path).unwrap().unwrap();
        assert_eq!(size, 5);
    }

    #[test]
    fn a_failed_write_does_not_destroy_the_previous_file() {
        // Writing over a directory must fail without taking the old content with it.
        let dir = tmp();
        let path = dir.path().join("target");
        write_atomic(&path, b"original", false).unwrap();

        let blocked = dir.path().join("blocked");
        create_dir_all(&blocked).unwrap();
        assert!(write_atomic(&blocked, b"nope", false).is_err());
        assert_eq!(read(&path).unwrap(), b"original");
    }

    #[test]
    fn io_error_kind_is_preserved_for_callers_that_branch_on_it() {
        let dir = tmp();
        let err = read(&dir.path().join("missing")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }
}
