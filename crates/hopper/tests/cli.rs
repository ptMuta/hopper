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
