//! Choosing ports that do not collide with other servers on the same machine.
//!
//! A port is taken if something is listening on it now, or if another server hopper installed
//! has it configured. The second matters as much as the first: a stopped server holds no port,
//! and handing its port to a new one would only surface as a failed start days later.

use std::collections::BTreeSet;

use super::properties;

pub const DEFAULT_SERVER_PORT: u16 = 25565;
pub const DEFAULT_RCON_PORT: u16 = 25575;

/// Every port a `server.properties` claims: the game port, and RCON and query when enabled.
pub fn claimed(text: &str) -> BTreeSet<u16> {
    let port = |key: &str| properties::get(text, key).and_then(|v| v.parse::<u16>().ok());
    let on = |key: &str| properties::get(text, key) == Some("true");

    let server = port("server-port").unwrap_or(DEFAULT_SERVER_PORT);
    let mut out = BTreeSet::from([server]);
    if on("enable-rcon") {
        out.insert(port("rcon.port").unwrap_or(DEFAULT_RCON_PORT));
    }
    if on("enable-query") {
        // Query defaults to the game port, on UDP; recorded either way.
        out.insert(port("query.port").unwrap_or(server));
    }
    out
}

/// The first port from `start` upwards that is neither in `taken` nor refused by `is_free`.
pub fn pick(start: u16, taken: &BTreeSet<u16>, is_free: impl Fn(u16) -> bool) -> Option<u16> {
    (start..=u16::MAX).find(|p| !taken.contains(p) && is_free(*p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_properties_file_claims_its_enabled_ports() {
        assert_eq!(claimed(""), BTreeSet::from([25565]));
        assert_eq!(
            claimed("server-port=25570\nenable-rcon=true\nrcon.port=25580\n"),
            BTreeSet::from([25570, 25580])
        );
        // Disabled RCON claims nothing, even with a port written down.
        assert_eq!(
            claimed("enable-rcon=false\nrcon.port=25580\n"),
            BTreeSet::from([25565])
        );
        assert_eq!(
            claimed("server-port=25570\nenable-query=true\n"),
            BTreeSet::from([25570])
        );
    }

    #[test]
    fn picks_upwards_past_configured_and_busy_ports() {
        let taken = BTreeSet::from([25565, 25566]);
        assert_eq!(pick(25565, &taken, |_| true), Some(25567));
        // 25567 is in use by something hopper does not know about.
        assert_eq!(pick(25565, &taken, |p| p != 25567), Some(25568));
        assert_eq!(pick(u16::MAX, &BTreeSet::from([u16::MAX]), |_| true), None);
    }
}
