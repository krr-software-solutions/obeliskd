//! The control API's wire types, shared by the server and the client.

use std::net::SocketAddrV4;

use ipnet::Ipv4Net;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use super::Peer;
use crate::model::Name;

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

/// The body of every non-2xx response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}
