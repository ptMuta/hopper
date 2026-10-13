//! The production CLI has no implicit directory/source engine entry point.
use std::process::{Command, Output};

fn run(args: &[&str], root: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hopper"))
        .args(args)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOPPER_DATA_DIR", root.join("data"))
        .env("HOPPER_CACHE_DIR", root.join("cache"))
        .env_remove("HOPPER_INSTANCE_ROOT")
        .current_dir(root)
        .output()
        .unwrap()
}

#[test]
fn curseforge_missing_key_fails_before_host_or_source_changes() {
    let root = tempfile::tempdir().unwrap();
    for action in ["install", "onboard"] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hopper"));
        cmd.args([
            action,
            "missing-key",
            "--provider",
            "curseforge",
            "--pack",
            "all-the-mods-10",
            "--eula",
        ]);
        if action == "onboard" {
            cmd.args(["--from", root.path().to_str().unwrap()]);
        }
        let result = cmd
            .env("HOME", root.path())
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("HOPPER_DATA_DIR", root.path().join("data"))
            .env("HOPPER_NO_INPUT", "1")
            .env_remove("CURSEFORGE_API_KEY")
            .env_remove("CREDENTIALS_DIRECTORY")
            .env_remove("HOPPER_CREDENTIAL_FILE")
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains("CurseForge requires CURSEFORGE_API_KEY"),
            "{error}"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

fn credential_free_command(args: &[&str], root: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hopper"));
    cmd.args(args)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOPPER_DATA_DIR", root.join("data"))
        .env("HOPPER_INSTANCE_ROOT", root.join("data/servers"))
        .env("HOPPER_NO_INPUT", "1")
        .env_remove("CURSEFORGE_API_KEY")
        .env_remove("CREDENTIALS_DIRECTORY")
        .env_remove("HOPPER_CREDENTIAL_FILE")
        .current_dir(root);
    cmd
}

fn without_curseforge_credentials(args: &[&str], root: &std::path::Path) -> Output {
    credential_free_command(args, root).output().unwrap()
}

#[test]
fn curseforge_search_and_versions_require_key_before_discovery() {
    let root = tempfile::tempdir().unwrap();
    for args in [
        vec!["search", "atm10", "--provider", "curseforge"],
        vec![
            "versions",
            "--provider",
            "curseforge",
            "--pack",
            "all-the-mods-10",
        ],
    ] {
        let result = without_curseforge_credentials(&args, root.path());
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains("CurseForge requires CURSEFORGE_API_KEY"),
            "{error}"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

#[test]
fn curseforge_instance_commands_fail_before_changes_but_local_status_needs_no_key() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("config/hopper/instances");
    std::fs::create_dir_all(&dir).unwrap();
    let record = serde_json::json!({
        "format": 1, "name": "atm10", "scope": "user",
        "root": root.path().join("data/servers/atm10"),
        "source": {"provider": "curseforge", "pack": "all-the-mods-10", "collection": null,
                   "file": null, "url": null, "channel": null, "pack_version": null,
                   "mc": null, "loader": null},
        "runtime": {"java": null, "java_major": null, "java_vendor": null, "jvm_args_file": null,
                    "no_optional": false, "force_include": [], "force_exclude": [],
                    "allow_client_pack": false, "skip_blocked": false},
        "state": "ready", "update": "off", "restart": "off", "warn": 300,
        "started": false, "firewall_port": null, "backup": null, "original": null,
    });
    let path = dir.join("atm10.json");
    let bytes = serde_json::to_vec(&record).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    for args in [
        vec!["repair", "atm10"],
        vec!["check", "atm10"],
        vec!["update", "atm10"],
        vec!["__maintenance", "atm10", "--update"],
        vec!["configure", "atm10", "--update", "daily"],
    ] {
        let result = without_curseforge_credentials(&args, root.path());
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains("CurseForge requires CURSEFORGE_API_KEY"),
            "{args:?}: {error}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert!(!root.path().join("data").exists());
    }
    let result = without_curseforge_credentials(&["status", "atm10", "--json"], root.path());
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    // The worker cannot reach the network with this incomplete fixture. Its
    // coordinator must still rotate the saved key before launching it.
    let key_path = dir.join("atm10.curseforge-key");
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(&key_path, b"old-synthetic-key").unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    for key in ["new-synthetic-key", " "] {
        let result = credential_free_command(&["check", "atm10"], root.path())
            .env("CURSEFORGE_API_KEY", key)
            .output()
            .unwrap();
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(!error.contains("CurseForge requires"), "{error}");
        assert!(!error.contains("synthetic-key"), "{error}");
        assert_eq!(std::fs::read(&key_path).unwrap(), b"new-synthetic-key");
        assert_eq!(
            std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn service_prelaunch_rejects_conflicts_until_explicitly_resolved() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("config")).unwrap();
    let sidecar = root.path().join("config/tuning.cfg.new");
    std::fs::write(&sidecar, b"incoming").unwrap();
    for scope in ["user", "system"] {
        let args = [
            "__start-check",
            "--scope",
            scope,
            "--dir",
            root.path().to_str().unwrap(),
        ];
        let result = run(&args, root.path());
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains("start refused") && error.contains("config/tuning.cfg.new"),
            "{error}"
        );
    }
    std::fs::remove_file(sidecar).unwrap();
    for scope in ["user", "system"] {
        let result = run(
            &[
                "__start-check",
                "--scope",
                scope,
                "--dir",
                root.path().to_str().unwrap(),
            ],
            root.path(),
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn bare_cli_is_help_and_does_not_mutate() {
    let root = tempfile::tempdir().unwrap();
    let result = run(&[], root.path());
    assert!(result.status.success());
    let text = String::from_utf8_lossy(&result.stdout);
    assert!(text.contains("install") && text.contains("onboard") && text.contains("configure"));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn explicit_provider_capabilities_need_no_service_or_network() {
    let root = tempfile::tempdir().unwrap();
    let result = run(&["providers"], root.path());
    assert!(result.status.success());
    let text = String::from_utf8_lossy(&result.stdout);
    for provider in ["modrinth", "curseforge", "gtnh", "mrpack"] {
        assert!(text.contains(provider));
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn legacy_magic_and_optional_service_commands_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    for args in [
        vec!["some-pack"],
        vec!["cf:some-pack"],
        vec!["--dir", "/tmp"],
        vec!["service", "install"],
        vec!["install", "test", "--provider", "gtnh", "--mods-only"],
    ] {
        let result = run(&args, root.path());
        assert_eq!(
            result.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn source_validation_precedes_host_mutation() {
    let root = tempfile::tempdir().unwrap();
    for args in [
        vec!["install", "test", "--provider", "modrinth"],
        vec![
            "install",
            "test",
            "--provider",
            "gtnh",
            "--pack",
            "anything",
        ],
        vec![
            "versions",
            "--provider",
            "mrpack",
            "--file",
            "x",
            "--channel",
            "stable",
        ],
    ] {
        assert!(!run(&args, root.path()).status.success(), "{args:?}");
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn default_scope_does_not_become_system_under_sudo_environment() {
    let root = tempfile::tempdir().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_hopper"))
        .args(["status", "missing"])
        .env("XDG_CONFIG_HOME", root.path())
        .env_remove("HOME")
        .env("SUDO_USER", "operator")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("user scope"));
}
