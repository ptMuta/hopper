//! `hopper service` and `hopper console`: running an installed server under systemd --user,
//! and talking to it over RCON.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use hopper_core::fs as hfs;
use hopper_core::server::properties;
use hopper_core::server::rcon::Rcon;
use hopper_core::server::systemd::{Schedule, ServerUnits, unit_stem};

use crate::cli::exit;

/// Where systemd looks for the operator's own units.
fn unit_dir() -> Result<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("could not determine the config directory; set HOME")?;
    Ok(base.join("systemd/user"))
}

/// Holds secrets the update unit needs, read through `EnvironmentFile=`.
fn env_file() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/hopper/env"))
}

fn systemctl(args: &[&str]) -> Result<std::process::ExitStatus> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("running systemctl --user")
}

fn server_dir(dir: &Path) -> Result<PathBuf> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("{} does not exist", dir.display()))?;
    if !dir.join("start.sh").is_file() {
        bail!(
            "{} has no start.sh; install a pack there first (without --mods-only)",
            dir.display()
        );
    }
    Ok(dir)
}

fn units_for<'a>(
    dir: &'a Path,
    stem: &'a str,
    title: &'a str,
    hopper: &'a Path,
) -> ServerUnits<'a> {
    ServerUnits {
        stem,
        title,
        dir,
        hopper,
    }
}

fn stem_for(dir: &Path, name: Option<&str>) -> Result<String> {
    let base = match name {
        Some(n) => n.to_owned(),
        None => dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    Ok(unit_stem(&base)?)
}

pub struct InstallOptions<'a> {
    pub dir: &'a Path,
    pub name: Option<&'a str>,
    /// `None` keeps whatever this installation had; `Some("off")` removes the timer.
    pub update: Option<&'a str>,
    pub now: bool,
    pub quiet: bool,
}

