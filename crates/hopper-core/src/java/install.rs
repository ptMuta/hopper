//! Unpacking a downloaded JVM.
//!
//! Archive extraction is a classic source of path-traversal vulnerabilities, and JDK tarballs
//! are a particularly awkward case: they legitimately contain symlinks (`bin/java` pointing
//! elsewhere, and the whole `Contents/Home` arrangement on macOS), so a blanket symlink ban is
//! wrong. Instead every entry name and every link target is resolved and checked against the
//! extraction root.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::fs::{self as hfs, IoPath};
use crate::model::RelPath;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error(transparent)]
    Io(#[from] IoPath),
    #[error("archive entry {0:?} would be written outside the extraction directory")]
    Escapes(String),
    #[error("archive entry {entry:?} links to {target:?}, outside the extraction directory")]
    LinkEscapes { entry: String, target: String },
    #[error("archive expands to more than {0} bytes")]
    TooLarge(u64),
    #[error("archive is not readable: {0}")]
    Corrupt(String),
    #[error("no bin/java found in the extracted JVM")]
    NoJavaBinary,
}

/// JDKs are a few hundred megabytes; well beyond that is a decompression bomb.
pub const MAX_EXTRACTED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Check that a relative path stays inside the root once joined.
///
/// `RelPath` already refuses traversal, so this is the second of two independent checks — worth
/// having, because the consequence of being wrong is writing anywhere on the filesystem.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, InstallError> {
    let checked = RelPath::parse(rel).map_err(|_| InstallError::Escapes(rel.to_owned()))?;
    let joined = checked.resolve_under(root);
    if !joined.starts_with(root) {
        return Err(InstallError::Escapes(rel.to_owned()));
    }
    Ok(joined)
}

/// Resolve a link target relative to the link's own directory and confirm it stays inside.
fn link_stays_inside(root: &Path, entry_path: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let base = entry_path.parent().unwrap_or(root);
    let mut resolved = base.to_path_buf();
    for c in target.components() {
        match c {
            Component::ParentDir => {
                if !resolved.pop() {
                    return false;
                }
            }
            Component::CurDir => {}
            other => resolved.push(other),
        }
    }
    resolved.starts_with(root)
}

/// Extract a `.tar.gz` JDK, stripping the single top-level directory vendors wrap them in.
pub fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<(), InstallError> {
    let file = std::fs::File::open(archive).map_err(|e| IoPath::new("open", archive, e))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);

    hfs::create_dir_all(dest)?;

    let mut total: u64 = 0;
    for entry in tar
        .entries()
        .map_err(|e| InstallError::Corrupt(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| InstallError::Corrupt(e.to_string()))?;
        let raw = entry
            .path()
            .map_err(|e| InstallError::Corrupt(e.to_string()))?
            .to_string_lossy()
            .into_owned();

        // Vendors wrap everything in one directory named after the build; strip it so the
        // layout is predictable regardless of vendor and version.
        let Some(stripped) = raw.split_once('/').map(|(_, rest)| rest) else {
            continue;
        };
        if stripped.is_empty() {
            continue;
        }

        let out_path = safe_join(dest, stripped)?;

        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                hfs::create_dir_all(&out_path)?;
            }
            tar::EntryType::Symlink | tar::EntryType::Link => {
                let target = entry
                    .link_name()
                    .map_err(|e| InstallError::Corrupt(e.to_string()))?
                    .ok_or_else(|| InstallError::Corrupt("link with no target".into()))?
                    .into_owned();
                // JDKs legitimately contain symlinks, so the check is on where they point.
                if !link_stays_inside(dest, &out_path, &target) {
                    return Err(InstallError::LinkEscapes {
                        entry: stripped.to_owned(),
                        target: target.to_string_lossy().into_owned(),
                    });
                }
                if let Some(parent) = out_path.parent() {
                    hfs::create_dir_all(parent)?;
                }
                let _ = std::fs::remove_file(&out_path);
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &out_path)
                    .map_err(|e| IoPath::new("create symlink", &out_path, e))?;
            }
            _ => {
                total = total.saturating_add(entry.header().size().unwrap_or(0));
                if total > MAX_EXTRACTED_BYTES {
                    return Err(InstallError::TooLarge(MAX_EXTRACTED_BYTES));
                }
                if let Some(parent) = out_path.parent() {
                    hfs::create_dir_all(parent)?;
                }
                let mut buf = Vec::new();
                entry
                    .read_to_end(&mut buf)
                    .map_err(|e| IoPath::new("read from archive into", &out_path, e))?;
                let executable = entry
                    .header()
                    .mode()
                    .map(|m| m & 0o111 != 0)
                    .unwrap_or(false);
                hfs::write_atomic(&out_path, &buf, executable)?;
            }
        }
    }
    Ok(())
}

