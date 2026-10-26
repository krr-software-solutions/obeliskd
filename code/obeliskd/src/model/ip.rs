use std::net::Ipv4Addr;

use ipnet::Ipv4Net;

use serde::{Deserialize, Serialize};

use super::{Hostname, Name, Subnet, SubnetKind};

/// One named address in a subnet. obeliskd always picks the address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ip {
    pub name: Name,
    pub address: Ipv4Addr,
    /// An optional group label ("sub-sub-net"), to find or remove a set of ips together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<Name>,
    /// Hostnames that resolve to this address, sorted. Unique across every subnet; the ip can't be
    /// removed while it has any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<Hostname>,
}

/// Whether obeliskd may hand `address` out of `subnet`.
///
/// - Never an address whose last octet is 0 or 255: some software treats those as special
///   regardless of the real mask.
/// - Docker subnets: never inside Docker's own slice (which also holds the gateway), and never the
///   subnet's network or broadcast address (it's a real network segment).
/// - Host subnets: anything else in the range (the whole range is local; it has no network or
///   broadcast address of its own).
#[must_use]
pub fn allocatable(subnet: &Subnet, address: Ipv4Addr) -> bool {
    let last = address.octets()[3];
    if last == 0 || last == 255 || !subnet.cidr.contains(&address) {
        return false;
    }
    match subnet.kind {
        SubnetKind::Host => true,
        SubnetKind::Docker => {
            address != subnet.cidr.network()
                && address != subnet.cidr.broadcast()
                && !subnet.docker_range.is_some_and(|r| r.contains(&address))
        }
    }
}

/// How many addresses in `cidr` don't end in `.0` or `.255`.
#[must_use]
pub fn usable_count(cidr: Ipv4Net) -> u64 {
    if cidr.prefix_len() <= 24 {
        // Whole /24s: 254 of every 256.
        (1u64 << (32 - cidr.prefix_len())) / 256 * 254
    } else {
        let (first, last) = (cidr.network().to_bits(), cidr.broadcast().to_bits());
        (first..=last)
            .filter(|&a| !matches!(a & 0xff, 0 | 255))
            .count() as u64
    }
}

/// How many addresses [`allocatable`] allows in a subnet of this shape, without walking them.
#[must_use]
pub fn allocatable_count(kind: SubnetKind, cidr: Ipv4Net, docker_range: Option<Ipv4Net>) -> u64 {
    let usable = usable_count(cidr);
    match kind {
        SubnetKind::Host => usable,
        SubnetKind::Docker => {
            let in_range = |a: Ipv4Addr| docker_range.is_some_and(|r| r.contains(&a));
            let ends_ok = |a: Ipv4Addr| !matches!(a.octets()[3], 0 | 255);
            let mut edges = vec![cidr.network()];
            if cidr.broadcast() != cidr.network() {
                edges.push(cidr.broadcast());
            }
            let edges = edges
                .into_iter()
                .filter(|a| ends_ok(*a) && !in_range(*a))
                .count() as u64;
            usable - docker_range.map_or(0, usable_count) - edges
        }
    }
}

