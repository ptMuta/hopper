//! Server List Ping: what the multiplayer screen asks a server before joining.
//!
//! Needs no password and works on every server since 1.7, so `hopper show` gets the live MOTD,
//! player count and version straight from the game rather than from config that may be stale.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum PingError {
    #[error("could not reach the server: {0}")]
    Connect(std::io::Error),
    #[error("status ping failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the server sent a malformed status response")]
    Malformed,
}

/// What a server says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub version: String,
    pub online: u32,
    pub max: u32,
    /// The names the server chose to show; servers send at most a sample.
    pub sample: Vec<String>,
    /// Plain text, formatting codes removed.
    pub motd: String,
    pub latency: Duration,
}

pub fn ping(host: &str, port: u16) -> Result<Status, PingError> {
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(PingError::Connect)?
        .next()
        .ok_or_else(|| PingError::Connect(std::io::ErrorKind::NotFound.into()))?;
    let mut s =
        TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(PingError::Connect)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;

    let started = Instant::now();
    // Handshake: protocol version (-1: "just asking"), address, port, next state 1 = status.
    let mut handshake = Vec::new();
    write_varint(&mut handshake, 0x00);
    write_varint(&mut handshake, -1);
    write_varint(&mut handshake, host.len() as i32);
    handshake.extend_from_slice(host.as_bytes());
    handshake.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut handshake, 1);
    send(&mut s, &handshake)?;
    // Status request: an empty packet 0x00.
    send(&mut s, &[0x00])?;

    let len = read_varint(&mut s)?;
    if !(1..=1 << 20).contains(&len) {
        return Err(PingError::Malformed);
    }
    let mut body = vec![0u8; len as usize];
    s.read_exact(&mut body)?;
    let latency = started.elapsed();

    let mut cur = &body[..];
    if read_varint(&mut cur)? != 0x00 {
        return Err(PingError::Malformed);
    }
    let json_len = read_varint(&mut cur)? as usize;
    let json = cur.get(..json_len).ok_or(PingError::Malformed)?;
    parse_status(json, latency)
}

fn parse_status(json: &[u8], latency: Duration) -> Result<Status, PingError> {
    let v: serde_json::Value = serde_json::from_slice(json).map_err(|_| PingError::Malformed)?;
    let players = &v["players"];
    Ok(Status {
        version: v["version"]["name"].as_str().unwrap_or_default().to_owned(),
        online: players["online"].as_u64().unwrap_or(0) as u32,
        max: players["max"].as_u64().unwrap_or(0) as u32,
        sample: players["sample"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| p["name"].as_str().map(strip_codes))
                    .collect()
            })
            .unwrap_or_default(),
        motd: strip_codes(&chat_text(&v["description"])).trim().to_owned(),
        latency,
    })
}

/// The plain text of a chat component: a string, or `{"text", "extra": [...]}`.
fn chat_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts.iter().map(chat_text).collect(),
        serde_json::Value::Object(o) => {
            let mut out = o
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_owned();
            if let Some(extra) = o.get("extra") {
                out.push_str(&chat_text(extra));
            }
            out
        }
        _ => String::new(),
    }
}

/// Remove `§x` formatting codes.
pub fn strip_codes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

fn send(s: &mut TcpStream, packet: &[u8]) -> std::io::Result<()> {
    let mut framed = Vec::with_capacity(packet.len() + 5);
    write_varint(&mut framed, packet.len() as i32);
    framed.extend_from_slice(packet);
    s.write_all(&framed)
}

fn write_varint(out: &mut Vec<u8>, value: i32) {
    let mut v = value as u32;
    loop {
        if v & !0x7F == 0 {
            out.push(v as u8);
            return;
        }
        out.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
}

fn read_varint(r: &mut impl Read) -> Result<i32, PingError> {
    let mut value: u32 = 0;
    for i in 0..5 {
        let mut b = [0u8];
        r.read_exact(&mut b)?;
        value |= u32::from(b[0] & 0x7F) << (7 * i);
        if b[0] & 0x80 == 0 {
            return Ok(value as i32);
        }
    }
    Err(PingError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn varints_round_trip() {
        for v in [0, 1, 127, 128, 255, 25565, 2_097_151, i32::MAX, -1] {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            assert_eq!(read_varint(&mut &buf[..]).unwrap(), v, "{v}");
        }
        let mut buf = Vec::new();
        write_varint(&mut buf, -1);
        assert_eq!(buf.len(), 5);
    }

    #[test]
    fn chat_components_flatten_to_plain_text() {
        let v =
            serde_json::json!({"text": "§aDeceased", "extra": [{"text": "Craft"}, " §lserver"]});
        assert_eq!(strip_codes(&chat_text(&v)), "DeceasedCraft server");
        assert_eq!(chat_text(&serde_json::json!("plain")), "plain");
    }

    #[test]
    fn pings_a_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // Read and ignore the handshake and the status request.
            for _ in 0..2 {
                let len = read_varint(&mut s).unwrap();
                let mut skip = vec![0u8; len as usize];
                s.read_exact(&mut skip).unwrap();
            }
            let json = serde_json::json!({
                "version": {"name": "1.20.1", "protocol": 763},
                "players": {"max": 20, "online": 2, "sample": [{"name": "Steve", "id": "x"}, {"name": "Alex", "id": "y"}]},
                "description": {"text": "§6A Minecraft Server"}
            })
            .to_string();
            let mut body = Vec::new();
            write_varint(&mut body, 0x00);
            write_varint(&mut body, json.len() as i32);
            body.extend_from_slice(json.as_bytes());
            let mut framed = Vec::new();
            write_varint(&mut framed, body.len() as i32);
            framed.extend_from_slice(&body);
            s.write_all(&framed).unwrap();
        });

        let st = ping("127.0.0.1", port).unwrap();
        assert_eq!(st.version, "1.20.1");
        assert_eq!((st.online, st.max), (2, 20));
        assert_eq!(st.sample, ["Steve", "Alex"]);
        assert_eq!(st.motd, "A Minecraft Server");
    }

    #[test]
    fn nothing_listening_is_a_connect_error() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(matches!(
            ping("127.0.0.1", port),
            Err(PingError::Connect(_))
        ));
    }
}
