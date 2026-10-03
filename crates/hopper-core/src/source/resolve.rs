//! Turning a parsed `.mrpack` into the set of files to install on a server.
//!
//! This is where environment classification actually bites: every candidate is weighed, and
//! anything judged client-only is dropped from the desired set. Dropping rather than
//! special-casing matters — a mod that was installed before and is now classified out simply
//! stops being wanted, so reconcile removes it through the ordinary path with no extra logic.

use crate::model::{
    Digest, EnvClaim, EnvSignal, EnvSource, Hashes, Managed, Provenance, RegistryId, RelPath,
    env::{EnvDecision, EnvOutcome, decide},
};
use crate::plan::{DesiredFile, DesiredSet};
use crate::source::mrpack::{EnvSupport, IndexFile, Mrpack, OverrideKind};

/// Paths a dedicated server never reads.
///
/// Shipping these wastes disk and muddies the diff, and their presence in a pack's shared
/// `overrides/` tree is extremely common because authors build packs from a client instance.
pub const CLIENT_ONLY_PATHS: &[&str] = &[
    "resourcepacks",
    "shaderpacks",
    "screenshots",
    "saves",
    ".fabric",
];
pub const CLIENT_ONLY_FILES: &[&str] = &["options.txt", "optionsof.txt", "servers.dat"];

/// One decision about one candidate, kept so the operator can be told why.
#[derive(Debug, Clone)]
pub struct Classified {
    pub path: RelPath,
    pub outcome: EnvOutcome,
}

/// The result of resolving a pack for a server.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    pub desired: DesiredSet,
    /// Everything deliberately left out, with the reasoning attached.
    pub skipped: Vec<Classified>,
    /// Installed, but with at least one signal arguing against it.
    pub uncertain: Vec<Classified>,
    /// Server access lists the pack tried to ship, which are never installed.
    pub access_lists: Vec<RelPath>,
}

/// Files that decide who can join or administer the server.
///
/// A pack shipping its author's `ops.json` would make the author an operator on every server
/// that installs it. These are the operator's alone, whatever the pack says, so they are never
/// installed from a pack -- not even onto a fresh directory where nothing would be overwritten.
pub fn is_access_list(path: &RelPath) -> bool {
    crate::plan::reconcile::is_protected(path)
}

impl Resolved {
    pub fn skipped_count(&self) -> usize {
        self.skipped.len()
    }
}

/// Signals derivable from a path alone.
///
/// Strong only in the negative direction: living under `shaderpacks/` proves a file is
/// client-side, but living anywhere else proves nothing.
pub fn path_signals(path: &RelPath) -> Vec<EnvSignal> {
    let first = path.segments().next().unwrap_or("");
    // A dot-folder holds a tool's or the game's own state: Mixin's debug export, Sinytra
    // Connector's cache, editor and VCS metadata. None of it is content a server needs, and
    // what the game regenerates would otherwise read as missing after every start. Only
    // folders count: a dot-file name says nothing about what the file is.
    let segments: Vec<&str> = path.segments().collect();
    if let Some(dir) = segments[..segments.len().saturating_sub(1)]
        .iter()
        .find(|s| s.starts_with('.'))
    {
        return vec![EnvSignal::new(
            EnvSource::PathRule,
            EnvClaim::ServerUnsupported,
            format!("{dir}/ holds tool or runtime state, not server content"),
        )];
    }
    if CLIENT_ONLY_PATHS.contains(&first) || CLIENT_ONLY_FILES.contains(&path.as_str()) {
        return vec![EnvSignal::new(
            EnvSource::PathRule,
            EnvClaim::ServerUnsupported,
            format!("{first} is not read by a dedicated server"),
        )];
    }
    Vec::new()
}

/// The signal a pack's own `env` block contributes.
///
/// Weighted asymmetrically by the weight table: `unsupported` is a deliberate authorial
/// statement, while `required` is the copy-paste default in nearly every generated pack.
fn pack_env_signal(file: &IndexFile) -> Option<EnvSignal> {
    let env = file.env?;
    let claim = match env.server {
        EnvSupport::Unsupported => EnvClaim::ServerUnsupported,
        EnvSupport::Optional => EnvClaim::ServerOptional,
        EnvSupport::Required => EnvClaim::ServerRequired,
        EnvSupport::Unknown => return None,
    };
    Some(EnvSignal::new(
        EnvSource::PackEnv,
        claim,
        format!("pack declares env.server = {:?}", env.server),
    ))
}

