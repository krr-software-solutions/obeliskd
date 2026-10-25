//! The DNS wire format (RFC 1035): only as much as obeliskd needs to read a query and answer it.
//!
//! A message is a 12-byte header, then sections of questions, answers, authority and additional
//! records. Queries carry one question: a name as length-prefixed labels ending in a zero byte,
//! then a type and a class.

use std::net::Ipv4Addr;

pub const HEADER_LEN: usize = 12;

/// The longest a label can be: the length byte's top two bits are reserved (`0b11` marks a
/// compression pointer).
const MAX_LABEL_LEN: usize = 63;
/// The longest a whole name can be on the wire, length bytes included.
const MAX_NAME_LEN: usize = 255;

pub const OPCODE_QUERY: u8 = 0;

pub const TYPE_A: u16 = 1;
pub const CLASS_IN: u16 = 1;

/// Response codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Rcode {
    NoError = 0,
    FormErr = 1,
    NotImp = 4,
    Refused = 5,
}

const FLAG_QR: u16 = 0x8000;
const FLAG_AA: u16 = 0x0400;
const FLAG_RD: u16 = 0x0100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub id: u16,
    pub flags: u16,
    pub qdcount: u16,
}

impl Header {
    /// `None` if the packet is too short to have a header.
    #[must_use]
    pub fn parse(packet: &[u8]) -> Option<Self> {
        let header = packet.get(..HEADER_LEN)?;
        let u16_at = |i: usize| u16::from_be_bytes([header[i], header[i + 1]]);
        Some(Header {
            id: u16_at(0),
            flags: u16_at(2),
            qdcount: u16_at(4),
        })
    }

    #[must_use]
    pub fn is_response(self) -> bool {
        self.flags & FLAG_QR != 0
    }

    #[must_use]
    pub fn opcode(self) -> u8 {
        // Four bits; the mask makes the cast lossless.
        ((self.flags >> 11) & 0xF) as u8
    }

    #[must_use]
    pub fn recursion_desired(self) -> bool {
        self.flags & FLAG_RD != 0
    }
}

/// A domain name, lowercased for matching (DNS names are case-insensitive).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    labels: Vec<Vec<u8>>,
}

impl Name {
    /// Whether this name is `zone` or below it, e.g. `a.obd.internal` and `obd.internal` are both
    /// in `obd.internal`.
    #[must_use]
    pub fn is_in(&self, zone: &str) -> bool {
        let zone: Vec<&[u8]> = zone.split('.').map(str::as_bytes).collect();
        self.labels.len() >= zone.len()
            && self.labels[self.labels.len() - zone.len()..]
                .iter()
                .zip(&zone)
                .all(|(label, z)| label.as_slice() == *z)
    }

    /// Whether this name is strictly below `zone` (`a.obd.internal` is, `obd.internal` isn't).
    #[must_use]
    pub fn is_below(&self, zone: &str) -> bool {
        self.is_in(zone) && self.labels.len() > zone.split('.').count()
    }
}

/// The question of a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question<'a> {
    pub name: Name,
    pub qtype: u16,
    pub qclass: u16,
    /// The question exactly as received, to echo back (names keep their original case).
    pub raw: &'a [u8],
}

/// The packet isn't a well-formed query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

impl<'a> Question<'a> {
    /// Parses the question that starts right after the header.
    ///
    /// # Errors
    ///
    /// [`Malformed`] if it's truncated, a label or the name is too long, or it uses compression.
    pub fn parse(packet: &'a [u8]) -> Result<Self, Malformed> {
        let mut pos = HEADER_LEN;
        let mut labels = Vec::new();
        let mut name_len = 0;
        loop {
            let len = usize::from(*packet.get(pos).ok_or(Malformed)?);
            // Also rejects compression pointers, which queries don't use.
            if len > MAX_LABEL_LEN {
                return Err(Malformed);
            }
            pos += 1;
            name_len += 1 + len;
            if name_len > MAX_NAME_LEN {
                return Err(Malformed);
            }
            if len == 0 {
                break;
            }
            let label = packet.get(pos..pos + len).ok_or(Malformed)?;
            labels.push(label.to_ascii_lowercase());
            pos += len;
        }

        let fixed = packet.get(pos..pos + 4).ok_or(Malformed)?;
        Ok(Question {
            name: Name { labels },
            qtype: u16::from_be_bytes([fixed[0], fixed[1]]),
            qclass: u16::from_be_bytes([fixed[2], fixed[3]]),
            raw: &packet[HEADER_LEN..pos + 4],
        })
    }
}

/// An A record answering the question (its name points back at the question's).
pub struct ARecord {
    pub ttl: u32,
    pub address: Ipv4Addr,
}

/// Builds a response to `query`.
///
/// `question` is echoed back if there is one. Recursion is never available: obeliskd only answers
/// for its own zone.
#[must_use]
pub fn response(
    query: Header,
    rcode: Rcode,
    authoritative: bool,
    question: Option<&Question<'_>>,
    answer: Option<&ARecord>,
) -> Vec<u8> {
    let mut flags = FLAG_QR | u16::from(rcode as u8);
    if authoritative {
        flags |= FLAG_AA;
    }
    if query.recursion_desired() {
        flags |= FLAG_RD;
    }

    let mut out = Vec::with_capacity(HEADER_LEN + 64);
    out.extend_from_slice(&query.id.to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&u16::from(question.is_some()).to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&u16::from(answer.is_some()).to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT

    if let Some(question) = question {
        out.extend_from_slice(question.raw);
    }
    if let Some(answer) = answer {
        // A compression pointer to the question's name, which starts right after the header.
        out.extend_from_slice(&[0xC0, 0x0C]);
        out.extend_from_slice(&TYPE_A.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&answer.ttl.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH
        out.extend_from_slice(&answer.address.octets());
    }
    out
}
