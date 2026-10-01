//! The server directories hopper has installed on this machine, so a new one can be given ports
//! none of them use, running or not.
//!
//! A plain list of absolute paths in the data directory. Entries whose directory no longer has
//! a hopper lockfile are skipped and dropped on the next write, so a deleted server frees its
//! ports without anyone having to tell hopper.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use hopper_core::fs as hfs;
use hopper_core::server::{ports, properties};

fn registry() -> Result<PathBuf> {
    Ok(crate::runtime::data_dir()?.join("instances"))
}

fn live(dir: &Path) -> bool {
    dir.join(".hopper/lock.json").is_file()
}

fn read() -> Vec<PathBuf> {
    let Ok(path) = registry() else {
        return Vec::new();
    };
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Remember `dir`. Failing to is not worth failing an install over; it only weakens the next
/// port choice, so the caller may ignore the error.
pub fn register(dir: &Path) -> Result<()> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut all: Vec<PathBuf> = read().into_iter().filter(|d| live(d)).collect();
    if !all.contains(&dir) {
        all.push(dir);
    }
    let mut text = String::new();
    for d in &all {
        text.push_str(&d.to_string_lossy());
        text.push('\n');
    }
    let path = registry()?;
    if let Some(parent) = path.parent() {
        hfs::create_dir_all(parent)?;
    }
    hfs::write_atomic(&path, text.as_bytes(), false).context("recording the server directory")?;
    Ok(())
}

/// Ports configured by every other server hopper knows about.
pub fn claimed_by_others(dir: &Path) -> BTreeSet<u16> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    read()
        .into_iter()
        .filter(|d| *d != dir && live(d))
        .filter_map(|d| std::fs::read_to_string(d.join("server.properties")).ok())
        .flat_map(|text| ports::claimed(&text))
        .collect()
}

/// Whether nothing is listening on `port` right now.
///
/// Minecraft listens on every interface by default, so the port must be bindable on IPv4 and,
/// where the machine has IPv6, on IPv6 too. A machine without IPv6 must not make every port
/// look busy, so only "in use" counts against it there.
pub fn port_free(port: u16) -> bool {
    if std::net::TcpListener::bind(("0.0.0.0", port)).is_err() {
        return false;
    }
    match std::net::TcpListener::bind(("::", port)) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::AddrInUse,
    }
}

/// A free game port for a new server in `dir`, searching up from `start`.
pub fn pick_server_port_from(dir: &Path, start: u16) -> u16 {
    ports::pick(start, &claimed_by_others(dir), port_free).unwrap_or(start)
}

/// A free RCON port for the server in `dir`, clear of its own ports too.
pub fn pick_rcon_port(dir: &Path) -> u16 {
    let mut taken = claimed_by_others(dir);
    let own = std::fs::read_to_string(dir.join("server.properties")).unwrap_or_default();
    taken.extend(ports::claimed(&own));
    // Its own RCON port, if one were set, is not in the way of itself.
    if let Some(p) = properties::get(&own, "rcon.port").and_then(|p| p.parse().ok()) {
        taken.remove(&p);
    }
    ports::pick(ports::DEFAULT_RCON_PORT, &taken, port_free).unwrap_or(ports::DEFAULT_RCON_PORT)
}