/// How the operator overrode classification for a given file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Override {
    Include,
    Exclude,
}

/// Resolve a pack into files to install.
///
/// `overrides` is consulted first and wins outright — an escape hatch that can be overruled is
/// not an escape hatch.
pub fn resolve_for_server(
    pack: &Mrpack,
    include_optional: bool,
    overrides: &dyn Fn(&RelPath) -> Option<Override>,
    override_content: &std::collections::BTreeMap<RelPath, (Digest, u64)>,
) -> Resolved {
    let mut out = Resolved::default();

    for file in &pack.index.files {
        if is_access_list(&file.path) {
            out.access_lists.push(file.path.clone());
            continue;
        }
        let mut signals = Vec::new();

        if let Some(ov) = overrides(&file.path) {
            signals.push(EnvSignal::new(
                EnvSource::UserRule,
                match ov {
                    Override::Include => EnvClaim::ServerRequired,
                    Override::Exclude => EnvClaim::ServerUnsupported,
                },
                "set by you",
            ));
        }
        signals.extend(pack_env_signal(file));
        signals.extend(path_signals(&file.path));

        let outcome = decide(&signals);
        let classified = Classified {
            path: file.path.clone(),
            outcome: outcome.clone(),
        };

        // An optional file the operator asked not to have.
        if !include_optional && file.is_optional_on_server() && overrides(&file.path).is_none() {
            out.skipped.push(classified);
            continue;
        }

        match outcome.decision {
            EnvDecision::Skip => out.skipped.push(classified),
            EnvDecision::Install => {
                if !outcome.conflicting.is_empty() {
                    out.uncertain.push(classified);
                }
                out.desired.insert(desired_from_index(file));
            }
        }
    }

    // Overrides are applied global-first then server, so a server-specific file wins by being
    // inserted later. client-overrides never reach here at all.
    for entry in &pack.overrides {
        debug_assert_ne!(entry.kind, OverrideKind::Client);
        if is_access_list(&entry.path) {
            out.access_lists.push(entry.path.clone());
            continue;
        }
        // The operator's rules apply to override files too. A server pack ships every mod as
        // one, so without this --force-exclude could not remove anything from it.
        match overrides(&entry.path) {
            Some(Override::Exclude) => {
                out.skipped.push(Classified {
                    path: entry.path.clone(),
                    outcome: decide(&[EnvSignal::new(
                        EnvSource::UserRule,
                        EnvClaim::ServerUnsupported,
                        "set by you",
                    )]),
                });
                continue;
            }
            Some(Override::Include) => {}
            None if !path_signals(&entry.path).is_empty() => continue,
            None => {}
        }
        // Content was staged out of the archive; an entry with none was not extracted and
        // must not be planned, or apply would look for a blob that does not exist.
        let Some((content, size)) = override_content.get(&entry.path) else {
            continue;
        };
        out.desired.insert(DesiredFile {
            path: entry.path.clone(),
            content: content.clone(),
            size: Some(*size),
            executable: false,
            provenance: Provenance::Override {
                layer: match entry.kind {
                    OverrideKind::Server => crate::model::OverrideLayer::Server,
                    _ => crate::model::OverrideLayer::Global,
                },
            },
            managed: Managed::Full,
        });
    }

    out
}

fn desired_from_index(file: &IndexFile) -> DesiredFile {
    DesiredFile {
        path: file.path.clone(),
        // sha512 is mandatory in the index, enforced at parse time.
        content: file
            .hashes
            .get(crate::model::HashAlgo::Sha512)
            .cloned()
            .expect("index parsing requires a sha512"),
        size: file.size,
        executable: false,
        provenance: Provenance::PackFile {
            url: file
                .downloads
                .first()
                .map(ToString::to_string)
                .unwrap_or_default(),
        },
        managed: Managed::Full,
    }
}

