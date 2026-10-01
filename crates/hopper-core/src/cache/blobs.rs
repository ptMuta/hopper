//! The content-addressed store.
//!
//! Every byte hopper downloads lands here first, keyed by its sha512, and the server directory
//! is only touched afterwards. That ordering buys three things at once:
//!
//! * **Honest confirmation.** Jars can be downloaded and inspected before the operator is asked
//!   to approve anything, so the diff they see is the diff that will be applied.
//! * **Clean failure.** A download that dies half-way leaves the server directory untouched,
//!   so "nothing was changed" is a true statement rather than a hope.
//! * **Sharing.** Several servers on one box, and repeated installs of the same pack, reuse one
//!   copy. Re-running an install after a failure re-downloads nothing.
//!
//! Content here is immutable and verified on the way in, so a blob that exists is a blob that
//! matched its hash.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::fs::{self as hfs, IoPath};
use crate::model::{Digest, HashAlgo, Hashes, MultiHasher};

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error(transparent)]
    Io(#[from] IoPath),
    #[error("content did not match its expected hash: wanted {expected}, got {actual}")]
    HashMismatch { expected: Digest, actual: Digest },
    #[error("expected sha512 for cache addressing, got {0}")]
    NotSha512(HashAlgo),
    #[error("internal error: content was not hashed with {algo}, so it cannot be verified")]
    NotHashed { algo: HashAlgo },
    #[error("content is {actual} bytes but {expected} were declared")]
    SizeMismatch { expected: u64, actual: u64 },
}