pub fn install(opts: &InstallOptions<'_>) -> Result<i32> {
    let dir = server_dir(opts.dir)?;
    let lock = crate::read_lockfile(&dir)
        .context("reading the lockfile")?
        .context("hopper did not install this directory; run `hopper <pack>` there first")?;
    let stem = stem_for(&dir, opts.name)?;
    let hopper = std::env::current_exe().context("locating the hopper binary")?;
    let hopper = hopper.canonicalize().unwrap_or(hopper);
    let units = units_for(&dir, &stem, &lock.pack.name, &hopper);

    let schedule = match opts.update {
        Some(s) => Some(Schedule::parse(s)?),
        None => None,
    };
    if let Some(Some(Schedule::Calendar(expr))) = &schedule {
        // Only systemd knows its own calendar grammar.
        let ok = Command::new("systemd-analyze")
            .args(["calendar", expr])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            bail!("systemd does not accept {expr:?} as an OnCalendar schedule");
        }
    }

    // 1. RCON, so `hopper console` can reach a server that has no terminal.
    let _ = crate::instances::register(&dir);
    let rcon = ensure_rcon(&dir)?;

    // 2. Units.
    let unit_dir = unit_dir()?;
    hfs::create_dir_all(&unit_dir)?;
    hfs::write_atomic(
        &unit_dir.join(units.service_name()),
        units.server_service()?.as_bytes(),
        false,
    )?;
    let timer_path = unit_dir.join(units.timer_name());
    let update_path = unit_dir.join(units.update_service_name());
    match &schedule {
        Some(Some(s)) => {
            hfs::write_atomic(&update_path, units.update_service()?.as_bytes(), false)?;
            hfs::write_atomic(&timer_path, units.update_timer(s).as_bytes(), false)?;
        }
        Some(None) => {
            let _ = systemctl(&["disable", "--now", &units.timer_name()]);
            let _ = std::fs::remove_file(&timer_path);
            let _ = std::fs::remove_file(&update_path);
        }
        // Unchanged: an installation keeps its schedule until told otherwise. The update unit
        // is still refreshed, since the hopper binary may have moved.
        None if timer_path.exists() => {
            hfs::write_atomic(&update_path, units.update_service()?.as_bytes(), false)?;
        }
        None => {}
    }
    let has_timer = timer_path.exists();

    // 3. CurseForge packs need the API key when the timer updates them unattended.
    if has_timer && lock.pack.registry == Some(hopper_core::model::RegistryId::CurseForge) {
        store_curseforge_key(opts.quiet)?;
    }

    // 4. Tell systemd.
    let mut systemd_ok = systemctl(&["daemon-reload"]).is_ok_and(|s| s.success());
    if systemd_ok {
        systemd_ok &= systemctl(&["enable", &units.service_name()]).is_ok_and(|s| s.success());
        if has_timer {
            systemd_ok &=
                systemctl(&["enable", "--now", &units.timer_name()]).is_ok_and(|s| s.success());
        }
        if opts.now {
            // Restart rather than start, so changed units and RCON settings take effect.
            systemd_ok &= systemctl(&["restart", &units.service_name()]).is_ok_and(|s| s.success());
        }
    }

    if opts.quiet {
        return Ok(if systemd_ok { exit::OK } else { exit::GENERIC });
    }
    println!("Wrote {}", unit_dir.join(units.service_name()).display());
    if has_timer {
        let cal = std::fs::read_to_string(&timer_path)
            .ok()
            .and_then(|t| {
                t.lines()
                    .find_map(|l| l.strip_prefix("OnCalendar=").map(str::to_owned))
            })
            .unwrap_or_default();
        println!("Wrote {} (updates: {cal})", timer_path.display());
    }
    if rcon.changed {
        println!(
            "Enabled RCON on port {} in server.properties (now readable only by you)",
            rcon.port
        );
        println!(
            "  note: RCON listens on every interface unless server-ip is set. Keep port {}",
            rcon.port
        );
        println!("        closed in your firewall; hopper console uses it locally.");
        if !opts.now {
            println!("  A running server picks this up on its next restart.");
        }
    }
    if !systemd_ok {
        println!("\nsystemctl --user did not finish cleanly. Once a user session is available:");
        println!("  systemctl --user daemon-reload");
        println!("  systemctl --user enable --now {}", units.service_name());
    }
    if !lingering() {
        println!("\n  note: user services stop when you log out and do not start at boot");
        println!("        unless lingering is on:  loginctl enable-linger");
    }
    let svc = units.service_name();
    println!("\n  Start:    systemctl --user start {svc}");
    println!("  Stop:     systemctl --user stop {svc}");
    println!("  Logs:     journalctl --user -u {svc} -f");
    println!("  Console:  hopper console --dir {}", dir.display());
    Ok(if systemd_ok { exit::OK } else { exit::GENERIC })
}

pub fn remove(dir: &Path, name: Option<&str>, quiet: bool) -> Result<i32> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let stem = stem_for(&dir, name)?;
    let hopper = PathBuf::from("hopper");
    let units = units_for(&dir, &stem, "", &hopper);
    let unit_dir = unit_dir()?;

    let _ = systemctl(&["disable", "--now", &units.timer_name()]);
    let _ = systemctl(&["disable", "--now", &units.service_name()]);
    let mut removed = 0;
    for name in [
        units.service_name(),
        units.update_service_name(),
        units.timer_name(),
    ] {
        if std::fs::remove_file(unit_dir.join(&name)).is_ok() {
            removed += 1;
        }
    }
    let _ = systemctl(&["daemon-reload"]);
    if !quiet {
        if removed == 0 {
            println!("No hopper units named {stem} were installed.");
        } else {
            println!("Stopped and removed {stem}. The server directory is untouched.");
        }
    }
    Ok(exit::OK)
}