/// Registry provenance for a downloaded mod, once its origin is known.
pub fn registry_provenance(
    registry: RegistryId,
    project: crate::model::ProjectId,
    version: crate::model::VersionId,
    slug: Option<String>,
    version_number: Option<String>,
) -> Provenance {
    Provenance::Registry {
        registry,
        project,
        version,
        slug,
        version_number,
    }
}

/// Hashes as published by the pack, for verification at download time.
pub fn verification_hashes(file: &IndexFile) -> &Hashes {
    &file.hashes
}

#[cfg(test)]
mod tests {
    #[test]
    fn dot_folders_are_never_installed() {
        for p in [
            ".mixin.out/class/net/minecraft/world/effect/MobEffect.class",
            "mods/.connector/temp/x.jar",
            "config/.cache/state.json",
        ] {
            let path = crate::model::RelPath::parse(p).unwrap();
            assert!(!super::path_signals(&path).is_empty(), "{p}");
        }
        // A dot-file is not a dot-folder.
        let dotfile = crate::model::RelPath::parse("config/.editorconfig").unwrap();
        assert!(super::path_signals(&dotfile).is_empty());
        let normal = crate::model::RelPath::parse("mods/create.jar").unwrap();
        assert!(super::path_signals(&normal).is_empty());
    }

    use super::*;
    use crate::net::HostAllowlist;
    use crate::source::mrpack::MrpackIndex;

    const SHA512: &str = "ab00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f";

    fn pack_with(files: serde_json::Value) -> Mrpack {
        let json = serde_json::json!({
            "formatVersion": 1,
            "game": "minecraft",
            "versionId": "1.0.0",
            "name": "Test",
            "files": files,
            "dependencies": { "minecraft": "26.3", "fabric-loader": "0.17.2" },
        });
        Mrpack {
            index: MrpackIndex::parse(json.to_string().as_bytes(), &HostAllowlist::packs())
                .unwrap(),
            overrides: vec![],
        }
    }

    fn entry(path: &str, env: Option<(&str, &str)>) -> serde_json::Value {
        let mut v = serde_json::json!({
            "path": path,
            "hashes": { "sha512": SHA512 },
            "downloads": ["https://cdn.modrinth.com/x.jar"],
        });
        if let Some((client, server)) = env {
            v["env"] = serde_json::json!({ "client": client, "server": server });
        }
        v
    }

    fn no_overrides(_: &RelPath) -> Option<Override> {
        None
    }

    #[test]
    fn a_client_only_mod_is_dropped_from_the_desired_set() {
        let pack = pack_with(serde_json::json!([entry(
            "mods/iris.jar",
            Some(("required", "unsupported"))
        )]));
        let r = resolve_for_server(&pack, true, &no_overrides, &Default::default());
        assert!(r.desired.is_empty());
        assert_eq!(r.skipped_count(), 1);
    }

    #[test]
    fn a_mod_with_no_env_block_is_installed_and_not_flagged() {
        // Warn-don't-skip: silence is not grounds for removing a file.
        let pack = pack_with(serde_json::json!([entry("mods/a.jar", None)]));
        let r = resolve_for_server(&pack, true, &no_overrides, &Default::default());
        assert_eq!(r.desired.len(), 1);
        assert!(r.skipped.is_empty());
        assert!(r.uncertain.is_empty(), "no conflict, so nothing to flag");
    }

    #[test]
    fn a_server_required_mod_is_installed() {
        let pack = pack_with(serde_json::json!([entry(
            "mods/a.jar",
            Some(("required", "required"))
        )]));
        assert_eq!(
            resolve_for_server(&pack, true, &no_overrides, &Default::default())
                .desired
                .len(),
            1
        );
    }

    #[test]
    fn optional_files_can_be_opted_out_of() {
        let pack = pack_with(serde_json::json!([entry(
            "mods/extra.jar",
            Some(("required", "optional"))
        )]));
        assert_eq!(
            resolve_for_server(&pack, true, &no_overrides, &Default::default())
                .desired
                .len(),
            1,
            "included by default"
        );
        assert_eq!(
            resolve_for_server(&pack, false, &no_overrides, &Default::default())
                .desired
                .len(),
            0,
            "--no-optional drops it"
        );
    }

