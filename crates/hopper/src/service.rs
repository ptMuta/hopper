//! Scope-aware service discovery, graceful stop and RCON console helpers.
//! and talking to it over RCON.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use hopper_core::fs as hfs;
use hopper_core::server::properties;
use hopper_core::server::rcon::Rcon;

use crate::cli::exit;
use std::sync::atomic::{AtomicBool, Ordering};
static SYSTEM_SCOPE: AtomicBool = AtomicBool::new(false);
pub fn set_scope(scope: crate::interface::Scope) {
    SYSTEM_SCOPE.store(scope == crate::interface::Scope::System, Ordering::Relaxed);
}
pub fn scope_args() -> Vec<&'static str> {
    if SYSTEM_SCOPE.load(Ordering::Relaxed) {
        vec![]
    } else {
        vec!["--user"]
    }
}

/// Where systemd looks for the operator's own units.
pub fn unit_dir() -> Result<PathBuf> {
    if SYSTEM_SCOPE.load(Ordering::Relaxed) {
        return Ok(PathBuf::from("/etc/systemd/system"));
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("could not determine the config directory; set HOME")?;
    Ok(base.join("systemd/user"))
}

/// Sidecars are deliberately not cleared by updates: the operator must review
/// the incoming version, merge or reject it, then remove the `.new` file.
pub fn ensure_conflicts_resolved(dir: &Path) -> Result<()> {
    fn visit(root: &Path, dir: &Path, conflicts: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("checking unresolved conflicts in {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if entry.file_name().as_encoded_bytes().ends_with(b".new") {
                conflicts.push(path.strip_prefix(root)?.to_owned());
            } else if entry.file_type()?.is_dir() {
                // Do not follow symlinks out of managed storage or into cycles.
                visit(root, &path, conflicts)?;
            }
        }
        Ok(())
    }
    let mut conflicts = Vec::new();
    visit(dir, dir, &mut conflicts)?;
    conflicts.sort();
    ensure!(
        conflicts.is_empty(),
        "start refused: unresolved .new conflicts:\n{}\nReview each incoming file, merge or deliberately reject its changes, then remove the .new sidecar and start again.",
        conflicts
            .iter()
            .map(|path| format!("  {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );
    Ok(())
}

/// What the server unit's `ExecStop` runs: `stop` over RCON, then wait for the process to exit.
///
/// Always succeeds from systemd's point of view. If RCON cannot be reached, systemd moves on to
/// SIGTERM, which is the right fallback; failing here would only add noise to the journal.
pub fn stop_server(dir: &Path, pid: u32) -> Result<i32> {
    let running = || Path::new(&format!("/proc/{pid}")).exists();
    if !running() {
        return Ok(exit::OK);
    }
    let text = std::fs::read_to_string(dir.join("server.properties")).unwrap_or_default();
    let port: u16 = properties::get(&text, "rcon.port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(25575);
    let password = properties::get(&text, "rcon.password").unwrap_or_default();
    let host = properties::get(&text, "server-ip")
        .filter(|h| !h.is_empty())
        .unwrap_or("127.0.0.1");

    match Rcon::connect((host, port), password).and_then(|mut r| r.command("stop")) {
        Ok(_) => println!("Sent stop over RCON; waiting for the server to save and exit"),
        Err(e) => {
            println!("RCON stop failed ({e}); systemd will send SIGTERM");
            return Ok(exit::OK);
        }
    }
    // Leaves a margin inside the unit's TimeoutStopSec, so SIGTERM still has time to work if
    // the server hangs while stopping.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(150);
    while running() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if running() {
        println!("The server did not exit within 150s; systemd will send SIGTERM");
    }
    Ok(exit::OK)
}

pub struct RconSetup {
    pub port: u16,
    pub changed: bool,
}

pub fn ensure_rcon(dir: &Path) -> Result<RconSetup> {
    let path = dir.join("server.properties");
    let text = std::fs::read_to_string(&path).unwrap_or_default();

    let enabled = properties::get(&text, "enable-rcon") == Some("true");
    let password = properties::get(&text, "rcon.password").filter(|p| !p.is_empty());
    let port = properties::get(&text, "rcon.port").and_then(|p| p.parse::<u16>().ok());
    if let (true, Some(_), Some(port)) = (enabled, password, port) {
        restrict(&path)?;
        return Ok(RconSetup {
            port,
            changed: false,
        });
    }

    let port = port.unwrap_or_else(|| crate::instances::pick_rcon_port(dir));
    let password = match password {
        Some(p) => p.to_owned(),
        None => random_password()?,
    };
    let updated = properties::set(
        &text,
        &[
            ("enable-rcon", "true"),
            ("rcon.port", &port.to_string()),
            ("rcon.password", &password),
        ],
    );
    hfs::write_private_atomic(&path, updated.as_bytes()).context("updating server.properties")?;
    restrict(&path)?;
    Ok(RconSetup {
        port,
        changed: true,
    })
}

/// The RCON password lives in server.properties, so other users must not read it.
fn restrict(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {}", path.display()))?;
    }
    Ok(())
}

fn random_password() -> Result<String> {
    use std::io::Read;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect())
}

/// The hopper service unit that runs the server in `dir`, if one is installed.
///
/// Found by the `WorkingDirectory=` hopper wrote, so a custom `--name` is found too.
pub fn unit_for(dir: &Path) -> Option<String> {
    let dir = dir.canonicalize().ok()?;
    let units = unit_dir().ok()?;
    std::fs::read_dir(units)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            n.starts_with("hopper-") && n.ends_with(".service") && !n.ends_with("-update.service")
        })
        .find(|name| {
            std::fs::read_to_string(unit_dir().unwrap_or_default().join(name))
                .ok()
                .and_then(|text| {
                    text.lines()
                        .find_map(|l| l.strip_prefix("WorkingDirectory="))
                        .map(|d| PathBuf::from(d.replace("%%", "%")))
                })
                .is_some_and(|d| d == dir)
        })
}