/// What the update timer runs: check, and only if something changed, stop, update and start.
pub fn run_update(dir: &Path, unit: &str) -> Result<i32> {
    let hopper = std::env::current_exe().context("locating the hopper binary")?;
    let check = Command::new(&hopper)
        .args(["--dry-run", "--quiet", "--dir"])
        .arg(dir)
        .status()
        .context("checking for updates")?;
    match check.code() {
        Some(c) if c == exit::OK => {
            println!("{} is up to date", dir.display());
            return Ok(exit::OK);
        }
        Some(c) if c == exit::CHANGES_PENDING => {}
        // A failed check changes nothing; the server keeps running.
        other => return Ok(other.unwrap_or(exit::GENERIC)),
    }

    // Only a server that was running is started again; one the operator stopped stays down.
    let was_running = systemctl(&["is-active", "--quiet", unit]).is_ok_and(|s| s.success());
    if was_running {
        println!("Stopping {unit} to update");
        systemctl(&["stop", unit])?;
    }
    let update = Command::new(&hopper)
        .args(["--yes", "--dir"])
        .arg(dir)
        .status()
        .context("updating")?;
    // Started again whether or not the update worked: a failed update leaves the directory as
    // it was, and a server that is down helps nobody.
    if was_running {
        println!("Starting {unit}");
        systemctl(&["start", unit])?;
    }
    Ok(update.code().unwrap_or(exit::GENERIC))
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

struct RconSetup {
    port: u16,
    changed: bool,
}

fn ensure_rcon(dir: &Path) -> Result<RconSetup> {
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
    hfs::write_atomic(&path, updated.as_bytes(), false).context("updating server.properties")?;
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

fn lingering() -> bool {
    let Ok(user) = std::env::var("USER") else {
        return true;
    };
    Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger", "--value"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
        // No loginctl means no way to tell; do not nag.
        .unwrap_or(true)
}

/// Make the API key available to the update unit, which does not inherit this shell.
fn store_curseforge_key(quiet: bool) -> Result<()> {
    let path = env_file()?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.starts_with(&format!("{}=", crate::curseforge::KEY_ENV)))
    {
        return Ok(());
    }
    let Some(key) = crate::curseforge::api_key(None) else {
        if !quiet {
            println!(
                "  note: scheduled updates of a CurseForge pack need {}; add it to {}",
                crate::curseforge::KEY_ENV,
                path.display()
            );
        }
        return Ok(());
    };
    if key.contains(['\n', '\r']) {
        bail!("{} contains a line break", crate::curseforge::KEY_ENV);
    }
    // systemd's EnvironmentFile treats quotes and backslashes specially; single quotes keep
    // the key literal, and a key never contains one.
    let line = format!("{}='{}'\n", crate::curseforge::KEY_ENV, key);
    if let Some(parent) = path.parent() {
        hfs::create_dir_all(parent)?;
    }
    hfs::write_atomic(&path, format!("{existing}{line}").as_bytes(), false)?;
    restrict(&path)?;
    if !quiet {
        println!(
            "Stored {} in {} (readable only by you) for scheduled updates",
            crate::curseforge::KEY_ENV,
            path.display()
        );
    }
    Ok(())
}

/// `hopper console`: one command from the arguments, or an interactive prompt.
pub fn console(dir: &Path, command: &[String]) -> Result<i32> {
    let text = std::fs::read_to_string(dir.join("server.properties"))
        .with_context(|| format!("reading {}/server.properties", dir.display()))?;
    if properties::get(&text, "enable-rcon") != Some("true") {
        bail!("RCON is off for this server; `hopper service install` turns it on");
    }
    let port: u16 = properties::get(&text, "rcon.port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(25575);
    let password = properties::get(&text, "rcon.password").unwrap_or_default();
    let host = properties::get(&text, "server-ip")
        .filter(|h| !h.is_empty())
        .unwrap_or("127.0.0.1");

    let mut rcon = Rcon::connect((host, port), password)
        .with_context(|| format!("connecting to {host}:{port}; is the server running?"))?;

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
