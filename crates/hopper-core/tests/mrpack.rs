//! `.mrpack` parsing, and the adversarial corpus it has to survive.
//!
//! Everything in a pack is attacker-controlled: packs travel as URLs, and even a pack from the
//! official CDN was uploaded by a stranger. These tests are the regression suite for that.

use std::io::{Cursor, Write};

use hopper_core::model::HashAlgo;
use hopper_core::net::HostAllowlist;
use hopper_core::source::mrpack::{self, MrpackError, MrpackIndex, OverrideKind};
use hopper_core::source::{EnvSupport, FileEnv};
use zip::write::SimpleFileOptions;

const SHA512_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA1_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn allow() -> HostAllowlist {
    HostAllowlist::packs()
}

/// A minimal valid index, which each test then perturbs.
fn index_json(files: serde_json::Value, deps: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": "1.0.0",
        "name": "Test Pack",
        "files": files,
        "dependencies": deps,
    }))
    .unwrap()
}

fn one_file(path: &str, downloads: serde_json::Value) -> serde_json::Value {
    serde_json::json!([{
        "path": path,
        "hashes": { "sha512": SHA512_A, "sha1": SHA1_A },
        "downloads": downloads,
        "fileSize": 1234,
    }])
}

fn fabric_deps() -> serde_json::Value {
    serde_json::json!({ "minecraft": "26.3", "fabric-loader": "0.17.2" })
}

fn cdn(name: &str) -> serde_json::Value {
    serde_json::json!([format!("https://cdn.modrinth.com/data/X/versions/Y/{name}")])
}

// ---------------------------------------------------------------- happy path

#[test]
fn parses_a_well_formed_pack() {
    let bytes = index_json(
        one_file("mods/sodium.jar", cdn("sodium.jar")),
        fabric_deps(),
    );
    let idx = MrpackIndex::parse(&bytes, &allow()).unwrap();

    assert_eq!(idx.name, "Test Pack");
    assert_eq!(idx.minecraft.as_str(), "26.3");
    assert_eq!(idx.loader, hopper_core::model::LoaderKind::Fabric);
    assert_eq!(idx.loader_version.as_deref(), Some("0.17.2"));
    assert_eq!(idx.files.len(), 1);
    assert_eq!(idx.files[0].path.as_str(), "mods/sodium.jar");
    assert_eq!(idx.files[0].size, Some(1234));
    assert_eq!(
        idx.files[0].hashes.get(HashAlgo::Sha512).unwrap().hex(),
        SHA512_A
    );
}

#[test]
fn handles_the_year_based_minecraft_versions() {
    // The 2026 scheme. Nothing may parse or compare these as numbers.
    for v in ["26.3", "26.1", "1.21.1", "1.7.10"] {
        let deps = serde_json::json!({ "minecraft": v, "fabric-loader": "0.17.2" });
        let bytes = index_json(one_file("mods/a.jar", cdn("a.jar")), deps);
        let idx = MrpackIndex::parse(&bytes, &allow()).unwrap();
        assert_eq!(idx.minecraft.as_str(), v);
    }
}

#[test]
fn recognises_every_loader_key_spelling() {
    // Fabric and Quilt carry `-loader`; Forge and NeoForge do not. Guessing breaks silently.
    for (key, want) in [
        ("fabric-loader", hopper_core::model::LoaderKind::Fabric),
        ("quilt-loader", hopper_core::model::LoaderKind::Quilt),
        ("neoforge", hopper_core::model::LoaderKind::NeoForge),
        ("forge", hopper_core::model::LoaderKind::Forge),
    ] {
        let deps = serde_json::json!({ "minecraft": "26.3", key: "1.2.3" });
        let bytes = index_json(one_file("mods/a.jar", cdn("a.jar")), deps);
        assert_eq!(MrpackIndex::parse(&bytes, &allow()).unwrap().loader, want);
    }
}

#[test]
fn a_pack_with_no_loader_is_vanilla() {
    let deps = serde_json::json!({ "minecraft": "26.3" });
    let bytes = index_json(one_file("mods/a.jar", cdn("a.jar")), deps);
    let idx = MrpackIndex::parse(&bytes, &allow()).unwrap();
    assert_eq!(idx.loader, hopper_core::model::LoaderKind::Vanilla);
    assert_eq!(idx.loader_version, None);
}