/// A verified blob in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub digest: Digest,
    pub path: PathBuf,
    pub size: u64,
    /// Every digest computed while ingesting, so a caller that needs the sha1 (for a registry
    /// lookup) does not have to read the file a second time.
    pub hashes: Hashes,
}

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a digest lives.
    ///
    /// Two levels of fanout keep any one directory to a manageable size; a flat layout becomes
    /// painfully slow to list once a few thousand mods accumulate.
    pub fn path_for(&self, digest: &Digest) -> Result<PathBuf, BlobError> {
        if digest.algo() != HashAlgo::Sha512 {
            return Err(BlobError::NotSha512(digest.algo()));
        }
        let (a, b) = digest.shard();
        Ok(self
            .root
            .join("blobs")
            .join("sha512")
            .join(a)
            .join(b)
            .join(digest.hex()))
    }

    pub fn contains(&self, digest: &Digest) -> bool {
        self.path_for(digest).is_ok_and(|p| p.is_file())
    }

    /// Look up a blob, returning `None` rather than an error when it is simply absent.
    pub fn get(&self, digest: &Digest) -> Result<Option<Blob>, BlobError> {
        let path = self.path_for(digest)?;
        match hfs::stat_fast(&path)? {
            Some((size, _)) => Ok(Some(Blob {
                digest: digest.clone(),
                path,
                size,
                hashes: Hashes::default(),
            })),
            None => Ok(None),
        }
    }

    /// Ingest bytes, verifying them against `expect` when one is given.
    pub fn insert_bytes(&self, bytes: &[u8], expect: Option<&Digest>) -> Result<Blob, BlobError> {
        self.insert_reader(&mut std::io::Cursor::new(bytes), expect, None)
    }

    /// Stream content in, hashing as it goes.
    ///
    /// Verification happens *before* the content is published under its final name, so a
    /// mismatched download never becomes a cache entry that a later run would trust.
    pub fn insert_reader<R: Read>(
        &self,
        reader: &mut R,
        expect: Option<&Digest>,
        expect_size: Option<u64>,
    ) -> Result<Blob, BlobError> {
        let tmp_dir = self.root.join("tmp");
        hfs::create_dir_all(&tmp_dir)?;

        let tmp = tmp_dir.join(format!(
            "incoming-{}-{}",
            std::process::id(),
            next_counter()
        ));

        // The hasher must cover whatever algorithm the caller will verify against, or the
        // comparison below can never succeed. JDK vendors publish sha256, which is not part of
        // the default set.
        let mut hasher = match expect.map(Digest::algo) {
            Some(HashAlgo::Sha256) => MultiHasher::new().with_sha256(),
            _ => MultiHasher::new(),
        };
        let mut size: u64 = 0;
        {
            let mut out =
                std::fs::File::create(&tmp).map_err(|e| IoPath::new("create", &tmp, e))?;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = reader
                    .read(&mut buf)
                    .map_err(|e| IoPath::new("read from source into", &tmp, e))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                size += n as u64;
                out.write_all(&buf[..n])
                    .map_err(|e| IoPath::new("write", &tmp, e))?;
            }
        }

        let (hashes, digest) = hasher.finish();

        let fail = |e: BlobError| {
            let _ = std::fs::remove_file(&tmp);
            e
        };
        if let Some(expected) = expect {
            // Compare on whichever algorithm the caller gave us: Modrinth hands out sha512,
            // Fabric's meta API and Mojang publish sha1, and JDK vendors publish sha256.
            match hashes.get(expected.algo()) {
                Some(observed) if observed == expected => {}
                Some(observed) => {
                    return Err(fail(BlobError::HashMismatch {
                        expected: expected.clone(),
                        actual: observed.clone(),
                    }));
                }
                // Would mean the hasher was not set up for this algorithm, which is a bug
                // rather than a bad download -- say so instead of reporting a mismatch between
                // two different algorithms, which reads as nonsense.
                None => {
                    return Err(fail(BlobError::NotHashed {
                        algo: expected.algo(),
                    }));
                }
            }
        }
        if let Some(expected) = expect_size
            && expected != size
        {
            return Err(fail(BlobError::SizeMismatch {
                expected,
                actual: size,
            }));
        }

        let final_path = self.path_for(&digest)?;
        if let Some(parent) = final_path.parent() {
            hfs::create_dir_all(parent).map_err(|e| fail(e.into()))?;
        }

        // Already stored -- by an earlier run, or a concurrent writer that got there first.
        // The content is identical by construction, so the copy is simply dropped. Checked
        // before syncing: re-staging a large pack whose content is all cached would otherwise
        // pay one fsync per file for data that is thrown away.
        if final_path.is_file() {
            let _ = std::fs::remove_file(&tmp);
        } else {
            std::fs::File::open(&tmp)
                .and_then(|f| f.sync_all())
                .map_err(|e| fail(IoPath::new("sync", &tmp, e).into()))?;
            hfs::rename(&tmp, &final_path).map_err(|e| fail(e.into()))?;
            if let Some(parent) = final_path.parent() {
                hfs::fsync_dir(parent)?;
            }
        }

        // Remember which sha512 a weaker verified digest maps to, so the next fetch by that
        // digest is a cache hit. Only written after verification passed, so an alias never
        // points at content that did not match. Losing one only costs a re-download.
        if let Some(expected) = expect
            && expected.algo() != HashAlgo::Sha512
        {
            let _ = self.write_alias(expected, &digest);
        }

        Ok(Blob {
            digest,
            path: final_path,
            size,
            hashes,
        })
    }

    fn alias_path(&self, weak: &Digest) -> PathBuf {
        let (a, b) = weak.shard();
        self.root
            .join("alias")
            .join(weak.algo().name())
            .join(a)
            .join(b)
            .join(weak.hex())
    }

    fn write_alias(&self, weak: &Digest, strong: &Digest) -> Result<(), BlobError> {
        let path = self.alias_path(weak);
        if let Some(parent) = path.parent() {
            hfs::create_dir_all(parent)?;
        }
        hfs::write_atomic(&path, strong.hex().as_bytes(), false)?;
        Ok(())
    }

    /// Look a blob up by a digest it was verified against, sha1 or sha256 included.
    ///
    /// Registries that publish only sha1 (CurseForge, Mojang) would otherwise miss the cache
    /// on every run, since blobs are addressed by sha512.
    pub fn get_verified(&self, digest: &Digest) -> Result<Option<Blob>, BlobError> {
        if digest.algo() == HashAlgo::Sha512 {
            return self.get(digest);
        }
        let Ok(hex) = std::fs::read_to_string(self.alias_path(digest)) else {
            return Ok(None);
        };
        match Digest::new(HashAlgo::Sha512, hex.trim()) {
            Ok(strong) => self.get(&strong),
            Err(_) => Ok(None),
        }
    }

    /// Copy a blob out to a destination path.
    ///
    /// Deliberately a copy, never a hard link: a hard link would let an in-place editor — which
    /// plenty of config tooling and some Java writers are — corrupt the shared cache entry for
    /// every other server on the machine.
    pub fn materialize(
        &self,
        digest: &Digest,
        dest: &Path,
        executable: bool,
    ) -> Result<(), BlobError> {
        let src = self.path_for(digest)?;
        let bytes = hfs::read(&src)?;
        hfs::write_atomic(dest, &bytes, executable)?;
        Ok(())
    }

    /// Total bytes held, for reporting and for deciding when to collect garbage.
    pub fn size_on_disk(&self) -> Result<u64, BlobError> {
        let blobs = self.root.join("blobs");
        Ok(walk_sizes(&blobs))
    }
}

