//! The DNS side: answers for the `obd.internal` zone over UDP.
//!
//! `.internal` is reserved for private use (ICANN, 2024), so no public name can ever collide.
//! The OS is meant to route only `*.obd.internal` names here (a routing domain), so anything else is
//! answered `REFUSED`: not our zone, try another server (never `NXDOMAIN`, which would claim a name
//! we don't own doesn't exist). Inside the zone obeliskd knows every name: a host answers with its
//! ip's address, and anything else is `NXDOMAIN`.

pub mod hosts;
pub mod wire;

use std::net::SocketAddrV4;
use std::sync::{Arc, RwLock};

use tokio::net::UdpSocket;

use crate::ObeliskdError;
pub use hosts::HostTable;
use wire::{ARecord, CLASS_IN, Header, OPCODE_QUERY, Question, Rcode, TYPE_A};

/// The suffix obeliskd answers for: `*.obd.internal`.
pub const ZONE: &str = "obd.internal";

/// Short, so changes take effect quickly in resolvers' caches.
const ANSWER_TTL: u32 = 60;

/// Big enough for any query, EDNS options included; longer datagrams are truncated by the OS.
const MAX_QUERY_LEN: usize = 4096;

/// Shared between the store (which replaces the table) and the DNS loop (which reads it).
pub type SharedHosts = Arc<RwLock<Arc<HostTable>>>;

/// The response to one query packet, or `None` to drop it.
#[must_use]
pub fn respond(packet: &[u8], hosts: &HostTable) -> Option<Vec<u8>> {
    let header = Header::parse(packet)?;
    if header.is_response() {
        return None;
    }
    if header.opcode() != OPCODE_QUERY {
        return Some(wire::response(header, Rcode::NotImp, false, None, None));
    }
    if header.qdcount != 1 {
        return Some(wire::response(header, Rcode::FormErr, false, None, None));
    }
    let Ok(question) = Question::parse(packet) else {
        return Some(wire::response(header, Rcode::FormErr, false, None, None));
    };

    if question.qclass != CLASS_IN || !question.name.is_in(ZONE) {
        return Some(wire::response(
            header,
            Rcode::Refused,
            false,
            Some(&question),
            None,
        ));
    }

    // Ours: authoritative from here on.
    let name = question.name.to_dotted();
    let (rcode, answer) = match hosts.lookup(&name) {
        Some(address) => {
            let answer = (question.qtype == TYPE_A).then_some(ARecord {
                ttl: ANSWER_TTL,
                address,
            });
            // Other types: the name exists, just not with that type (NODATA), never NXDOMAIN.
            (Rcode::NoError, answer)
        }
        // The zone itself, and names with hosts below them (RFC 8020's "empty non-terminals"),
        // exist too: NODATA.
        None if name == ZONE || hosts.has_below(&name) => (Rcode::NoError, None),
        None => (Rcode::NxDomain, None),
    };
    Some(wire::response(
        header,
        rcode,
        true,
        Some(&question),
        answer.as_ref(),
    ))
}

/// Binds the DNS socket (UDP).
///
/// # Errors
///
/// [`ObeliskdError::DnsBind`], e.g. when the port is taken or needs root (port 53 does).
pub async fn bind(address: SocketAddrV4) -> Result<UdpSocket, ObeliskdError> {
    UdpSocket::bind(address)
        .await
        .map_err(|source| ObeliskdError::DnsBind { address, source })
}

