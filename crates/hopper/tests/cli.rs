//! End-to-end tests against the built binary.
//!
//! The core crate's tests cover each stage; these cover the wiring between them, which is where
//! a whole class of bug lives that unit tests cannot see. The one that prompted this file:
//! override files were installed correctly but never recorded in the lockfile, so nothing could
//! update or remove them afterwards — every component behaved, and the product did not.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_hopper");

struct Server {
    dir: tempfile::TempDir,
}

impl Server {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("server")
    }

    fn cache(&self) -> PathBuf {
        self.dir.path().join("cache")
    }

    /// Install a pack. Always `--mods-only`, so the suite never touches the network: the
    /// loader, server jar and JVM all come from remote metadata.
    fn install(&self, pack: &Path, extra: &[&str]) -> (String, String, i32) {
        let mut args = vec![pack.to_str().unwrap(), "--mods-only"];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    fn run(&self, args: &[&str]) -> (String, String, i32) {
        let out = Command::new(BIN)
            .args(args)
            .arg("--dir")
            .arg(self.root())
            .env("HOPPER_CACHE_DIR", self.cache())
            .output()
            .expect("running hopper");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code().unwrap_or(-1),
        )
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.root().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.root().join(rel)).ok()
    }

    fn exists(&self, rel: &str) -> bool {
        self.root().join(rel).exists()
    }
}

