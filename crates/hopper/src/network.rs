//! Explicit server network settings. Only allocation of missing ports probes sockets.
use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use hopper_core::server::{ports, properties};
use serde::{Deserialize, Serialize};

use crate::managed::{self, Instance};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Change {
    pub game: Option<u16>,
    pub rcon: Option<u16>,
    pub address: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Settings {
    pub game: u16,
    pub rcon: u16,
    pub address: String,
}

pub fn parse_address(value: &str) -> Result<String, String> {
    match value {
        "" | "*" => Ok(String::new()),
        "localhost" => Ok("127.0.0.1".into()),
        _ => value
            .parse::<IpAddr>()
            .map(|ip| ip.to_string())
            .map_err(|_| "expected an IPv4 or IPv6 address, localhost, or *".into()),
    }
}

/// Connect locally when a listener is bound to all interfaces.
pub fn connect_address(address: Option<&str>) -> &str {
    match address.unwrap_or_default() {
        "" | "*" | "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        address => address,
    }
}

pub fn configured_port(text: &str, key: &str) -> Result<Option<u16>> {
    match properties::get(text, key) {
        Some(value) => {
            let port = value
                .parse::<u16>()
                .with_context(|| format!("invalid {key}"))?;
            ensure!(port != 0, "invalid {key}: port must be nonzero");
            Ok(Some(port))
        }
        None => Ok(None),
    }
}

fn read(text: &str) -> Result<Settings> {
    Ok(Settings {
        game: configured_port(text, "server-port")?.unwrap_or(ports::DEFAULT_SERVER_PORT),
        rcon: configured_port(text, "rcon.port")?.unwrap_or(ports::DEFAULT_RCON_PORT),
        address: properties::get(text, "server-ip")
            .unwrap_or_default()
            .to_owned(),
    })
}

/// Executed by the instance owner, including for system instances.
pub fn worker(server: &Path, change: &Change) -> Result<Settings> {
    let path = server.join("server.properties");
    let text = std::fs::read_to_string(&path).context("reading server.properties")?;
    let mut pairs = Vec::new();
    if let Some(port) = change.game {
        ensure!(port != 0, "game port must be nonzero");
        pairs.push(("server-port", port.to_string()));
    }
    if let Some(port) = change.rcon {
        ensure!(port != 0, "RCON port must be nonzero");
        pairs.push(("rcon.port", port.to_string()));
    }
    if let Some(address) = &change.address {
        pairs.push((
            "server-ip",
            parse_address(address).map_err(anyhow::Error::msg)?,
        ));
    }
    let updated = if pairs.is_empty() {
        text.clone()
    } else {
        properties::set(
            &text,
            &pairs
                .iter()
                .map(|(k, v)| (*k, v.as_str()))
                .collect::<Vec<_>>(),
        )
    };
    let settings = read(&updated)?;
    if updated != text {
        hopper_core::fs::write_private_atomic(&path, updated.as_bytes())?;
    }
    Ok(settings)
}

pub fn configure(record: &mut Instance, change: Change, json: bool) -> Result<i32> {
    let changing = change.game.is_some() || change.rcon.is_some() || change.address.is_some();
    let _guard = managed::operation_lock(record)?;
    if record.scope == crate::interface::Scope::System {
        managed::refresh_system_helper()?;
    }
    let settings: Settings = serde_json::from_str(&managed::capture_args(
        record,
        "network",
        &[serde_json::to_string(&change)?],
    )?)?;
    if changing {
        record.game_port = Some(settings.game);
        record.rcon_port = Some(settings.rcon);
        record.listen_address = Some(settings.address.clone());
        managed::save(record)?;
    }
    if json {
        println!("{}", serde_json::to_string(&settings)?);
    } else {
        let address = if settings.address.is_empty() {
            "*"
        } else {
            &settings.address
        };
        println!(
            "{}: game={} rcon={} listen={address}",
            record.name, settings.game, settings.rcon
        );
        if changing {
            println!("Saved; takes effect on the next start or restart.");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_validation_and_connections_support_ipv4_ipv6_and_wildcards() {
        for (input, stored, connected) in [
            ("*", "", "127.0.0.1"),
            ("localhost", "127.0.0.1", "127.0.0.1"),
            ("0.0.0.0", "0.0.0.0", "127.0.0.1"),
            ("::", "::", "::1"),
            ("::1", "::1", "::1"),
            ("192.0.2.10", "192.0.2.10", "192.0.2.10"),
        ] {
            assert_eq!(parse_address(input).unwrap(), stored);
            assert_eq!(connect_address(Some(stored)), connected);
        }
        assert!(parse_address("not an address").is_err());
        assert!(configured_port("server-port=invalid\n", "server-port").is_err());
        assert!(configured_port("rcon.port=0\n", "rcon.port").is_err());
    }

    #[test]
    fn explicit_settings_preserve_password_comments_and_unrelated_properties() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("server.properties");
        std::fs::write(&path, "# operator config\nmotd=hello\nserver-port=25565\nrcon.port=25575\nrcon.password=private\nserver-ip=\n").unwrap();
        let settings = worker(
            temp.path(),
            &Change {
                game: Some(25000),
                rcon: Some(25001),
                address: Some("::1".into()),
            },
        )
        .unwrap();
        assert_eq!(
            (settings.game, settings.rcon, settings.address.as_str()),
            (25000, 25001, "::1")
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# operator config\nmotd=hello\n"));
        assert!(text.contains("rcon.password=private\n"));
        assert!(
            !serde_json::to_string(&settings)
                .unwrap()
                .contains("private")
        );
        worker(
            temp.path(),
            &Change {
                address: Some("*".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            properties::get(&std::fs::read_to_string(&path).unwrap(), "server-ip"),
            Some("")
        );
    }

    #[test]
    fn inspecting_and_invalid_settings_do_not_change_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("server.properties");
        let original = "# keep this\nmotd=original\n";
        std::fs::write(&path, original).unwrap();
        let settings = worker(temp.path(), &Change::default()).unwrap();
        assert_eq!((settings.game, settings.rcon), (25565, 25575));
        for change in [
            Change {
                game: Some(0),
                ..Default::default()
            },
            Change {
                address: Some("evil\nserver-port=1".into()),
                ..Default::default()
            },
        ] {
            assert!(worker(temp.path(), &change).is_err());
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