fn walk_sizes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => walk_sizes(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

fn next_counter() -> u64 {
    static C: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    C.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = BlobStore::new(dir.path().join("cache"));
        (dir, s)
    }

    fn digest_of(bytes: &[u8]) -> Digest {
        let mut h = MultiHasher::new();
        h.update(bytes);
        h.finish().1
    }

    #[test]
    fn round_trips_content() {
        let (_d, s) = store();
        let blob = s.insert_bytes(b"hello world", None).unwrap();
        assert_eq!(blob.size, 11);
        assert_eq!(blob.digest, digest_of(b"hello world"));
        assert!(s.contains(&blob.digest));
        assert_eq!(std::fs::read(&blob.path).unwrap(), b"hello world");
    }

    #[test]
    fn addresses_by_sharded_sha512() {
        let (_d, s) = store();
        let blob = s.insert_bytes(b"x", None).unwrap();
        let hex = blob.digest.hex();
        let p = blob.path.to_string_lossy();
        assert!(
            p.contains(&format!("sha512/{}/{}/", &hex[0..2], &hex[2..4])),
            "got {p}"
        );
        assert!(p.ends_with(hex));
    }

    #[test]
    fn refuses_to_address_by_a_weaker_hash() {
        // sha1 is fine for verifying an upstream's claim, but it is not our identity.
        let (_d, s) = store();
        let sha1 = Digest::new(HashAlgo::Sha1, &"a".repeat(40)).unwrap();
        assert!(matches!(
            s.path_for(&sha1).unwrap_err(),
            BlobError::NotSha512(_)
        ));
    }

    #[test]
    fn verifies_against_an_expected_sha512() {
        let (_d, s) = store();
        let good = digest_of(b"payload");
        assert!(s.insert_bytes(b"payload", Some(&good)).is_ok());
    }

    #[test]
    fn rejects_content_that_does_not_match_its_hash() {
        let (_d, s) = store();
        let wrong = digest_of(b"something else");
        let err = s.insert_bytes(b"payload", Some(&wrong)).unwrap_err();
        assert!(matches!(err, BlobError::HashMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn a_mismatched_download_never_becomes_a_cache_entry() {
        // The property that matters: a later run must not find and trust bad bytes.
        let (_d, s) = store();
        let wrong = digest_of(b"something else");
        let _ = s.insert_bytes(b"payload", Some(&wrong));
        assert!(
            !s.contains(&digest_of(b"payload")),
            "rejected content must not be published"
        );
        // And no debris is left in the staging area.
        let tmp = s.root().join("tmp");
        let leftovers: Vec<_> = std::fs::read_dir(&tmp)
            .map(|d| d.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "left {} temp files", leftovers.len());
    }

    #[test]
    fn verifies_against_a_sha1_when_that_is_all_upstream_published() {
        // Fabric's meta API and Mojang's manifests give sha1 only.
        let (_d, s) = store();
        let mut h = MultiHasher::new();
        h.update(b"payload");
        let (all, _) = h.finish();
        let sha1 = all.get(HashAlgo::Sha1).unwrap().clone();

        let blob = s.insert_bytes(b"payload", Some(&sha1)).unwrap();
        // Stored under sha512 regardless of which algorithm verified it.
        assert_eq!(blob.digest.algo(), HashAlgo::Sha512);

        let bad_sha1 = Digest::new(HashAlgo::Sha1, &"f".repeat(40)).unwrap();
        assert!(s.insert_bytes(b"payload", Some(&bad_sha1)).is_err());
    }

    #[test]
    fn a_verified_sha1_finds_the_blob_again() {
        let (_d, s) = store();
        let sha1 = Digest::new(HashAlgo::Sha1, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        assert_eq!(s.get_verified(&sha1).unwrap(), None);

        let blob = s.insert_bytes(b"hello", Some(&sha1)).unwrap();
        let again = s.get_verified(&sha1).unwrap().expect("aliased");
        assert_eq!(again.digest, blob.digest);
    }

    #[test]
    fn a_failed_verification_leaves_no_alias() {
        let (_d, s) = store();
        let sha1 = Digest::new(HashAlgo::Sha1, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        assert!(s.insert_bytes(b"not hello", Some(&sha1)).is_err());
        assert_eq!(s.get_verified(&sha1).unwrap(), None);
    }

    #[test]
    fn verifies_against_a_sha256_which_is_what_jdk_vendors_publish() {
        // The default hasher does not compute sha256, so this only works because insert_reader
        // widens it based on what is being verified.
        let (_d, s) = store();
        let mut h = MultiHasher::new().with_sha256();
        h.update(b"jdk bytes");
        let (all, _) = h.finish();
        let sha256 = all.get(HashAlgo::Sha256).unwrap().clone();

        let blob = s.insert_bytes(b"jdk bytes", Some(&sha256)).unwrap();
        assert_eq!(
            blob.digest.algo(),
            HashAlgo::Sha512,
            "still addressed by sha512"
        );

        let wrong = Digest::new(HashAlgo::Sha256, &"c".repeat(64)).unwrap();
        assert!(matches!(
            s.insert_bytes(b"jdk bytes", Some(&wrong)).unwrap_err(),
            BlobError::HashMismatch { .. }
        ));
    }

    #[test]
    fn rejects_a_size_that_contradicts_the_manifest() {
        let (_d, s) = store();
        let err = s
            .insert_reader(&mut std::io::Cursor::new(b"1234"), None, Some(999))
            .unwrap_err();
        assert!(matches!(err, BlobError::SizeMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn inserting_the_same_content_twice_is_idempotent() {
        let (_d, s) = store();
        let a = s.insert_bytes(b"same", None).unwrap();
        let b = s.insert_bytes(b"same", None).unwrap();
        assert_eq!(a.digest, b.digest);
        assert_eq!(a.path, b.path);
        assert_eq!(s.size_on_disk().unwrap(), 4, "stored once, not twice");
    }

    #[test]
    fn exposes_every_digest_computed_on_the_way_in() {
        // So a caller needing sha1 for a registry lookup does not re-read the file.
        let (_d, s) = store();
        let blob = s.insert_bytes(b"content", None).unwrap();
        assert!(blob.hashes.get(HashAlgo::Sha1).is_some());
        assert_eq!(blob.hashes.get(HashAlgo::Sha512), Some(&blob.digest));
    }

    #[test]
    fn materialize_writes_the_content_out() {
        let (d, s) = store();
        let blob = s.insert_bytes(b"jar bytes", None).unwrap();
        let dest = d.path().join("server/mods/a.jar");
        s.materialize(&blob.digest, &dest, false).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"jar bytes");
    }

    #[cfg(unix)]
    #[test]
    fn materialize_does_not_link_the_cache_into_the_server_directory() {
        // A hard link would let an in-place edit corrupt the shared cache for every server on
        // the machine.
        use std::os::unix::fs::MetadataExt;
        let (d, s) = store();
        let blob = s.insert_bytes(b"shared", None).unwrap();
        let dest = d.path().join("server/mods/a.jar");
        s.materialize(&blob.digest, &dest, false).unwrap();

        let cached = std::fs::metadata(&blob.path).unwrap();
        let placed = std::fs::metadata(&dest).unwrap();
        assert_ne!(cached.ino(), placed.ino(), "must be a copy, not a link");
        assert_eq!(cached.nlink(), 1);
    }

    #[test]
    fn materialize_can_set_the_executable_bit() {
        let (d, s) = store();
        let blob = s.insert_bytes(b"#!/bin/sh\n", None).unwrap();
        let dest = d.path().join("start.sh");
        s.materialize(&blob.digest, &dest, true).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111);
        }
    }

    #[test]
    fn missing_content_reads_as_absent_rather_than_failing() {
        let (_d, s) = store();
        let absent = digest_of(b"never stored");
        assert!(!s.contains(&absent));
        assert_eq!(s.get(&absent).unwrap(), None);
        assert!(s.materialize(&absent, Path::new("/tmp/x"), false).is_err());
    }

    #[test]
    fn handles_content_larger_than_the_read_buffer() {
        let (_d, s) = store();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        let blob = s.insert_bytes(&data, None).unwrap();
        assert_eq!(blob.size, data.len() as u64);
        assert_eq!(blob.digest, digest_of(&data));
    }

    #[test]
    fn empty_content_is_storable() {
        let (_d, s) = store();
        let blob = s.insert_bytes(b"", None).unwrap();
        assert_eq!(blob.size, 0);
        assert!(s.contains(&blob.digest));
    }
}