/// Locate the `java` binary in an extracted JDK.
///
/// Checks the macOS bundle layout as well as the plain one, so the caller does not have to know
/// which vendor produced the archive.
pub fn find_java_binary(dir: &Path) -> Result<PathBuf, InstallError> {
    for candidate in [
        dir.join("bin/java"),
        dir.join("Contents/Home/bin/java"),
        dir.join("bin/java.exe"),
    ] {
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(InstallError::NoJavaBinary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a tar.gz with a single wrapping directory, like a real JDK archive.
    fn build_archive(entries: &[(&str, &[u8])], links: &[(&str, &str)]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (name, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(&mut header, format!("jdk-21/{name}"), *data)
                    .unwrap();
            }
            for (name, target) in links {
                let mut header = tar::Header::new_gnu();
                header.set_size(0);
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                builder
                    .append_link(&mut header, format!("jdk-21/{name}"), target)
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar_bytes).unwrap();
        gz.finish().unwrap()
    }

    fn extract(bytes: &[u8]) -> (tempfile::TempDir, Result<(), InstallError>) {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("jdk.tar.gz");
        std::fs::write(&archive, bytes).unwrap();
        let dest = dir.path().join("out");
        let r = extract_tar_gz(&archive, &dest);
        (dir, r)
    }

    #[test]
    fn extracts_a_jdk_and_strips_the_wrapping_directory() {
        let bytes = build_archive(
            &[
                ("bin/java", b"#!/bin/sh\n"),
                ("lib/modules", b"module data"),
                ("release", b"JAVA_VERSION=\"21\"\n"),
            ],
            &[],
        );
        let (dir, r) = extract(&bytes);
        r.unwrap();
        let out = dir.path().join("out");
        // `jdk-21/` is gone, so the layout is the same whatever the vendor named it.
        assert!(out.join("bin/java").exists());
        assert!(out.join("lib/modules").exists());
        assert!(!out.join("jdk-21").exists());
    }

    #[test]
    fn finds_the_java_binary() {
        let bytes = build_archive(&[("bin/java", b"#!/bin/sh\n")], &[]);
        let (dir, r) = extract(&bytes);
        r.unwrap();
        let java = find_java_binary(&dir.path().join("out")).unwrap();
        assert!(java.ends_with("bin/java"));
    }

    #[test]
    fn a_jdk_without_a_java_binary_is_an_error() {
        let bytes = build_archive(&[("lib/modules", b"x")], &[]);
        let (dir, r) = extract(&bytes);
        r.unwrap();
        assert!(matches!(
            find_java_binary(&dir.path().join("out")).unwrap_err(),
            InstallError::NoJavaBinary
        ));
    }

    #[cfg(unix)]
    #[test]
    fn legitimate_internal_symlinks_are_preserved() {
        // JDKs really do contain these, so a blanket ban would break extraction.
        let bytes = build_archive(&[("bin/java", b"#!/bin/sh\n")], &[("bin/javaw", "java")]);
        let (dir, r) = extract(&bytes);
        r.unwrap();
        let link = dir.path().join("out/bin/javaw");
        assert!(link.symlink_metadata().unwrap().is_symlink());
    }

    #[test]
    fn a_symlink_escaping_the_extraction_root_is_refused() {
        // The attack a blanket ban would have stopped, caught precisely instead.
        let bytes = build_archive(
            &[("bin/java", b"x")],
            &[("bin/evil", "../../../../etc/passwd")],
        );
        let (_dir, r) = extract(&bytes);
        assert!(
            matches!(r, Err(InstallError::LinkEscapes { .. })),
            "got {r:?}"
        );
    }

    #[test]
    fn an_absolute_symlink_target_is_refused() {
        let bytes = build_archive(&[("bin/java", b"x")], &[("bin/evil", "/etc/passwd")]);
        let (_dir, r) = extract(&bytes);
        assert!(
            matches!(r, Err(InstallError::LinkEscapes { .. })),
            "got {r:?}"
        );
    }

    #[test]
    fn a_traversing_entry_name_is_refused() {
        // Note the `tar` crate will not even build an archive containing `..`, so this is
        // tested at the enforcement point. That is two independent guards -- the crate's and
        // ours -- and ours is the one that has to hold against a hand-crafted archive.
        let root = Path::new("/out");
        for evil in [
            "../../../../tmp/pwned",
            "/etc/passwd",
            "bin/../../../etc/shadow",
        ] {
            assert!(
                safe_join(root, evil).is_err(),
                "{evil:?} must not resolve inside the extraction root"
            );
        }
        // And an ordinary entry still works.
        assert_eq!(
            safe_join(root, "bin/java").unwrap(),
            Path::new("/out/bin/java")
        );
    }

    #[test]
    fn link_resolution_is_relative_to_the_links_own_directory() {
        let root = Path::new("/out");
        // /out/bin/javaw -> java  =>  /out/bin/java, inside.
        assert!(link_stays_inside(
            root,
            Path::new("/out/bin/javaw"),
            Path::new("java")
        ));
        // /out/bin/x -> ../lib/y  =>  /out/lib/y, inside.
        assert!(link_stays_inside(
            root,
            Path::new("/out/bin/x"),
            Path::new("../lib/y")
        ));
        // /out/bin/x -> ../../etc/passwd  =>  outside.
        assert!(!link_stays_inside(
            root,
            Path::new("/out/bin/x"),
            Path::new("../../etc/passwd")
        ));
    }

    #[test]
    fn corrupt_archives_are_reported_not_panicked_on() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("bad.tar.gz");
        std::fs::write(&archive, b"not a gzip stream").unwrap();
        assert!(extract_tar_gz(&archive, &dir.path().join("out")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_bit_survives_extraction() {
        use std::os::unix::fs::PermissionsExt;
        let bytes = build_archive(&[("bin/java", b"#!/bin/sh\n")], &[]);
        let (dir, r) = extract(&bytes);
        r.unwrap();
        let mode = std::fs::metadata(dir.path().join("out/bin/java"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o111,
            0o111,
            "a JVM that is not executable is useless"
        );
    }
}
