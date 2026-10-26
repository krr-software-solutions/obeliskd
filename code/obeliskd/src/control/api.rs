//! The control API's wire types, shared by the server and the client.

use std::net::SocketAddrV4;

use ipnet::Ipv4Net;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use super::Peer;
use std::net::Ipv4Addr;

use crate::model::{Name, Subnet, SubnetKind};

/// `GET /health`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub caller: Peer,
    pub daemon: DaemonInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInfo {
    pub started_at: Timestamp,
    /// Where the daemon answers DNS.
    pub dns_address: SocketAddrV4,
}

/// `POST /supernets`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupernetAlloc {
    pub name: Name,
    pub cidr: Ipv4Net,
}

/// `POST /subnets`: obeliskd picks the first free range of `size`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubnetAlloc {
    pub supernet: Name,
    pub name: Name,
    pub kind: SubnetKind,
    /// A prefix length, e.g. 24.
    pub size: u8,
    /// Docker only, and required there: the prefix length of Docker's own slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_size: Option<u8>,
}

/// A subnet, with the supernet it's in (`GET /subnets`, `POST /subnets`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubnetEntry {
    pub supernet: Name,
    #[serde(flatten)]
    pub subnet: Subnet,
}

/// `GET /subnets?supernet=…`
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubnetQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supernet: Option<Name>,
}

/// `DELETE /supernets/{supernet}?recursive=…`
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupernetDealloc {
    /// Also remove its subnets (otherwise it must have none).
    #[serde(default)]
    pub recursive: bool,
}

/// `DELETE /subnets/{supernet}/{subnet}?recursive=…`
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubnetDealloc {
    /// Also remove its ips (otherwise it must have none).
    #[serde(default)]
    pub recursive: bool,
}

/// `POST /ips`: obeliskd picks the lowest free address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpAlloc {
    pub supernet: Name,
    pub subnet: Name,
    pub name: Name,
}

/// An ip, with where it lives (`GET /ips`, `POST /ips`, `DELETE /ips/…`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpEntry {
    pub supernet: Name,
    pub subnet: Name,
    pub kind: SubnetKind,
    pub name: Name,
    pub address: Ipv4Addr,
}

/// `GET /ips?supernet=…&subnet=…` (`subnet` needs `supernet`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supernet: Option<Name>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnet: Option<Name>,
}

/// The body of every non-2xx response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}
