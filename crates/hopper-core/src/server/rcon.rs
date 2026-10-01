//! A minimal RCON client, for `hopper console`.
//!
//! The protocol is Valve's Source RCON as Minecraft implements it: little-endian length-prefixed
//! packets of `id`, `type` and a NUL-terminated ASCII body. Authenticate once, then send
//! commands. Minecraft answers each command with one packet (long output is truncated by the
//! server at about 4KB, which is a server limit, not ours).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const TYPE_AUTH: i32 = 3;
const TYPE_COMMAND: i32 = 2;
const TYPE_RESPONSE: i32 = 0;
/// Minecraft rejects anything longer than this from a client.
const MAX_REQUEST_BODY: usize = 1446;
/// Generous: Minecraft's responses are at most about 4KB.
const MAX_PACKET: i32 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RconError {
    #[error("could not reach the server's RCON port: {0}")]
    Connect(std::io::Error),
    #[error("RCON connection failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the server rejected the RCON password")]
    BadPassword,
    #[error("the server sent a malformed RCON packet")]
    Malformed,
    #[error("command is longer than the {MAX_REQUEST_BODY} bytes RCON accepts")]
    TooLong,
}

pub struct Rcon {
    stream: TcpStream,
    next_id: i32,
}

impl Rcon {
    /// Connect and authenticate.
    pub fn connect(addr: impl ToSocketAddrs, password: &str) -> Result<Self, RconError> {
        let addr = addr
            .to_socket_addrs()
            .map_err(RconError::Connect)?
            .next()
            .ok_or_else(|| RconError::Connect(std::io::ErrorKind::NotFound.into()))?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .map_err(RconError::Connect)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut rcon = Self { stream, next_id: 1 };

        let id = rcon.send(TYPE_AUTH, password)?;
        // A failed login is answered with id -1. Some servers send an empty response packet
        // before the auth reply, so skip anything that is not the reply.
        loop {
            let (rid, ty, _) = rcon.recv()?;
            if rid == -1 {
                return Err(RconError::BadPassword);
            }
            if rid == id && ty == TYPE_COMMAND {
                return Ok(rcon);
            }
        }
    }

    /// Run one command and return what the server printed.
    pub fn command(&mut self, cmd: &str) -> Result<String, RconError> {
        if cmd.len() > MAX_REQUEST_BODY {
            return Err(RconError::TooLong);
        }
        let id = self.send(TYPE_COMMAND, cmd)?;
        loop {
            let (rid, ty, body) = self.recv()?;
            if rid == id && ty == TYPE_RESPONSE {
                return Ok(body);
            }
        }
    }

    fn send(&mut self, ty: i32, body: &str) -> Result<i32, RconError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let len = (4 + 4 + body.len() + 2) as i32;
        let mut buf = Vec::with_capacity(len as usize + 4);
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&id.to_le_bytes());
        buf.extend_from_slice(&ty.to_le_bytes());
        buf.extend_from_slice(body.as_bytes());
        buf.extend_from_slice(&[0, 0]);
        self.stream.write_all(&buf)?;
        Ok(id)
    }

    fn recv(&mut self) -> Result<(i32, i32, String), RconError> {
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len)?;
        let len = i32::from_le_bytes(len);
        if !(10..=MAX_PACKET).contains(&len) {
            return Err(RconError::Malformed);
        }
        let mut rest = vec![0u8; len as usize];
        self.stream.read_exact(&mut rest)?;
        let id = i32::from_le_bytes(rest[0..4].try_into().expect("4 bytes"));
        let ty = i32::from_le_bytes(rest[4..8].try_into().expect("4 bytes"));
        let body = &rest[8..rest.len() - 2];
        Ok((id, ty, String::from_utf8_lossy(body).into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A server that speaks just enough RCON: checks the password, echoes commands.
    fn fake_server(password: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let read = |s: &mut TcpStream| -> Option<(i32, i32, String)> {
                let mut len = [0u8; 4];
                s.read_exact(&mut len).ok()?;
                let mut rest = vec![0u8; i32::from_le_bytes(len) as usize];
                s.read_exact(&mut rest).ok()?;
                Some((
                    i32::from_le_bytes(rest[0..4].try_into().unwrap()),
                    i32::from_le_bytes(rest[4..8].try_into().unwrap()),
                    String::from_utf8_lossy(&rest[8..rest.len() - 2]).into_owned(),
                ))
            };
            let write = |s: &mut TcpStream, id: i32, ty: i32, body: &str| {
                let mut b = Vec::new();
                b.extend_from_slice(&((10 + body.len()) as i32).to_le_bytes());
                b.extend_from_slice(&id.to_le_bytes());
                b.extend_from_slice(&ty.to_le_bytes());
                b.extend_from_slice(body.as_bytes());
                b.extend_from_slice(&[0, 0]);
                s.write_all(&b).unwrap();
            };
            while let Some((id, ty, body)) = read(&mut s) {
                if ty == TYPE_AUTH {
                    let ok = body == password;
                    write(&mut s, if ok { id } else { -1 }, TYPE_COMMAND, "");
                    if !ok {
                        return;
                    }
                } else {
                    write(&mut s, id, TYPE_RESPONSE, &format!("ran: {body}"));
                }
            }
        });
        addr
    }

    #[test]
    fn authenticates_and_runs_commands() {
        let addr = fake_server("hunter2");
        let mut rcon = Rcon::connect(addr, "hunter2").unwrap();
        assert_eq!(rcon.command("list").unwrap(), "ran: list");
        assert_eq!(rcon.command("say hi").unwrap(), "ran: say hi");
    }

    #[test]
    fn a_wrong_password_is_reported_as_such() {
        let addr = fake_server("hunter2");
        assert!(matches!(
            Rcon::connect(addr, "nope"),
            Err(RconError::BadPassword)
        ));
    }

    #[test]
    fn overlong_commands_are_refused_before_sending() {
        let addr = fake_server("pw");
        let mut rcon = Rcon::connect(addr, "pw").unwrap();
        assert!(matches!(
            rcon.command(&"x".repeat(2000)),
            Err(RconError::TooLong)
        ));
    }
}