/// The lowest allocatable address in `subnet` that none of its ips have.
#[must_use]
pub fn pick(subnet: &Subnet) -> Option<Ipv4Addr> {
    let first = subnet.cidr.network().to_bits();
    let last = subnet.cidr.broadcast().to_bits();
    (first..=last)
        .map(Ipv4Addr::from_bits)
        .find(|a| allocatable(subnet, *a) && !subnet.ips.iter().any(|ip| ip.address == *a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subnet(kind: SubnetKind, cidr: &str, docker_range: Option<&str>) -> Subnet {
        Subnet {
            name: Name::parse("s").unwrap(),
            kind,
            cidr: cidr.parse().unwrap(),
            docker_range: docker_range.map(|r| r.parse().unwrap()),
            docker_gateway: None,
            grants: Vec::new(),
            ips: Vec::new(),
        }
    }

    fn addr(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn counts_match_walking_every_address() {
        let walk = |s: &Subnet| {
            let (first, last) = (s.cidr.network().to_bits(), s.cidr.broadcast().to_bits());
            (first..=last)
                .filter(|&a| allocatable(s, Ipv4Addr::from_bits(a)))
                .count() as u64
        };
        for (kind, cidr, range) in [
            (SubnetKind::Host, "10.57.0.0/16", None),
            (SubnetKind::Host, "10.57.0.0/24", None),
            (SubnetKind::Host, "10.57.0.0/28", None),
            (SubnetKind::Host, "10.57.0.240/28", None),
            (SubnetKind::Host, "10.57.0.255/32", None),
            (SubnetKind::Docker, "10.57.0.0/24", Some("10.57.0.0/26")),
            (SubnetKind::Docker, "10.57.0.0/22", Some("10.57.0.0/24")),
            (SubnetKind::Docker, "10.57.0.16/28", Some("10.57.0.16/30")),
            (SubnetKind::Docker, "10.57.0.240/28", Some("10.57.0.240/30")),
        ] {
            let s = subnet(kind, cidr, range);
            assert_eq!(
                allocatable_count(kind, s.cidr, s.docker_range),
                walk(&s),
                "{kind} {cidr}"
            );
        }
        assert_eq!(usable_count("10.0.0.0/8".parse().unwrap()), 65_536 * 254);
    }

    #[test]
    fn host_subnets_start_at_dot_one_and_skip_0_and_255() {
        let mut host = subnet(SubnetKind::Host, "10.57.0.0/23", None);
        assert_eq!(pick(&host), Some(addr("10.57.0.1")));
        assert!(!allocatable(&host, addr("10.57.0.255")));
        assert!(!allocatable(&host, addr("10.57.1.0")));
        assert!(allocatable(&host, addr("10.57.1.1")));

        // Fill .1–.254 and the next one is in the second /24, past .255 and .0.
        host.ips = (1..=254)
            .map(|i| Ip {
                name: Name::parse(&format!("i{i}")).unwrap(),
                address: Ipv4Addr::new(10, 57, 0, i),
                tag: None,
                hosts: Vec::new(),
            })
            .collect();
        assert_eq!(pick(&host), Some(addr("10.57.1.1")));
    }

    #[test]
    fn small_host_subnets_keep_their_last_address() {
        // In a /28, .15 is an ordinary local address (no broadcast in a host subnet).
        let host = subnet(SubnetKind::Host, "10.57.0.0/28", None);
        assert!(allocatable(&host, addr("10.57.0.15")));
    }

    #[test]
    fn docker_subnets_start_after_dockers_slice() {
        let docker = subnet(SubnetKind::Docker, "10.57.1.0/24", Some("10.57.1.0/26"));
        assert_eq!(pick(&docker), Some(addr("10.57.1.64")));
        assert!(
            !allocatable(&docker, addr("10.57.1.1")),
            "the gateway is in Docker's slice"
        );
        assert!(!allocatable(&docker, addr("10.57.1.255")));

        // A /25 docker subnet: its broadcast (.127) is excluded even though it doesn't end in 255.
        let small = subnet(SubnetKind::Docker, "10.57.2.0/25", Some("10.57.2.0/26"));
        assert!(!allocatable(&small, addr("10.57.2.127")));
        assert!(allocatable(&small, addr("10.57.2.126")));
    }

    #[test]
    fn full_subnets_have_nothing_to_pick() {
        let mut tiny = subnet(SubnetKind::Docker, "10.57.3.0/29", Some("10.57.3.0/30"));
        // .4–.6 are allocatable (.7 is the broadcast).
        tiny.ips = (4..=6)
            .map(|i| Ip {
                name: Name::parse(&format!("i{i}")).unwrap(),
                address: Ipv4Addr::new(10, 57, 3, i),
                tag: None,
                hosts: Vec::new(),
            })
            .collect();
        assert_eq!(pick(&tiny), None);
    }
}