#[test]
fn files_are_sorted_so_parsing_is_deterministic() {
    let files = serde_json::json!([
        { "path": "mods/z.jar", "hashes": {"sha512": SHA512_A}, "downloads": ["https://cdn.modrinth.com/z.jar"] },
        { "path": "config/a.toml", "hashes": {"sha512": SHA512_A}, "downloads": ["https://cdn.modrinth.com/a.toml"] },
    ]);
    let idx = MrpackIndex::parse(&index_json(files, fabric_deps()), &allow()).unwrap();
    let paths: Vec<&str> = idx.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["config/a.toml", "mods/z.jar"]);
}

// ---------------------------------------------------------------- env metadata

#[test]
fn env_absent_means_unknown_not_yes() {
    // The single most important reading of this field. A missing `env` block is extremely
    // common and must not be mistaken for a positive server assertion.
    let bytes = index_json(one_file("mods/a.jar", cdn("a.jar")), fabric_deps());
    let idx = MrpackIndex::parse(&bytes, &allow()).unwrap();
    assert_eq!(idx.files[0].env, None);
    assert_eq!(
        idx.files[0].wanted_on_server(),
        None,
        "absence is unknown, so classification must decide"
    );
}

#[test]
fn env_server_unsupported_is_an_explicit_no() {
    let files = serde_json::json!([{
        "path": "mods/iris.jar",
        "hashes": { "sha512": SHA512_A },
        "downloads": ["https://cdn.modrinth.com/iris.jar"],
        "env": { "client": "required", "server": "unsupported" },
    }]);
    let idx = MrpackIndex::parse(&index_json(files, fabric_deps()), &allow()).unwrap();
    assert_eq!(
        idx.files[0].env,
        Some(FileEnv {
            client: EnvSupport::Required,
            server: EnvSupport::Unsupported
        })
    );
    assert_eq!(idx.files[0].wanted_on_server(), Some(false));
}

#[test]
fn an_env_value_hopper_does_not_know_says_nothing_rather_than_breaking_the_pack() {
    // Current Modrinth packs write "unknown". Refusing the whole pack over one value would make
    // it uninstallable; reading it as yes or no would be a guess.
    let files = serde_json::json!([{
        "path": "mods/a.jar",
        "hashes": { "sha512": SHA512_A },
        "downloads": ["https://cdn.modrinth.com/a.jar"],
        "env": { "client": "required", "server": "unknown" },
    }]);
    let idx = MrpackIndex::parse(&index_json(files, fabric_deps()), &allow()).unwrap();
    assert_eq!(idx.files[0].wanted_on_server(), None);
    assert!(!idx.files[0].is_optional_on_server());
}

#[test]
fn env_server_optional_is_wanted_but_flagged_optional() {
    let files = serde_json::json!([{
        "path": "mods/a.jar",
        "hashes": { "sha512": SHA512_A },
        "downloads": ["https://cdn.modrinth.com/a.jar"],
        "env": { "client": "required", "server": "optional" },
    }]);
    let idx = MrpackIndex::parse(&index_json(files, fabric_deps()), &allow()).unwrap();
    assert_eq!(idx.files[0].wanted_on_server(), Some(true));
    assert!(idx.files[0].is_optional_on_server());
}

// ---------------------------------------------------------------- path traversal

#[test]
fn rejects_every_traversal_shape() {
    for path in [
        "../../../../etc/passwd",
        "..",
        "mods/../../etc/shadow",
        "/etc/passwd",
        "C:/windows/system32/cmd.exe",
        "..\\..\\windows\\system32",
        "mods/a.jar:stream",
        "mods/./a.jar",
        "mods//a.jar",
    ] {
        let bytes = index_json(one_file(path, cdn("x.jar")), fabric_deps());
        let err = MrpackIndex::parse(&bytes, &allow())
            .unwrap_err_or_panic(&format!("path {path:?} must be refused"));
        assert!(
            matches!(err, MrpackError::BadPath { .. }),
            "{path:?} gave {err:?}"
        );
    }
}

/// Small helper so the loop above reads cleanly.
trait UnwrapErrOrPanic<T, E> {
    fn unwrap_err_or_panic(self, msg: &str) -> E;
}
impl<T: std::fmt::Debug, E> UnwrapErrOrPanic<T, E> for Result<T, E> {
    fn unwrap_err_or_panic(self, msg: &str) -> E {
        match self {
            Ok(v) => panic!("{msg}, but it parsed as {v:?}"),
            Err(e) => e,
        }
    }
}

// ---------------------------------------------------------------- download URLs

#[test]
fn rejects_downloads_from_unlisted_hosts() {
    let bytes = index_json(
        one_file(
            "mods/a.jar",
            serde_json::json!(["https://evil.test/payload.jar"]),
        ),
        fabric_deps(),
    );
    let err = MrpackIndex::parse(&bytes, &allow()).unwrap_err();
    assert!(matches!(err, MrpackError::BadUrl { .. }), "got {err:?}");
}