/// Answers queries on `socket` until the future is dropped.
///
/// # Panics
///
/// If the host table's lock is poisoned, which can't happen: nothing panics while holding it.
pub async fn serve(socket: UdpSocket, hosts: SharedHosts) {
    let mut buf = vec![0u8; MAX_QUERY_LEN];
    loop {
        let (len, from) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(e) => {
                eprintln!("obeliskd: DNS receive failed: {e}");
                continue;
            }
        };
        // Cloning the Arc is all the lock is held for.
        let table = Arc::clone(&hosts.read().expect("the host table lock is never poisoned"));
        let Some(response) = respond(&buf[..len], &table) else {
            continue;
        };
        if let Err(e) = socket.send_to(&response, from).await {
            eprintln!("obeliskd: DNS reply to {from} failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn hosts() -> HostTable {
        HostTable::from([
            (
                "my.nested.obd.internal".to_string(),
                Ipv4Addr::new(10, 57, 0, 5),
            ),
            (
                "a.deep.obd.internal".to_string(),
                Ipv4Addr::new(10, 57, 0, 6),
            ),
        ])
    }
    const TYPE_AAAA: u16 = 28;

    /// A query as a stub resolver sends it: id 0xBEEF, RD set, one question.
    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0xBE, 0xEF, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.').filter(|l| !l.is_empty()) {
            q.push(u8::try_from(label.len()).unwrap());
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        q
    }

    struct Parsed {
        flags: u16,
        qdcount: u16,
        ancount: u16,
        rest: Vec<u8>,
    }

    fn parse(response: &[u8]) -> Parsed {
        let u16_at = |i: usize| u16::from_be_bytes([response[i], response[i + 1]]);
        assert_eq!(u16_at(0), 0xBEEF, "the id is echoed");
        Parsed {
            flags: u16_at(2),
            qdcount: u16_at(4),
            ancount: u16_at(6),
            rest: response[12..].to_vec(),
        }
    }

    fn rcode(flags: u16) -> u16 {
        flags & 0xF
    }

    #[test]
    fn answers_a_for_names_below_the_zone() {
        let q = query("My.Nested.OBD.Internal", TYPE_A);
        let r = parse(&respond(&q, &hosts()).unwrap());
        assert_eq!(r.flags, 0x8500, "QR, AA, RD; NOERROR");
        assert_eq!((r.qdcount, r.ancount), (1, 1));

        let question = &q[12..];
        assert_eq!(
            &r.rest[..question.len()],
            question,
            "the question is echoed in its case"
        );
        let answer = &r.rest[question.len()..];
        #[rustfmt::skip]
        assert_eq!(answer, [
            0xC0, 0x0C,       // name: pointer to the question's
            0, 1, 0, 1,       // A, IN
            0, 0, 0, 60,      // TTL
            0, 4, 10, 57, 0, 5,
        ]);
    }

    #[test]
    fn wildcard_hosts_answer_for_one_label() {
        let hosts = HostTable::from([(
            "*.apps.obd.internal".to_string(),
            Ipv4Addr::new(10, 57, 0, 9),
        )]);
        let r = parse(&respond(&query("Web.Apps.OBD.internal", TYPE_A), &hosts).unwrap());
        assert_eq!((rcode(r.flags), r.ancount), (0, 1));
        assert_eq!(&r.rest[r.rest.len() - 4..], [10, 57, 0, 9]);

        let deeper = parse(&respond(&query("a.web.apps.obd.internal", TYPE_A), &hosts).unwrap());
        assert_eq!(
            rcode(deeper.flags),
            Rcode::NxDomain as u16,
            "* is exactly one label"
        );
        let parent = parse(&respond(&query("apps.obd.internal", TYPE_A), &hosts).unwrap());
        assert_eq!(
            (rcode(parent.flags), parent.ancount),
            (0, 0),
            "NODATA: names exist below it"
        );
    }

    #[test]
    fn unknown_names_in_the_zone_are_nxdomain() {
        for name in [
            "nope.obd.internal",
            "nested.my.obd.internal",
            "x.a.deep.obd.internal",
        ] {
            let r = parse(&respond(&query(name, TYPE_A), &hosts()).unwrap());
            assert_eq!(rcode(r.flags), Rcode::NxDomain as u16, "{name}");
            assert_eq!(r.flags & 0x0400, 0x0400, "authoritative for {name}");
            assert_eq!((r.qdcount, r.ancount), (1, 0));
        }
    }

    #[test]
    fn other_types_in_the_zone_are_nodata_not_nxdomain() {
        for q in [
            query("my.nested.obd.internal", TYPE_AAAA), // a host, another type
            query("obd.internal", TYPE_A),              // the zone itself
            query("deep.obd.internal", TYPE_A),         // a host below it (empty non-terminal)
            query("nested.obd.internal", TYPE_A),
        ] {
            let r = parse(&respond(&q, &hosts()).unwrap());
            assert_eq!(r.flags, 0x8500, "NOERROR, authoritative");
            assert_eq!((r.qdcount, r.ancount), (1, 0));
        }
    }

    #[test]
    fn names_outside_the_zone_are_refused() {
        for name in [
            "example.com",
            "obd",
            "x.obd",
            "internal",
            "other.internal",
            "notobd.internal",
            "obd.internal.example",
            ".",
        ] {
            let r = parse(&respond(&query(name, TYPE_A), &hosts()).unwrap());
            assert_eq!(rcode(r.flags), Rcode::Refused as u16, "{name}");
            assert_eq!(r.flags & 0x0400, 0, "not authoritative for {name}");
            assert_eq!(r.ancount, 0);
        }
    }

    #[test]
    fn rejects_what_isnt_a_plain_query() {
        // Too short for a header, or a response: dropped.
        assert_eq!(respond(&[0; 11], &hosts()), None);
        let mut response = query("x.obd.internal", TYPE_A);
        response[2] |= 0x80;
        assert_eq!(respond(&response, &hosts()), None);

        // Not a standard query (opcode 2, STATUS).
        let mut status = query("x.obd.internal", TYPE_A);
        status[2] |= 2 << 3;
        let r = parse(&respond(&status, &hosts()).unwrap());
        assert_eq!(rcode(r.flags), Rcode::NotImp as u16);

        // Truncated mid-name, a compression pointer, and two questions.
        let truncated = &query("x.obd.internal", TYPE_A)[..15];
        let mut pointer = query("x.obd.internal", TYPE_A);
        pointer[12] = 0xC0;
        let mut two = query("x.obd.internal", TYPE_A);
        two[5] = 2;
        for bad in [truncated, &pointer, &two] {
            let r = parse(&respond(bad, &hosts()).unwrap());
            assert_eq!(rcode(r.flags), Rcode::FormErr as u16);
            assert_eq!(r.qdcount, 0);
        }
    }

    #[test]
    fn rejects_oversized_labels() {
        let long = "a".repeat(64);
        let r = parse(&respond(&query(&format!("{long}.obd.internal"), TYPE_A), &hosts()).unwrap());
        assert_eq!(rcode(r.flags), Rcode::FormErr as u16);
    }

    #[tokio::test]
    async fn serves_over_udp() {
        let server = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = server.local_addr().unwrap();
        let shared: SharedHosts = Arc::new(RwLock::new(Arc::new(hosts())));
        let serving = tokio::spawn(serve(server, Arc::clone(&shared)));

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(&query("my.nested.obd.internal", TYPE_A), address)
            .await
            .unwrap();
        let mut buf = [0u8; 512];
        let len = client.recv(&mut buf).await.unwrap();
        let r = parse(&buf[..len]);
        assert_eq!((rcode(r.flags), r.ancount), (0, 1));
        assert_eq!(&buf[len - 4..len], [10, 57, 0, 5]);

        // A new table is picked up by the very next query.
        *shared.write().unwrap() = Arc::new(HostTable::from([(
            "my.nested.obd.internal".to_string(),
            Ipv4Addr::new(10, 57, 9, 9),
        )]));
        client
            .send_to(&query("my.nested.obd.internal", TYPE_A), address)
            .await
            .unwrap();
        let len = client.recv(&mut buf).await.unwrap();
        assert_eq!(&buf[len - 4..len], [10, 57, 9, 9]);

        serving.abort();
    }
}
