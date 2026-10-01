//! This machine's public address, as Google's nameservers see it.
//!
//! `ns1.google.com` answers a TXT query for `o-o.myaddr.l.google.com` with the address the
//! query came from: the same trick as `dig TXT o-o.myaddr.l.google.com @ns1.google.com`. One
//! UDP round trip, no HTTP and no third-party "what is my IP" site. Asked over IPv4 and IPv6
//! separately, since a machine has a different public address on each.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

const NAMESERVER: &str = "ns1.google.com";
const QUERY_NAME: &str = "o-o.myaddr.l.google.com";
const TYPE_TXT: u16 = 16;

/// The public address on each family the machine can reach Google over.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublicIps {
    pub v4: Option<IpAddr>,
    pub v6: Option<IpAddr>,
}

/// Both lookups run at once; neither takes longer than `timeout`.
pub fn lookup(timeout: Duration) -> PublicIps {
    let Ok(servers) = (NAMESERVER, 53).to_socket_addrs() else {
        return PublicIps::default();
    };
    let servers: Vec<SocketAddr> = servers.collect();
    let v4 = servers.iter().find(|a| a.is_ipv4()).copied();
    let v6 = servers.iter().find(|a| a.is_ipv6()).copied();

    let ask = move |server: Option<SocketAddr>| {
        std::thread::spawn(move || server.and_then(|s| query(s, timeout)))
    };
    let (h4, h6) = (ask(v4), ask(v6));
    PublicIps {
        v4: h4.join().ok().flatten().filter(IpAddr::is_ipv4),
        v6: h6.join().ok().flatten().filter(IpAddr::is_ipv6),
    }
}

fn query(server: SocketAddr, timeout: Duration) -> Option<IpAddr> {
    let bind: SocketAddr = if server.is_ipv4() {
        "0.0.0.0:0".parse().ok()?
    } else {
        "[::]:0".parse().ok()?
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.set_read_timeout(Some(timeout)).ok()?;
    socket.connect(server).ok()?;

    let id = transaction_id();
    socket.send(&build_query(id)).ok()?;
    let mut buf = [0u8; 1500];
    let n = socket.recv(&mut buf).ok()?;
    parse_answer(&buf[..n], id)
}

/// Not security-relevant (the answer is only displayed), but a fixed id would let a stray
/// packet from an earlier run be mistaken for this one's answer.
fn transaction_id() -> u16 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos ^ std::process::id()) as u16
}

fn build_query(id: u16) -> Vec<u8> {
    let mut q = Vec::with_capacity(48);
    q.extend_from_slice(&id.to_be_bytes());
    // Standard query, no recursion needed: the nameserver is authoritative for the name.
    q.extend_from_slice(&[0x00, 0x00]);
    q.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // 1 question, no other records
    for label in QUERY_NAME.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&TYPE_TXT.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes()); // class IN
    q
}

/// The first TXT string in the answer that is an IP address.
///
/// Google may add a second TXT record describing the client subnet; it does not parse as an
/// address and is skipped.
fn parse_answer(msg: &[u8], id: u16) -> Option<IpAddr> {
    if msg.len() < 12 || u16::from_be_bytes([msg[0], msg[1]]) != id {
        return None;
    }
    // Reply flag set, and no error code.
    if msg[2] & 0x80 == 0 || msg[3] & 0x0F != 0 {
        return None;
    }
    let questions = u16::from_be_bytes([msg[4], msg[5]]);
    let answers = u16::from_be_bytes([msg[6], msg[7]]);

    let mut pos = 12;
    for _ in 0..questions {
        pos = skip_name(msg, pos)? + 4;
    }
    for _ in 0..answers {
        pos = skip_name(msg, pos)?;
        let header = msg.get(pos..pos + 10)?;
        let ty = u16::from_be_bytes([header[0], header[1]]);
        let len = u16::from_be_bytes([header[8], header[9]]) as usize;
        pos += 10;
        let rdata = msg.get(pos..pos + len)?;
        pos += len;
        if ty != TYPE_TXT {
            continue;
        }
        // TXT rdata is a run of length-prefixed strings.
        let mut i = 0;
        while i < rdata.len() {
            let l = rdata[i] as usize;
            let s = rdata.get(i + 1..i + 1 + l)?;
            if let Ok(ip) = std::str::from_utf8(s).ok()?.trim().parse::<IpAddr>() {
                return Some(ip);
            }
            i += 1 + l;
        }
    }
    None
}

/// Past a (possibly compressed) domain name.
fn skip_name(msg: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *msg.get(pos)?;
        match len {
            0 => return Some(pos + 1),
            // A compression pointer ends the name in two bytes.
            l if l & 0xC0 == 0xC0 => return Some(pos + 2),
            l => pos += 1 + l as usize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply as ns1.google.com sends it: the question echoed, then TXT answers.
    fn reply(id: u16, txts: &[&str]) -> Vec<u8> {
        let q = build_query(id);
        let mut m = q.clone();
        m[2] = 0x84; // response, authoritative
        m[3] = 0x00;
        m[6..8].copy_from_slice(&(txts.len() as u16).to_be_bytes());
        for t in txts {
            m.extend_from_slice(&[0xC0, 0x0C]); // pointer to the question name
            m.extend_from_slice(&TYPE_TXT.to_be_bytes());
            m.extend_from_slice(&1u16.to_be_bytes());
            m.extend_from_slice(&60u32.to_be_bytes());
            m.extend_from_slice(&((t.len() + 1) as u16).to_be_bytes());
            m.push(t.len() as u8);
            m.extend_from_slice(t.as_bytes());
        }
        m
    }

    #[test]
    fn reads_the_address_from_the_answer() {
        let m = reply(7, &["203.0.113.9"]);
        assert_eq!(parse_answer(&m, 7), "203.0.113.9".parse().ok());
        let m = reply(7, &["2001:db8::1"]);
        assert_eq!(parse_answer(&m, 7), "2001:db8::1".parse().ok());
    }

    #[test]
    fn skips_the_client_subnet_record() {
        let m = reply(7, &["edns0-client-subnet 198.51.100.0/24", "203.0.113.9"]);
        assert_eq!(parse_answer(&m, 7), "203.0.113.9".parse().ok());
    }

    #[test]
    fn rejects_someone_elses_answer_and_errors() {
        assert_eq!(parse_answer(&reply(7, &["203.0.113.9"]), 8), None);
        let mut m = reply(7, &["203.0.113.9"]);
        m[3] = 0x03; // NXDOMAIN
        assert_eq!(parse_answer(&m, 7), None);
        assert_eq!(parse_answer(&[0; 5], 7), None);
    }

    #[test]
    fn the_query_names_the_magic_record() {
        let q = build_query(1);
        assert!(q.windows(4).any(|w| w == b"myad"));
        assert_eq!(&q[q.len() - 4..], &[0, 16, 0, 1]);
    }
}
