//! Elevate the system coordinator before it can ask questions or change state.
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::interface::{Action, App, Scope};

fn required(app: &App, root: bool) -> bool {
    app.scope == Scope::System
        && !root
        && !matches!(
            app.command,
            None | Some(
                Action::Providers
                    | Action::Completions { .. }
                    | Action::Search { .. }
                    | Action::Versions { .. }
                    | Action::SelfUpdate { .. }
                    | Action::Worker { .. }
                    | Action::StartCheck { .. }
                    | Action::StopWorker { .. }
                    | Action::Maintenance { .. }
            )
        )
}

fn find_sudo(candidates: &[&Path]) -> Result<PathBuf> {
    candidates
        .iter()
        .find(|path| {
            path.metadata().is_ok_and(|m| {
                m.is_file() && m.permissions().mode() & 0o111 != 0
            })
        })
        .map(|path| path.to_path_buf())
        .context("system scope requires root privileges, but sudo is unavailable; install sudo or run hopper as root")
}

fn sudo_command(sudo: &Path, executable: &Path, args: &[OsString], interactive: bool) -> Command {
    let mut command = Command::new(sudo);
    if !interactive {
        command.arg("--non-interactive");
    }
    command
        .args([
            "--user=root",
            "--preserve-env=CURSEFORGE_API_KEY,HOPPER_NO_INPUT,HOPPER_ACCESSIBLE,CI,NO_COLOR",
            "--",
        ])
        .arg(executable)
        .args(args);
    command
}

pub fn ensure_privileges(app: &App) -> Result<()> {
    let root = crate::managed::is_root();
    if app.scope == Scope::System && matches!(app.command, Some(Action::Maintenance { .. })) {
        ensure!(root, "system maintenance must run as root");
    }
    if !required(app, root) {
        return Ok(());
    }
    // Use standard system paths, independent of the invoking user's PATH.
    let sudo = find_sudo(&[Path::new("/usr/bin/sudo"), Path::new("/bin/sudo")])?;
    let executable = std::env::current_exe()
        .context("locating hopper for sudo elevation")?
        .canonicalize()
        .context("locating hopper for sudo elevation")?;
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let interactive = crate::prompt::can_ask(app.no_input, app.json, false);
    // exec preserves sudo/the elevated command's exit status and signal handling.
    let error = sudo_command(&sudo, &executable, &args, interactive).exec();
    bail!(
        "could not elevate system scope using sudo: {error}; run hopper as root or grant sudo privileges"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn root_bypasses_elevation_and_workers_remain_unprivileged() {
        for args in [
            vec!["install", "world", "--provider", "gtnh"],
            vec![
                "onboard",
                "world",
                "--provider",
                "gtnh",
                "--from",
                "/tmp/world",
            ],
            vec!["repair", "world"],
            vec!["check", "world"],
            vec!["update", "world"],
            vec!["configure", "world", "--warn", "60"],
            vec!["start", "world"],
            vec!["stop", "world"],
            vec!["restart", "world"],
            vec!["remove", "world"],
            vec!["status", "world"],
            vec!["list"],
            vec!["disable", "world", "mod.jar"],
            vec!["enable", "world", "mod.jar"],
            vec!["show", "world"],
            vec!["chat", "world"],
            vec!["console", "world", "--", "list"],
            vec!["logs", "world", "--", "--since", "1 hour ago"],
        ] {
            let mut argv = vec!["hopper", "--scope", "system"];
            argv.extend(args);
            let app = App::try_parse_from(argv).unwrap();
            assert!(required(&app, false));
            assert!(!required(&app, true));
        }
        for args in [
            vec!["__worker", "world", "install"],
            vec!["__start-check", "--dir", "/tmp/world"],
            vec!["__stop", "--dir", "/tmp/world"],
            vec!["__maintenance", "world", "--update"],
            vec!["providers"],
            vec!["search", "atm10", "--provider", "curseforge"],
            vec!["versions", "--provider", "gtnh"],
            vec!["self-update", "--check"],
            vec!["completions", "bash"],
            vec![],
        ] {
            let mut argv = vec!["hopper", "--scope", "system"];
            argv.extend(args);
            let app = App::try_parse_from(argv).unwrap();
            assert!(!required(&app, false));
        }
        let app = App::try_parse_from(["hopper", "repair", "world"]).unwrap();
        assert!(!required(&app, false));
    }

    #[test]
    fn elevation_preserves_arguments_and_passes_credentials_only_by_environment() {
        let args: Vec<OsString> = ["repair", "atm10", "--scope=system", "--no-input"]
            .into_iter()
            .map(Into::into)
            .collect();
        for interactive in [false, true] {
            let command = sudo_command(
                Path::new("/usr/bin/sudo"),
                Path::new("/tmp/hopper with spaces"),
                &args,
                interactive,
            );
            let actual: Vec<_> = command.get_args().map(|s| s.to_str().unwrap()).collect();
            let mut expected = vec![];
            if !interactive {
                expected.push("--non-interactive");
            }
            expected.extend([
                "--user=root",
                "--preserve-env=CURSEFORGE_API_KEY,HOPPER_NO_INPUT,HOPPER_ACCESSIBLE,CI,NO_COLOR",
                "--",
                "/tmp/hopper with spaces",
                "repair",
                "atm10",
                "--scope=system",
                "--no-input",
            ]);
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn missing_or_nonexecutable_sudo_fails_early() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sudo");
        assert!(
            find_sudo(&[&path])
                .unwrap_err()
                .to_string()
                .contains("sudo is unavailable")
        );
        std::fs::write(&path, b"#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(find_sudo(&[&path]).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_sudo(&[&path]).unwrap(), path);
    }
}
