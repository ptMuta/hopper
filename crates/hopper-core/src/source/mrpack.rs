//! Reading `.mrpack` archives.
//!
//! A `.mrpack` is a ZIP holding `modrinth.index.json` plus optional `overrides/`,
//! `server-overrides/` and `client-overrides/` trees. Everything in it is attacker-controlled
//! as far as this module is concerned — the pack may come from a URL, and even a pack from the
//! official CDN was uploaded by a stranger. So every field is validated rather than trusted:
//! entry paths through [`RelPath`], download URLs through the [`HostAllowlist`], and the archive
//! itself against decompression limits.

use std::collections::BTreeMap;
use std::io::{Read, Seek};

use serde::Deserialize;
use url::Url;

use crate::model::{Digest, HashAlgo, Hashes, LoaderKind, MinecraftVersion, RelPath};
use crate::net::{HostAllowlist, HostError};

/// The only `formatVersion` defined so far. A newer pack is refused rather than guessed at.
pub const SUPPORTED_FORMAT_VERSION: u32 = 1;

/// Decompression guards. A ZIP can claim a few kilobytes and expand to gigabytes, so the
/// limits are enforced while reading rather than from the header, which lies.
pub const MAX_INDEX_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 100_000;

#[derive(Debug, thiserror::Error)]
pub enum MrpackError {
    #[error("not a valid .mrpack archive: {0}")]
    Archive(String),
    #[error("archive has no modrinth.index.json at its root")]
    MissingIndex,
    #[error("modrinth.index.json is not valid JSON: {0}")]
    BadJson(String),
    #[error(
        "this pack uses .mrpack format version {found}, but this hopper understands {supported}; upgrade hopper"
    )]
    UnsupportedFormat { found: u32, supported: u32 },
    #[error("pack is for game {0:?}, not minecraft")]
    NotMinecraft(String),
    #[error("pack declares no Minecraft version")]
    NoMinecraftVersion,
    #[error("pack declares more than one mod loader: {0}")]
    MultipleLoaders(String),
    #[error("pack declares unknown dependency {0:?}")]
    UnknownDependency(String),
    #[error("entry {path:?}: {source}")]
    BadPath {
        path: String,
        #[source]
        source: crate::model::PathError,
    },
    #[error("entry {path:?} lists no download URLs")]
    NoDownloads { path: String },
    #[error("entry {path:?} has an unusable download URL {url:?}: {source}")]
    BadUrl {
        path: String,
        url: String,
        #[source]
        source: HostError,
    },
    #[error("entry {path:?} is missing a {algo} hash")]
    MissingHash { path: String, algo: HashAlgo },
    #[error("entry {path:?} has a malformed hash: {source}")]
    BadHash {
        path: String,
        #[source]
        source: crate::model::HashError,
    },
    #[error("archive declares {0} entries, more than the {MAX_ENTRIES} allowed")]
    TooManyEntries(usize),
    #[error("archive entry {path:?} expands to more than {limit} bytes")]
    EntryTooLarge { path: String, limit: u64 },
    #[error("archive expands to more than {0} bytes in total")]
    TotalTooLarge(u64),
    #[error("archive entry {0:?} is a symlink; refusing to extract")]
    SymlinkEntry(String),
    #[error("pack lists {path:?} twice")]
    DuplicatePath { path: String },
    #[error("io error reading archive: {0}")]
    Io(String),
}

