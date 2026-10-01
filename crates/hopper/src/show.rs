//! `hopper show`: the server at a glance, neofetch style, with the address ready to copy.
//!
//! Live facts (MOTD, players, version) come from a status ping, the same one the multiplayer
//! screen sends, so they are what a player would see. Uptime comes from the systemd unit when
//! there is one. Piped, `--plain` or `NO_COLOR` output drops the art and colour and keeps
//! `key: value` lines, so it can be grepped or pasted.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result};

use hopper_core::server::{ping, properties};

use crate::cli::exit;

struct Field {
    key: &'static str,
    value: String,
}

/// The art, seven rows. A hopper, of course.
const ART: [&str; 7] = [
    "▄▄▄▄▄▄▄▄▄▄▄▄▄▄",
    "█▀▀▀▀▀▀▀▀▀▀▀▀█",
    "█            █",
    "▀█▄        ▄█▀",
    "  ▀█▄▄▄▄▄▄█▀  ",
    "    █▀▀▀▀█    ",
    "     ▀██▀     ",
];

pub fn run(dir: &Path, plain: bool) -> Result<i32> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("{} does not exist", dir.display()))?;
    let lock = crate::read_lockfile(&dir)?
        .context("hopper did not install this directory; run `hopper <pack>` there first")?;
    let props = std::fs::read_to_string(dir.join("server.properties")).unwrap_or_default();
    let port: u16 = properties::get(&props, "server-port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(25565);
    let bind = properties::get(&props, "server-ip").filter(|h| !h.is_empty());

    // Started first and joined late: the lookup overlaps the status ping instead of adding to it.
    let public_lookup = std::thread::spawn(|| {
        hopper_core::net::publicip::lookup(std::time::Duration::from_secs(2))
    });

    let unit = crate::service::unit_for(&dir);
    let state = unit
        .as_deref()
        .and_then(|u| crate::service::unit_property(u, "ActiveState"));
    let status = ping::ping(bind.unwrap_or("127.0.0.1"), port).ok();
    let online = status.is_some();

    let mut fields = Vec::new();
    let mut add = |key, value: String| fields.push(Field { key, value });

    let version = lock.pack.version_label.clone().unwrap_or_default();
    add(
        "Server",
        format!("{} {}", lock.pack.name, version).trim().to_owned(),
    );
    add(
        "Status",
        match (&status, state.as_deref()) {
            (Some(s), _) => {
                let up = unit
                    .as_deref()
                    .and_then(uptime)
                    .map(|u| format!(" · up {u}"))
                    .unwrap_or_default();
                format!("online{up} · {}ms", s.latency.as_millis())
            }
            (None, Some("activating")) | (None, Some("active")) => "starting".to_owned(),
            (None, _) => "offline".to_owned(),
        },
    );
    let motd = status
        .as_ref()
        .map(|s| s.motd.clone())
        .filter(|m| !m.is_empty())
        .or_else(|| properties::get(&props, "motd").map(ping::strip_codes))
        .unwrap_or_default();
    if !motd.is_empty() {
        add("MOTD", motd.replace('\n', " / "));
    }
    let max = properties::get(&props, "max-players").unwrap_or("20");
    add(
        "Players",
        match &status {
            Some(s) if s.sample.is_empty() => format!("{}/{}", s.online, s.max),
            Some(s) => format!("{}/{} ({})", s.online, s.max, s.sample.join(", ")),
            None => format!("-/{max}"),
        },
    );
    let loader = if lock.server.loader_version.is_empty() {
        lock.server.loader.to_string()
    } else {
        format!("{} {}", lock.server.loader, lock.server.loader_version)
    };
    add(
        "Version",
        format!("Minecraft {} · {loader}", lock.server.minecraft),
    );

    // Addresses, each on its own line with nothing after it, so a triple-click copies it. The
    // public one is what players outside the network use, if the port is forwarded to here.
    if let Ok(ips) = public_lookup.join() {
        if let Some(ip) = ips.v4 {
            add("Public", format!("{ip}:{port}"));
        }
        if let Some(ip) = ips.v6 {
            add("Public", format!("[{ip}]:{port}"));
        }
    }
    match bind {
        Some(ip) => add("Address", format!("{ip}:{port}")),
        None => {
            if let Some(ip) = lan_ip() {
                add("Address", format!("{ip}:{port}"));
            }
            if let Some(host) = hostname() {
                add("Host", format!("{host}:{port}"));
            }
        }
    }

    match &unit {
        Some(u) => {
            let schedule = timer_schedule(u);
            add(
                "Service",
                match schedule {
                    Some(s) => format!("{} · updates {s}", u.trim_end_matches(".service")),
                    None => u.trim_end_matches(".service").to_owned(),
                },
            );
        }
        None => add("Service", "none · hopper service install".to_owned()),
    }
    add("Path", dir.display().to_string());

    let color = !plain && std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    if !color {
        for f in &fields {
            println!("{}: {}", f.key.to_ascii_lowercase(), f.value);
        }
        return Ok(if online { exit::OK } else { exit::GENERIC });
    }

    let title = format!(
        "{}@{}",
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        hostname().unwrap_or_else(|| "localhost".into())
    );
    // Green when the server answers, grey when it does not.
    let art_colour = if online { "\x1b[32m" } else { "\x1b[90m" };
    let mut lines: Vec<String> = vec![
        format!("\x1b[1;32m{title}\x1b[0m"),
        "─".repeat(title.chars().count()),
    ];
    for f in &fields {
        lines.push(format!(
            "\x1b[1;36m{:<8}\x1b[0m {}",
            format!("{}:", f.key),
            f.value
        ));
    }
    let rows = lines.len().max(ART.len());
    println!();
    for i in 0..rows {
        let art = ART.get(i).copied().unwrap_or("              ");
        let text = lines.get(i).map(String::as_str).unwrap_or("");
        println!("  {art_colour}{art}\x1b[0m   {text}");
    }
    println!();
    Ok(if online { exit::OK } else { exit::GENERIC })
}

/// How long the unit's server process has been running, e.g. `2h 14m`.
fn uptime(unit: &str) -> Option<String> {
    let out = std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            unit,
            "--property",
            "ExecMainStartTimestamp",
            "--value",
            "--timestamp=unix",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let started: u64 = text.trim().strip_prefix('@')?.parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(human(now.checked_sub(started)?))
}

fn human(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

/// The schedule the unit's update timer was given, if it has one.
fn timer_schedule(unit: &str) -> Option<String> {
    let timer = format!("{}-update.timer", unit.trim_end_matches(".service"));
    let path = crate::service::unit_dir().ok()?.join(timer);
    let text = std::fs::read_to_string(path).ok()?;
    let cal = text.lines().find_map(|l| l.strip_prefix("OnCalendar="))?;
    Some(match cal {
        "*-*-* 04:00" => "daily".to_owned(),
        "Mon *-*-* 04:00" => "weekly".to_owned(),
        other => other.to_owned(),
    })
}

/// The address other machines on the network reach this one by.
///
/// Connecting a UDP socket sends nothing; it only makes the kernel pick the outgoing interface,
/// whose address is the one to share.
fn lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptimes_read_naturally() {
        assert_eq!(human(59), "0m");
        assert_eq!(human(3_660), "1h 1m");
        assert_eq!(human(90_061), "1d 1h");
    }

    #[test]
    fn the_art_is_rectangular() {
        let w = ART[0].chars().count();
        assert!(ART.iter().all(|r| r.chars().count() == w));
    }
}