/// Build a `.mrpack` with no downloadable entries, so the tests need no network.
fn build_pack(path: &Path, version: &str, overrides: &[(&str, &str)]) {
    let index = serde_json::json!({
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": version,
        "name": "Test Pack",
        "files": [],
        "dependencies": { "minecraft": "26.3", "fabric-loader": "0.17.2" },
    });

    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("modrinth.index.json", opts).unwrap();
    zip.write_all(index.to_string().as_bytes()).unwrap();
    for (name, content) in overrides {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn installs_a_pack_and_records_every_file_it_wrote() {
    // The regression this file exists for: files installed but not tracked are invisible to
    // every later update.
    let s = Server::new();
    let pack = s.dir.path().join("v1.mrpack");
    build_pack(
        &pack,
        "1.0.0",
        &[
            ("overrides/config/a.toml", "setting=default\n"),
            ("server-overrides/config/server.toml", "tick=20\n"),
        ],
    );

    let (out, err, code) = s.install(&pack, &["--yes", "--eula"]);
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(s.exists("config/a.toml"));
    assert!(s.exists("config/server.toml"));

    let (status, _, _) = s.run(&["status"]);
    assert!(
        status.contains("2 managed by hopper"),
        "every installed file must be tracked:\n{status}"
    );
}

#[test]
fn client_only_content_never_reaches_the_server() {
    let s = Server::new();
    let pack = s.dir.path().join("v1.mrpack");
    build_pack(
        &pack,
        "1.0.0",
        &[
            ("overrides/config/a.toml", "x=1\n"),
            // Never applied to a server, and there is no flag to change that.
            ("client-overrides/options.txt", "fov:90\n"),
            // Recognised as client-only from the path alone.
            ("overrides/shaderpacks/pretty.zip", "shader\n"),
            ("overrides/resourcepacks/pack.zip", "textures\n"),
        ],
    );

    let (out, err, code) = s.install(&pack, &["--yes"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(s.exists("config/a.toml"));
    assert!(!s.exists("options.txt"), "client-overrides must be dropped");
    assert!(!s.exists("shaderpacks/pretty.zip"));
    assert!(!s.exists("resourcepacks/pack.zip"));
}

#[test]
fn an_update_preserves_operator_work_and_removes_what_the_pack_dropped() {
    let s = Server::new();
    let v1 = s.dir.path().join("v1.mrpack");
    build_pack(
        &v1,
        "1.0.0",
        &[
            ("overrides/config/a.toml", "setting=default\n"),
            ("overrides/config/gone.toml", "bye\n"),
        ],
    );
    s.install(&v1, &["--yes", "--eula"]);

    // The operator adds a mod and tunes a config.
    s.write("mods/my-plugin.jar", "mine");
    s.write("config/a.toml", "setting=tuned-by-me\n");

    let v2 = s.dir.path().join("v2.mrpack");
    build_pack(
        &v2,
        "2.0.0",
        &[("overrides/config/a.toml", "setting=NEW\n")],
    );
    let (out, err, code) = s.install(&v2, &["--yes"]);
    assert_eq!(code, 0, "{out}{err}");

    assert_eq!(s.read("mods/my-plugin.jar").as_deref(), Some("mine"));
    assert_eq!(
        s.read("config/a.toml").as_deref(),
        Some("setting=tuned-by-me\n"),
        "their edit must win"
    );
    assert_eq!(
        s.read("config/a.toml.new").as_deref(),
        Some("setting=NEW\n"),
        "and the pack's version should be there to compare"
    );
    assert!(
        !s.exists("config/gone.toml"),
        "a file the pack dropped must actually go"
    );
    assert!(
        out.contains("files added by you, untouched"),
        "the safety promise should be restated:\n{out}"
    );
}

#[test]
fn a_second_identical_run_does_nothing() {
    let s = Server::new();
    let pack = s.dir.path().join("v1.mrpack");
    build_pack(&pack, "1.0.0", &[("overrides/config/a.toml", "x=1\n")]);

    s.install(&pack, &["--yes", "--eula"]);
    let (out, _, code) = s.install(&pack, &["--yes"]);
    assert_eq!(code, 0);
    assert!(out.contains("Already up to date"), "got:\n{out}");
}

#[test]
fn dry_run_reports_pending_changes_without_applying_them() {
    let s = Server::new();
    let pack = s.dir.path().join("v1.mrpack");
    build_pack(&pack, "1.0.0", &[("overrides/config/a.toml", "x=1\n")]);

    let (out, _, code) = s.install(&pack, &["-n"]);
    assert_eq!(
        code, 10,
        "pending changes exit 10 so cron can branch:\n{out}"
    );
    assert!(!s.exists("config/a.toml"), "--dry-run must not write");

    // And nothing at all once it is settled.
    s.install(&pack, &["--yes"]);
    let (_, _, code) = s.install(&pack, &["-n"]);
    assert_eq!(code, 0, "a settled directory exits 0");
}

#[test]
fn a_bare_invocation_replays_the_recorded_source() {
    let s = Server::new();
    let pack = s.dir.path().join("v1.mrpack");
    build_pack(&pack, "1.0.0", &[("overrides/config/a.toml", "x=1\n")]);
    s.install(&pack, &["--yes", "--eula"]);

    // No pack argument: it should remember what this directory was installed from, and
    // that it was a mods-only install -- otherwise a loader and a JVM would appear.
    let (out, err, code) = s.run(&["--yes"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("Already up to date"), "got:\n{out}{err}");
}

#[test]
fn an_empty_directory_with_no_argument_explains_what_to_do() {
    let s = Server::new();
    std::fs::create_dir_all(s.root()).unwrap();
    let (_, err, code) = s.run(&[]);
    assert_ne!(code, 0);
    assert!(err.contains("hopper <pack>"), "got:\n{err}");
}

#[test]
fn yes_does_not_accept_the_eula() {
    // A "don't prompt me" flag must not agree to a licence agreement.
    let s = Server::new();
    let pack = s.dir.path().join("v1.mrpack");
    build_pack(&pack, "1.0.0", &[("overrides/config/a.toml", "x=1\n")]);

    let (out, _, _) = s.install(&pack, &["--yes"]);
    assert!(
        !s.exists("eula.txt"),
        "--yes must not write eula.txt:\n{out}"
    );
    assert!(out.contains("EULA has not been accepted"), "got:\n{out}");

    let (_, _, _) = s.install(&pack, &["--yes", "--eula"]);
    assert!(s.read("eula.txt").unwrap().contains("eula=true"));
}

#[test]
fn status_on_an_unmanaged_directory_says_so_plainly() {
    let s = Server::new();
    std::fs::create_dir_all(s.root()).unwrap();
    let (out, _, code) = s.run(&["status"]);
    assert_eq!(code, 0);
    assert!(out.contains("No pack is installed"), "got:\n{out}");
}

#[test]
fn pointing_at_a_mod_page_names_the_mistake() {
    let s = Server::new();
    std::fs::create_dir_all(s.root()).unwrap();
    let (_, err, code) = s.run(&["https://modrinth.com/mod/sodium"]);
    assert_ne!(code, 0);
    assert!(err.contains("not a modpack"), "got:\n{err}");
}

#[test]
fn a_hostile_pack_is_refused_with_a_security_exit_code() {
    // Path traversal in an override entry, which would otherwise write outside the directory.
    let s = Server::new();
    let pack = s.dir.path().join("evil.mrpack");
    build_pack(
        &pack,
        "1.0.0",
        &[("overrides/../../../../tmp/hopper-pwned", "payload\n")],
    );

    let (_, err, code) = s.install(&pack, &["--yes"]);
    assert_eq!(code, 5, "security refusals get their own exit code:\n{err}");
    assert!(!Path::new("/tmp/hopper-pwned").exists());
}