#[test]
fn rejects_plaintext_downloads() {
    let bytes = index_json(
        one_file(
            "mods/a.jar",
            serde_json::json!(["http://cdn.modrinth.com/a.jar"]),
        ),
        fabric_deps(),
    );
    assert!(MrpackIndex::parse(&bytes, &allow()).is_err());
}

#[test]
fn rejects_a_lookalike_host_using_credentials() {
    let bytes = index_json(
        one_file(
            "mods/a.jar",
            serde_json::json!(["https://cdn.modrinth.com@evil.test/a.jar"]),
        ),
        fabric_deps(),
    );
    assert!(MrpackIndex::parse(&bytes, &allow()).is_err());
}

#[test]
fn one_bad_mirror_rejects_the_whole_entry() {
    // Falling back to a good mirror would still have fetched from the bad one first.
    let bytes = index_json(
        one_file(
            "mods/a.jar",
            serde_json::json!(["https://cdn.modrinth.com/a.jar", "https://evil.test/a.jar"]),
        ),
        fabric_deps(),
    );
    assert!(MrpackIndex::parse(&bytes, &allow()).is_err());
}

#[test]
fn rejects_an_entry_with_no_downloads() {
    let bytes = index_json(one_file("mods/a.jar", serde_json::json!([])), fabric_deps());
    assert!(matches!(
        MrpackIndex::parse(&bytes, &allow()).unwrap_err(),
        MrpackError::NoDownloads { .. }
    ));
}

// ---------------------------------------------------------------- hashes

#[test]
fn requires_sha512_because_it_is_our_content_identity() {
    let files = serde_json::json!([{
        "path": "mods/a.jar",
        "hashes": { "sha1": SHA1_A },
        "downloads": ["https://cdn.modrinth.com/a.jar"],
    }]);
    assert!(matches!(
        MrpackIndex::parse(&index_json(files, fabric_deps()), &allow()).unwrap_err(),
        MrpackError::MissingHash { .. }
    ));
}

#[test]
fn rejects_a_malformed_hash() {
    let files = serde_json::json!([{
        "path": "mods/a.jar",
        "hashes": { "sha512": "nonsense" },
        "downloads": ["https://cdn.modrinth.com/a.jar"],
    }]);
    assert!(matches!(
        MrpackIndex::parse(&index_json(files, fabric_deps()), &allow()).unwrap_err(),
        MrpackError::BadHash { .. }
    ));
}

// ---------------------------------------------------------------- manifest shape

#[test]
fn refuses_a_newer_format_version_rather_than_guessing() {
    let mut v: serde_json::Value = serde_json::from_slice(&index_json(
        one_file("mods/a.jar", cdn("a.jar")),
        fabric_deps(),
    ))
    .unwrap();
    v["formatVersion"] = serde_json::json!(2);
    let err = MrpackIndex::parse(v.to_string().as_bytes(), &allow()).unwrap_err();
    assert!(matches!(err, MrpackError::UnsupportedFormat { .. }));
    assert!(err.to_string().contains("upgrade hopper"));
}

#[test]
fn refuses_a_pack_for_another_game() {
    let mut v: serde_json::Value = serde_json::from_slice(&index_json(
        one_file("mods/a.jar", cdn("a.jar")),
        fabric_deps(),
    ))
    .unwrap();
    v["game"] = serde_json::json!("terraria");
    assert!(matches!(
        MrpackIndex::parse(v.to_string().as_bytes(), &allow()).unwrap_err(),
        MrpackError::NotMinecraft(_)
    ));
}

#[test]
fn refuses_an_unknown_dependency_instead_of_ignoring_it() {
    // Ignoring it would install a vanilla server for a modded pack.
    let deps = serde_json::json!({ "minecraft": "26.3", "babric-loader": "1.0" });
    let bytes = index_json(one_file("mods/a.jar", cdn("a.jar")), deps);
    assert!(matches!(
        MrpackIndex::parse(&bytes, &allow()).unwrap_err(),
        MrpackError::UnknownDependency(_)
    ));
}

#[test]
fn refuses_two_loaders() {
    let deps = serde_json::json!({ "minecraft": "26.3", "fabric-loader": "1", "neoforge": "2" });
    let bytes = index_json(one_file("mods/a.jar", cdn("a.jar")), deps);
    assert!(matches!(
        MrpackIndex::parse(&bytes, &allow()).unwrap_err(),
        MrpackError::MultipleLoaders(_)
    ));
}