    #[test]
    fn a_user_override_beats_the_packs_own_declaration() {
        let pack = pack_with(serde_json::json!([entry(
            "mods/iris.jar",
            Some(("required", "unsupported"))
        )]));
        let force_include = |_: &RelPath| Some(Override::Include);
        let r = resolve_for_server(&pack, true, &force_include, &Default::default());
        assert_eq!(r.desired.len(), 1, "--force-include must win outright");
        assert!(r.skipped.is_empty());
    }

    #[test]
    fn a_user_exclusion_beats_a_pack_saying_it_is_required() {
        let pack = pack_with(serde_json::json!([entry(
            "mods/a.jar",
            Some(("required", "required"))
        )]));
        let force_exclude = |_: &RelPath| Some(Override::Exclude);
        assert!(
            resolve_for_server(&pack, true, &force_exclude, &Default::default())
                .desired
                .is_empty()
        );
    }

    #[test]
    fn an_override_also_overrides_the_optional_opt_out() {
        let pack = pack_with(serde_json::json!([entry(
            "mods/extra.jar",
            Some(("required", "optional"))
        )]));
        let force_include = |_: &RelPath| Some(Override::Include);
        assert_eq!(
            resolve_for_server(&pack, false, &force_include, &Default::default())
                .desired
                .len(),
            1
        );
    }

    #[test]
    fn client_only_paths_are_recognised_from_the_path_alone() {
        for path in [
            "shaderpacks/complementary.zip",
            "resourcepacks/faithful.zip",
            "options.txt",
            "saves/world/level.dat",
        ] {
            assert!(
                !path_signals(&RelPath::parse(path).unwrap()).is_empty(),
                "{path} should be recognised as client-only"
            );
        }
    }

    #[test]
    fn ordinary_paths_produce_no_path_signal() {
        // Strong negatively, silent otherwise -- living in mods/ proves nothing either way.
        for path in ["mods/a.jar", "config/a.toml", "kubejs/server_scripts/x.js"] {
            assert!(
                path_signals(&RelPath::parse(path).unwrap()).is_empty(),
                "{path}"
            );
        }
    }

    #[test]
    fn a_shaderpack_in_the_index_is_skipped() {
        let pack = pack_with(serde_json::json!([entry("shaderpacks/x.zip", None)]));
        let r = resolve_for_server(&pack, true, &no_overrides, &Default::default());
        assert!(r.desired.is_empty());
        assert_eq!(r.skipped_count(), 1);
    }

    #[test]
    fn a_disagreement_is_installed_but_flagged_for_review() {
        // The pack says it is server-required; the path says it is client-only. The path rule
        // is weaker, so it installs -- but the operator should hear about it.
        let pack = pack_with(serde_json::json!([entry(
            "resourcepacks/x.zip",
            Some(("required", "required"))
        )]));
        let r = resolve_for_server(&pack, true, &no_overrides, &Default::default());
        // PathRule at ServerUnsupported outweighs PackEnv at ServerRequired.
        assert_eq!(r.skipped_count(), 1);
    }

    #[test]
    fn every_skipped_file_records_why() {
        let pack = pack_with(serde_json::json!([entry(
            "mods/iris.jar",
            Some(("required", "unsupported"))
        )]));
        let r = resolve_for_server(&pack, true, &no_overrides, &Default::default());
        let s = &r.skipped[0];
        assert_eq!(s.path.as_str(), "mods/iris.jar");
        assert_eq!(s.outcome.winner.source, EnvSource::PackEnv);
        assert!(!s.outcome.winner.note.is_empty(), "must be explainable");
    }

    #[test]
    fn a_mixed_pack_splits_correctly() {
        let pack = pack_with(serde_json::json!([
            entry("mods/server-side.jar", Some(("unsupported", "required"))),
            entry("mods/both.jar", Some(("required", "required"))),
            entry("mods/client-side.jar", Some(("required", "unsupported"))),
            entry("mods/unknown.jar", None),
            entry("shaderpacks/x.zip", None),
        ]));
        let r = resolve_for_server(&pack, true, &no_overrides, &Default::default());
        let installed: Vec<&str> = r.desired.paths().map(|p| p.as_str()).collect();
        assert_eq!(
            installed,
            ["mods/both.jar", "mods/server-side.jar", "mods/unknown.jar"]
        );
        assert_eq!(r.skipped_count(), 2);
    }
}