/// Whether a file is needed on a given side. `.mrpack`'s `env` block is optional, so absence is
/// common and must not be read as either a yes or a no.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvSupport {
    Required,
    Optional,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct FileEnv {
    pub client: EnvSupport,
    pub server: EnvSupport,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireIndex {
    format_version: u32,
    game: String,
    #[serde(default)]
    version_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    files: Vec<WireFile>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireFile {
    path: String,
    #[serde(default)]
    hashes: BTreeMap<String, String>,
    #[serde(default)]
    env: Option<FileEnv>,
    #[serde(default)]
    downloads: Vec<String>,
    #[serde(default)]
    file_size: Option<u64>,
}

/// One validated entry from the pack index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexFile {
    pub path: RelPath,
    pub hashes: Hashes,
    /// Mirrors, in the order the pack listed them. Every one has passed the allowlist.
    pub downloads: Vec<Url>,
    pub env: Option<FileEnv>,
    pub size: Option<u64>,
}

impl IndexFile {
    /// Whether a dedicated server needs this file.
    ///
    /// `None` when the pack said nothing — genuinely unknown, not a yes. Callers must route
    /// that through environment classification rather than defaulting either way here.
    pub fn wanted_on_server(&self) -> Option<bool> {
        self.env.map(|e| e.server != EnvSupport::Unsupported)
    }

    pub fn is_optional_on_server(&self) -> bool {
        self.env.is_some_and(|e| e.server == EnvSupport::Optional)
    }
}

/// A parsed, validated `modrinth.index.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MrpackIndex {
    pub name: String,
    pub version_id: String,
    pub summary: Option<String>,
    pub minecraft: MinecraftVersion,
    pub loader: LoaderKind,
    /// `None` for vanilla, where there is no loader version to speak of.
    pub loader_version: Option<String>,
    pub files: Vec<IndexFile>,
}