#[test]
fn refuses_a_pack_with_no_minecraft_version() {
    let deps = serde_json::json!({ "fabric-loader": "0.17.2" });
    let bytes = index_json(one_file("mods/a.jar", cdn("a.jar")), deps);
    assert!(matches!(
        MrpackIndex::parse(&bytes, &allow()).unwrap_err(),
        MrpackError::NoMinecraftVersion
    ));
}

#[test]
fn refuses_a_duplicated_path() {
    let files = serde_json::json!([
        { "path": "mods/a.jar", "hashes": {"sha512": SHA512_A}, "downloads": ["https://cdn.modrinth.com/a.jar"] },
        { "path": "mods/a.jar", "hashes": {"sha512": SHA512_A}, "downloads": ["https://cdn.modrinth.com/b.jar"] },
    ]);
    assert!(matches!(
        MrpackIndex::parse(&index_json(files, fabric_deps()), &allow()).unwrap_err(),
        MrpackError::DuplicatePath { .. }
    ));
}

#[test]
fn rejects_non_json() {
    assert!(matches!(
        MrpackIndex::parse(b"this is not json", &allow()).unwrap_err(),
        MrpackError::BadJson(_)
    ));
}

// ---------------------------------------------------------------- the archive

fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
        for (name, data) in entries {
            w.start_file(*name, SimpleFileOptions::default()).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }
    buf
}

#[test]
fn reads_an_archive_and_layers_overrides() {
    let idx = index_json(one_file("mods/a.jar", cdn("a.jar")), fabric_deps());
    let zip = build_zip(&[
        ("modrinth.index.json", &idx),
        ("overrides/config/common.toml", b"shared"),
        ("server-overrides/config/common.toml", b"server wins"),
        ("server-overrides/config/server-only.toml", b"x"),
        ("client-overrides/options.txt", b"client only"),
    ]);

    let pack = mrpack::read(Cursor::new(zip), &allow()).unwrap();
    assert_eq!(pack.index.name, "Test Pack");

    // client-overrides is dropped entirely — there is no flag to bring it back.
    assert!(
        pack.overrides
            .iter()
            .all(|o| o.kind != OverrideKind::Client),
        "client overrides must never reach a server"
    );

    // Both layers of the shared path survive, ordered so the server layer applies last.
    let common: Vec<OverrideKind> = pack
        .overrides
        .iter()
        .filter(|o| o.path.as_str() == "config/common.toml")
        .map(|o| o.kind)
        .collect();
    assert_eq!(common, [OverrideKind::Global, OverrideKind::Server]);
}

#[test]
fn override_paths_have_the_prefix_stripped() {
    let idx = index_json(one_file("mods/a.jar", cdn("a.jar")), fabric_deps());
    let zip = build_zip(&[
        ("modrinth.index.json", &idx),
        ("overrides/config/x.toml", b"x"),
    ]);
    let pack = mrpack::read(Cursor::new(zip), &allow()).unwrap();
    assert_eq!(pack.overrides[0].path.as_str(), "config/x.toml");
    assert_eq!(pack.overrides[0].zip_name, "overrides/config/x.toml");
}

#[test]
fn an_archive_without_an_index_is_refused() {
    let zip = build_zip(&[("overrides/config/x.toml", b"x")]);
    assert!(matches!(
        mrpack::read(Cursor::new(zip), &allow()).unwrap_err(),
        MrpackError::MissingIndex
    ));
}

#[test]
fn a_traversing_override_entry_is_refused() {
    // The classic archive-extraction vulnerability, in the override tree rather than the index.
    let idx = index_json(one_file("mods/a.jar", cdn("a.jar")), fabric_deps());
    let zip = build_zip(&[
        ("modrinth.index.json", &idx),
        ("overrides/../../../etc/cron.d/pwn", b"* * * * * root sh"),
    ]);
    let err = mrpack::read(Cursor::new(zip), &allow()).unwrap_err();
    assert!(matches!(err, MrpackError::BadPath { .. }), "got {err:?}");
}

#[test]
fn garbage_is_not_mistaken_for_an_archive() {
    assert!(matches!(
        mrpack::read(Cursor::new(b"not a zip".to_vec()), &allow()).unwrap_err(),
        MrpackError::Archive(_)
    ));
}

#[test]
fn a_pack_with_no_overrides_is_fine() {
    let idx = index_json(one_file("mods/a.jar", cdn("a.jar")), fabric_deps());
    let zip = build_zip(&[("modrinth.index.json", &idx)]);
    let pack = mrpack::read(Cursor::new(zip), &allow()).unwrap();
    assert!(pack.overrides.is_empty());
    assert_eq!(pack.index.files.len(), 1);
}
