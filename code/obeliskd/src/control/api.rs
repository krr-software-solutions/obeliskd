//! The control API's wire types, shared by the server and the client.

use std::net::SocketAddrV4;

use ipnet::Ipv4Net;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use super::Peer;
use std::net::Ipv4Addr;

use crate::model::{Hostname, Ip, Name, Subnet, SubnetKind, Supernet};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<Name>,
}

/// `GET /ips?supernet=…&subnet=…&tag=…` (`subnet` needs `supernet`). With `tag`, only that tag's
/// ips, and only the branches that have any.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supernet: Option<Name>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnet: Option<Name>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<Name>,
}

/// `POST /hosts`: point a hostname at an ip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAlloc {
    pub name: Hostname,
    pub supernet: Name,
    pub subnet: Name,
    pub ip: Name,
}

/// Every response that lists or changes allocations is a tree from the supernets down: a list goes
/// down to its own level (`supernet list` stops at supernets, `ip list` reaches ips), and a change
/// is the path to what changed. A level that was asked for is always present (`[]` when empty).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Depth {
    Supernets,
    Subnets,
    Ips,
    Hosts,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupernetNode {
    pub name: Name,
    pub cidr: Ipv4Net,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnets: Option<Vec<SubnetNode>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubnetNode {
    pub name: Name,
    pub kind: SubnetKind,
    pub cidr: Ipv4Net,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_range: Option<Ipv4Net>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_gateway: Option<Ipv4Addr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ips: Option<Vec<IpNode>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpNode {
    pub name: Name,
    pub address: Ipv4Addr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<Name>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<Hostname>>,
}

impl SupernetNode {
    /// `supernet` and everything under it, down to `depth`.
    #[must_use]
    pub fn of(supernet: &Supernet, depth: Depth) -> Self {
        SupernetNode {
            name: supernet.name.clone(),
            cidr: supernet.cidr,
            subnets: (depth >= Depth::Subnets).then(|| {
                supernet
                    .subnets
                    .iter()
                    .map(|s| SubnetNode::of(s, depth))
                    .collect()
            }),
        }
    }

    /// Just `supernet`, with `subnet` as its only child: the path to something that changed.
    #[must_use]
    pub fn path(supernet: &Supernet, subnet: SubnetNode) -> Self {
        SupernetNode {
            subnets: Some(vec![subnet]),
            ..SupernetNode::of(supernet, Depth::Supernets)
        }
    }
}

impl SubnetNode {
    #[must_use]
    pub fn of(subnet: &Subnet, depth: Depth) -> Self {
        SubnetNode {
            name: subnet.name.clone(),
            kind: subnet.kind,
            cidr: subnet.cidr,
            docker_range: subnet.docker_range,
            docker_gateway: subnet.docker_gateway,
            ips: (depth >= Depth::Ips)
                .then(|| subnet.ips.iter().map(|ip| IpNode::of(ip, depth)).collect()),
        }
    }

    /// Just `subnet`, with `ip` as its only child.
    #[must_use]
    pub fn path(subnet: &Subnet, ip: IpNode) -> Self {
        SubnetNode {
            ips: Some(vec![ip]),
            ..SubnetNode::of(subnet, Depth::Subnets)
        }
    }
}

impl IpNode {
    #[must_use]
    pub fn of(ip: &Ip, depth: Depth) -> Self {
        IpNode {
            name: ip.name.clone(),
            address: ip.address,
            tag: ip.tag.clone(),
            hosts: (depth >= Depth::Hosts).then(|| ip.hosts.clone()),
        }
    }
}

/// The body of every non-2xx response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}