impl MrpackIndex {
    /// Parse and validate the index.
    ///
    /// `allow` is passed in rather than constructed here so an operator can permit a mirror or
    /// a self-hosted Modrinth instance without this module knowing about it.
    pub fn parse(bytes: &[u8], allow: &HostAllowlist) -> Result<Self, MrpackError> {
        let wire: WireIndex =
            serde_json::from_slice(bytes).map_err(|e| MrpackError::BadJson(e.to_string()))?;

        if wire.format_version > SUPPORTED_FORMAT_VERSION {
            return Err(MrpackError::UnsupportedFormat {
                found: wire.format_version,
                supported: SUPPORTED_FORMAT_VERSION,
            });
        }
        if wire.game != "minecraft" {
            return Err(MrpackError::NotMinecraft(wire.game));
        }

        let (minecraft, loader, loader_version) = parse_dependencies(&wire.dependencies)?;

        let mut files = Vec::with_capacity(wire.files.len());
        let mut seen = std::collections::HashSet::new();
        for f in wire.files {
            let file = parse_file(f, allow)?;
            if !seen.insert(file.path.clone()) {
                return Err(MrpackError::DuplicatePath {
                    path: file.path.as_str().to_owned(),
                });
            }
            files.push(file);
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(Self {
            name: wire.name,
            version_id: wire.version_id,
            summary: wire.summary.filter(|s| !s.is_empty()),
            minecraft,
            loader,
            loader_version,
            files,
        })
    }
}

/// Resolve `dependencies` into a Minecraft version and exactly one loader.
///
/// The keys are matched explicitly because their spelling is not uniform — Fabric and Quilt
/// carry a `-loader` suffix while Forge and NeoForge do not. An unrecognised key is an error:
/// silently ignoring one would install a vanilla server for a modded pack.
fn parse_dependencies(
    deps: &BTreeMap<String, String>,
) -> Result<(MinecraftVersion, LoaderKind, Option<String>), MrpackError> {
    let mc = deps
        .get("minecraft")
        .filter(|v| !v.is_empty())
        .ok_or(MrpackError::NoMinecraftVersion)?;

    let mut found: Vec<(LoaderKind, String)> = Vec::new();
    for (key, version) in deps {
        if key == "minecraft" {
            continue;
        }
        match LoaderKind::from_mrpack_key(key) {
            Some(kind) => found.push((kind, version.clone())),
            None => return Err(MrpackError::UnknownDependency(key.clone())),
        }
    }

    match found.len() {
        0 => Ok((MinecraftVersion::new(mc), LoaderKind::Vanilla, None)),
        1 => {
            let (kind, version) = found.remove(0);
            Ok((MinecraftVersion::new(mc), kind, Some(version)))
        }
        _ => Err(MrpackError::MultipleLoaders(
            found
                .iter()
                .map(|(k, _)| k.name())
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

fn parse_file(f: WireFile, allow: &HostAllowlist) -> Result<IndexFile, MrpackError> {
    let path = RelPath::parse(&f.path).map_err(|source| MrpackError::BadPath {
        path: f.path.clone(),
        source,
    })?;

    // sha512 is required: it is our content identity and the cache key, and a pack that omits
    // it cannot be verified to the standard we hold everything else to.
    let mut hashes = Hashes::default();
    for (algo, want_required) in [(HashAlgo::Sha512, true), (HashAlgo::Sha1, false)] {
        match f.hashes.get(algo.name()) {
            Some(hex) => {
                let d = Digest::new(algo, hex).map_err(|source| MrpackError::BadHash {
                    path: f.path.clone(),
                    source,
                })?;
                hashes.set(d);
            }
            None if want_required => {
                return Err(MrpackError::MissingHash {
                    path: f.path.clone(),
                    algo,
                });
            }
            None => {}
        }
    }

    if f.downloads.is_empty() {
        return Err(MrpackError::NoDownloads {
            path: f.path.clone(),
        });
    }
    let mut downloads = Vec::with_capacity(f.downloads.len());
    for raw in &f.downloads {
        let url = Url::parse(raw).map_err(|_| MrpackError::BadUrl {
            path: f.path.clone(),
            url: raw.clone(),
            source: HostError::NoHost,
        })?;
        allow.check(&url).map_err(|source| MrpackError::BadUrl {
            path: f.path.clone(),
            url: raw.clone(),
            source,
        })?;
        downloads.push(url);
    }

    Ok(IndexFile {
        path,
        hashes,
        downloads,
        env: f.env,
        size: f.file_size,
    })
}

/// Which override tree an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideKind {
    /// `overrides/` — applied to every install.
    Global,
    /// `server-overrides/` — applied after `Global`, wins on conflict.
    Server,
    /// `client-overrides/` — never applied to a server, at all.
    Client,
}

impl OverrideKind {
    fn from_entry(name: &str) -> Option<(Self, &str)> {
        for (prefix, kind) in [
            ("overrides/", Self::Global),
            ("server-overrides/", Self::Server),
            ("client-overrides/", Self::Client),
        ] {
            if let Some(rest) = name.strip_prefix(prefix) {
                return Some((kind, rest));
            }
        }
        None
    }
}

/// An override file found inside the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideEntry {
    /// Destination path, with the `overrides/` prefix stripped.
    pub path: RelPath,
    pub kind: OverrideKind,
    /// Name of the entry inside the ZIP, for extraction.
    pub zip_name: String,
    pub size: u64,
}

/// The contents of a `.mrpack`, validated.
#[derive(Debug, Clone)]
pub struct Mrpack {
    pub index: MrpackIndex,
    /// Server-relevant overrides only. `client-overrides/` is dropped here and has no flag to
    /// bring it back: applying it to a dedicated server is a bug, not a preference.
    pub overrides: Vec<OverrideEntry>,
}

/// Extract override content into a content store, returning each file's digest and size.
///
/// Overrides live inside the archive rather than behind a URL, so they have to be staged into
/// the store before anything can be planned against them — the planner works in digests, and
/// apply materialises from the store. Content is re-validated on the way in, and the
/// decompression limits apply here too, since the sizes a ZIP header declares are a claim
/// rather than a fact.
pub fn stage_overrides<R: Read + Seek>(
    reader: R,
    entries: &[OverrideEntry],
    store: &crate::cache::BlobStore,
) -> Result<std::collections::BTreeMap<RelPath, (crate::model::Digest, u64)>, MrpackError> {
    use std::collections::BTreeMap;

    let mut zip = zip::ZipArchive::new(reader).map_err(|e| MrpackError::Archive(e.to_string()))?;
    let mut out: BTreeMap<RelPath, (crate::model::Digest, u64)> = BTreeMap::new();
    let mut total: u64 = 0;

    // `entries` is ordered global-then-server, so a later insert at the same path replaces an
    // earlier one -- which is exactly how server-overrides is meant to win.
    for entry in entries {
        let mut file = zip
            .by_name(&entry.zip_name)
            .map_err(|e| MrpackError::Archive(e.to_string()))?;

        let mut buf = Vec::new();
        file.by_ref()
            .take(MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|e| MrpackError::Io(e.to_string()))?;
        if buf.len() as u64 > MAX_ENTRY_BYTES {
            return Err(MrpackError::EntryTooLarge {
                path: entry.zip_name.clone(),
                limit: MAX_ENTRY_BYTES,
            });
        }
        total = total.saturating_add(buf.len() as u64);
        if total > MAX_TOTAL_BYTES {
            return Err(MrpackError::TotalTooLarge(MAX_TOTAL_BYTES));
        }

        let blob = store
            .insert_bytes(&buf, None)
            .map_err(|e| MrpackError::Io(e.to_string()))?;
        out.insert(entry.path.clone(), (blob.digest, blob.size));
    }
    Ok(out)
}

/// Read and validate a `.mrpack` archive.
pub fn read<R: Read + Seek>(reader: R, allow: &HostAllowlist) -> Result<Mrpack, MrpackError> {
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| MrpackError::Archive(e.to_string()))?;

    if zip.len() > MAX_ENTRIES {
        return Err(MrpackError::TooManyEntries(zip.len()));
    }

    let index = {
        let mut entry = zip
            .by_name("modrinth.index.json")
            .map_err(|_| MrpackError::MissingIndex)?;
        let mut buf = Vec::new();
        entry
            .by_ref()
            .take(MAX_INDEX_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|e| MrpackError::Io(e.to_string()))?;
        if buf.len() as u64 > MAX_INDEX_BYTES {
            return Err(MrpackError::EntryTooLarge {
                path: "modrinth.index.json".into(),
                limit: MAX_INDEX_BYTES,
            });
        }
        MrpackIndex::parse(&buf, allow)?
    };

    let mut overrides = Vec::new();
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let entry = zip
            .by_index(i)
            .map_err(|e| MrpackError::Archive(e.to_string()))?;
        let name = entry.name().to_owned();

        if entry.is_dir() {
            continue;
        }
        let Some((kind, rest)) = OverrideKind::from_entry(&name) else {
            continue;
        };
        // Never applied to a dedicated server, so there is no reason to even look at it.
        if kind == OverrideKind::Client {
            continue;
        }
        // A symlink inside the archive could point anywhere once extracted. ZIP stores the
        // unix mode in the high bits of the external attributes; S_IFLNK is 0o120000.
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(MrpackError::SymlinkEntry(name));
        }

        let size = entry.size();
        if size > MAX_ENTRY_BYTES {
            return Err(MrpackError::EntryTooLarge {
                path: name,
                limit: MAX_ENTRY_BYTES,
            });
        }
        total = total.saturating_add(size);
        if total > MAX_TOTAL_BYTES {
            return Err(MrpackError::TotalTooLarge(MAX_TOTAL_BYTES));
        }

        let path = RelPath::parse(rest).map_err(|source| MrpackError::BadPath {
            path: name.clone(),
            source,
        })?;
        overrides.push(OverrideEntry {
            path,
            kind,
            zip_name: name,
            size,
        });
    }

    // `overrides/` first, then `server-overrides/`, so the server layer wins a same-path
    // conflict simply by being applied later.
    overrides.sort_by(|a, b| {
        let rank = |k: OverrideKind| match k {
            OverrideKind::Global => 0,
            OverrideKind::Server => 1,
            OverrideKind::Client => 2,
        };
        a.path.cmp(&b.path).then(rank(a.kind).cmp(&rank(b.kind)))
    });

    Ok(Mrpack { index, overrides })
}