/// `systemctl --user show` of one property, trimmed.
pub fn unit_property(unit: &str, property: &str) -> Option<String> {
    let out = Command::new("systemctl")
        .args(scope_args())
        .args(["show", unit, "--property", property, "--value"])
        .output()
        .ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!v.is_empty()).then_some(v)
}

/// Connection details for the server's RCON, from its server.properties.
struct RconTarget {
    host: String,
    port: u16,
    password: String,
}

fn rcon_target(dir: &Path, unit: Option<&str>) -> Result<RconTarget> {
    let text = std::fs::read_to_string(dir.join("server.properties"))
        .with_context(|| format!("reading {}/server.properties", dir.display()))?;
    if properties::get(&text, "enable-rcon") != Some("true") {
        match unit {
            // hopper turned it on, so something else turned it off.
            Some(_) => bail!(
                "RCON is off in server.properties, though Hopper enabled it during setup.\n\
                 Something rewrote the file, often a pack applying its default settings on first\n\
                 start. Restore enable-rcon=true, then restart the managed instance."
            ),
            None => bail!("RCON is off for this server; onboard it as a managed instance first"),
        }
    }
    Ok(RconTarget {
        port: properties::get(&text, "rcon.port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(25575),
        password: properties::get(&text, "rcon.password")
            .unwrap_or_default()
            .to_owned(),
        host: properties::get(&text, "server-ip")
            .filter(|h| !h.is_empty())
            .unwrap_or("127.0.0.1")
            .to_owned(),
    })
}

/// Connect, waiting while the service is still starting: RCON only opens once the world has
/// loaded, which takes minutes for a large pack.
pub fn connect_waiting(dir: &Path) -> Result<Rcon> {
    use hopper_core::server::rcon::RconError;

    let unit = unit_for(dir);
    let target = rcon_target(dir, unit.as_deref())?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15 * 60);
    let mut said = false;
    loop {
        match Rcon::connect((target.host.as_str(), target.port), &target.password) {
            Ok(r) => return Ok(r),
            Err(RconError::Connect(e)) => {
                let state = unit
                    .as_deref()
                    .and_then(|u| unit_property(u, "ActiveState"))
                    .unwrap_or_default();
                let up = state == "active" || state == "activating" || state == "reloading";
                if !up || std::time::Instant::now() > deadline {
                    let how = match &unit {
                        Some(u) => format!(
                            "start it with `systemctl {} start {u}`; logs: `journalctl {} -u {u}`",
                            scope_args().join(" "),
                            scope_args().join(" ")
                        ),
                        None => "start it with `hopper start NAME`".to_owned(),
                    };
                    bail!(
                        "the server is not running ({}:{}: {e}); {how}",
                        target.host,
                        target.port
                    );
                }
                if !said {
                    eprintln!(
                        "Waiting for the server to finish starting (large packs take a few minutes)..."
                    );
                    said = true;
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// `hopper console`: one command from the arguments, or an interactive prompt.
pub fn console(dir: &Path, command: &[String]) -> Result<i32> {
    let mut rcon = connect_waiting(dir)?;
    let (host, port) = {
        let t = rcon_target(dir, None).ok();
        t.map(|t| (t.host, t.port)).unwrap_or_default()
    };

    if !command.is_empty() {
        let out = rcon.command(&command.join(" "))?;
        print_response(&out);
        return Ok(exit::OK);
    }

    use std::io::{BufRead, IsTerminal, Write};
    let interactive = std::io::stdin().is_terminal();
    if interactive {
        println!("Connected to {host}:{port}. Type server commands; Ctrl-D to leave.");
    }
    let stdin = std::io::stdin();
    loop {
        if interactive {
            print!("> ");
            std::io::stdout().flush().ok();
        }
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if interactive && (line == "exit" || line == "quit") {
            break;
        }
        print_response(&rcon.command(line)?);
    }
    Ok(exit::OK)
}

/// Minecraft colours its output with `§x` codes, which mean nothing in a terminal.
fn print_response(text: &str) {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next();
        } else {
            out.push(c);
        }
    }
    if !out.is_empty() {
        println!("{}", out.trim_end());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn conflicts_block_start_until_sidecars_are_removed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("config/nested")).unwrap();
        std::fs::write(root.join("config/nested/tuning.cfg"), b"custom").unwrap();
        std::fs::write(root.join("config/nested/tuning.cfg.new"), b"incoming").unwrap();
        std::fs::write(root.join(".hopper-launch.sh.new"), b"launcher").unwrap();
        let error = super::ensure_conflicts_resolved(root)
            .unwrap_err()
            .to_string();
        assert!(error.contains("config/nested/tuning.cfg.new"));
        assert!(error.contains(".hopper-launch.sh.new"));
        // Merging is not enough until the operator removes the sidecar.
        std::fs::write(root.join("config/nested/tuning.cfg"), b"incoming").unwrap();
        assert!(super::ensure_conflicts_resolved(root).is_err());
        std::fs::remove_file(root.join("config/nested/tuning.cfg.new")).unwrap();
        assert!(super::ensure_conflicts_resolved(root).is_err());
        std::fs::remove_file(root.join(".hopper-launch.sh.new")).unwrap();
        super::ensure_conflicts_resolved(root).unwrap();
    }

    #[test]
    fn conflict_scan_does_not_follow_links_but_blocks_new_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("server");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&root, root.join("cycle")).unwrap();
        super::ensure_conflicts_resolved(&root).unwrap();
        std::os::unix::fs::symlink("missing", root.join("config.new")).unwrap();
        assert!(super::ensure_conflicts_resolved(&root).is_err());
    }

    #[test]
    fn conflict_scan_fails_closed_when_storage_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        assert!(super::ensure_conflicts_resolved(&temp.path().join("missing")).is_err());
    }
    use super::*;

    #[test]
    fn passwords_are_long_and_unambiguous() {
        let p = random_password().unwrap();
        assert_eq!(p.len(), 32);
        assert!(p.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(!p.contains(['0', 'O', '1', 'l', 'I']));
        assert_ne!(p, random_password().unwrap());
    }

    #[test]
    fn rcon_is_enabled_once_and_then_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.properties");
        std::fs::write(&path, "motd=hi\nenable-rcon=false\n").unwrap();

        let first = ensure_rcon(dir.path()).unwrap();
        assert!(first.changed);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("motd=hi\nenable-rcon=true\n"), "{text}");
        let pw = properties::get(&text, "rcon.password").unwrap().to_owned();

        let second = ensure_rcon(dir.path()).unwrap();
        assert!(!second.changed);
        assert_eq!(second.port, first.port);
        let again = std::fs::read_to_string(&path).unwrap();
        assert_eq!(properties::get(&again, "rcon.password"), Some(pw.as_str()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
