//! `--format pretty`: what each client command prints for a person.
//!
//! Lists are tables with one column per level of the tree. A parent's columns are printed once, on
//! its first row, and left blank under it; a parent without children gets a row of `-`. An empty
//! list is an empty table.

use std::fmt::Write;

use ipnet::Ipv4Net;
use obeliskd::control::api::{Health, IpNode, SubnetNode, SupernetNode};
use obeliskd::model::SubnetKind;

pub fn health(health: &Health) -> String {
    let caller = &health.caller;
    let pid = caller
        .pid
        .map_or_else(|| "?".to_string(), |pid| pid.to_string());
    format!(
        "daemon   up since {}\n         answering DNS on {}\ncaller   uid {}, gid {}, pid {pid}\n",
        health.daemon.started_at, health.daemon.dns_address, caller.uid, caller.gid
    )
}

// ----- lists -----

pub fn supernets(supernets: &[SupernetNode]) -> String {
    let rows: Vec<[String; 3]> = supernets
        .iter()
        .map(|s| [s.name.to_string(), s.cidr.to_string(), size(s.cidr)])
        .collect();
    table(&["SUPERNET", "CIDR", "ADDRESSES"], &rows, &[])
}

pub fn subnets(supernets: &[SupernetNode]) -> String {
    let mut rows: Vec<[String; 7]> = Vec::new();
    for supernet in supernets {
        let subnets = supernet.subnets.as_deref().unwrap_or_default();
        if subnets.is_empty() {
            rows.push(dashes(&[supernet.name.to_string()]));
        }
        for subnet in subnets {
            let [name, kind, cidr, range, gateway, addresses] = subnet_cells(subnet);
            rows.push([
                supernet.name.to_string(),
                name,
                kind,
                cidr,
                range,
                gateway,
                addresses,
            ]);
        }
    }
    table(
        &[
            "SUPERNET",
            "SUBNET",
            "KIND",
            "CIDR",
            "DOCKER RANGE",
            "GATEWAY",
            "ADDRESSES",
        ],
        &rows,
        &[1],
    )
}

pub fn ips(supernets: &[SupernetNode]) -> String {
    let mut rows: Vec<[String; 6]> = Vec::new();
    for_each_ip(supernets, |parents, ip| {
        let [supernet, subnet, kind] = parents;
        rows.push(match ip {
            Some(ip) => [
                supernet,
                subnet,
                kind,
                ip.name.to_string(),
                ip.address.to_string(),
                or_dash(ip.tag.as_ref()),
            ],
            None => dashes(&[supernet, subnet, kind]),
        });
    });
    table(
        &["SUPERNET", "SUBNET", "KIND", "IP", "ADDRESS", "TAG"],
        &rows,
        &[1, 3],
    )
}

pub fn hosts(supernets: &[SupernetNode]) -> String {
    let mut rows: Vec<[String; 6]> = Vec::new();
    for_each_ip(supernets, |parents, ip| {
        let [supernet, subnet, kind] = parents;
        let Some(ip) = ip else {
            rows.push(dashes(&[supernet, subnet, kind]));
            return;
        };
        let (name, address) = (ip.name.to_string(), ip.address.to_string());
        let hosts = ip.hosts.as_deref().unwrap_or_default();
        if hosts.is_empty() {
            rows.push(dashes(&[supernet, subnet, kind, name, address]));
            return;
        }
        for host in hosts {
            rows.push([
                supernet.clone(),
                subnet.clone(),
                kind.clone(),
                name.clone(),
                address.clone(),
                host.to_string(),
            ]);
        }
    });
    table(
        &["SUPERNET", "SUBNET", "KIND", "IP", "ADDRESS", "HOST"],
        &rows,
        &[1, 3, 5],
    )
}

/// Calls `f` with `[supernet, subnet, kind]` and each ip, or `None` where a subnet has no ips;
/// a supernet without subnets comes through as `[supernet, "-", "-"]` and `None`.
fn for_each_ip(supernets: &[SupernetNode], mut f: impl FnMut([String; 3], Option<&IpNode>)) {
    for supernet in supernets {
        let subnets = supernet.subnets.as_deref().unwrap_or_default();
        if subnets.is_empty() {
            f([supernet.name.to_string(), "-".into(), "-".into()], None);
        }
        for subnet in subnets {
            let parents = || {
                [
                    supernet.name.to_string(),
                    subnet.name.to_string(),
                    kind(subnet.kind).to_string(),
                ]
            };
            let ips = subnet.ips.as_deref().unwrap_or_default();
            if ips.is_empty() {
                f(parents(), None);
            }
            for ip in ips {
                f(parents(), Some(ip));
            }
        }
    }
}

// ----- changes -----

pub fn supernet_allocated(supernet: &SupernetNode) -> String {
    format!(
        "allocated supernet {} ({}, {} addresses)\n",
        supernet.name,
        supernet.cidr,
        size(supernet.cidr)
    )
}

pub fn supernet_deallocated(supernet: &SupernetNode) -> String {
    let mut out = format!(
        "deallocated supernet {} ({})\n",
        supernet.name, supernet.cidr
    );
    for subnet in supernet.subnets.as_deref().unwrap_or_default() {
        let path = format!("{}/{}", supernet.name, subnet.name);
        let _ = writeln!(
            out,
            "  and its {} subnet {path} ({})",
            kind(subnet.kind),
            subnet.cidr
        );
        for ip in subnet.ips.as_deref().unwrap_or_default() {
            let _ = writeln!(out, "  and its ip {path}/{} ({})", ip.name, ip.address);
        }
    }
    out
}

pub fn subnet_allocated(supernet: &SupernetNode) -> String {
    let Some(subnet) = only_subnet(supernet) else {
        return String::new();
    };
    let docker = match (subnet.docker_range, subnet.docker_gateway) {
        (Some(range), Some(gateway)) => {
            format!("; gateway {gateway}, Docker assigns {range}, obeliskd the rest")
        }
        _ => String::new(),
    };
    format!(
        "allocated {} subnet {}/{} ({}, {} addresses{docker})\n",
        kind(subnet.kind),
        supernet.name,
        subnet.name,
        subnet.cidr,
        size(subnet.cidr)
    )
}

pub fn subnet_deallocated(supernet: &SupernetNode) -> String {
    let Some(subnet) = only_subnet(supernet) else {
        return String::new();
    };
    let path = format!("{}/{}", supernet.name, subnet.name);
    let mut out = format!(
        "deallocated {} subnet {path} ({})\n",
        kind(subnet.kind),
        subnet.cidr
    );
    for ip in subnet.ips.as_deref().unwrap_or_default() {
        let _ = writeln!(out, "  and its ip {path}/{} ({})", ip.name, ip.address);
    }
    out
}

pub fn ip_allocated(supernet: &SupernetNode) -> String {
    ip_change("allocated", supernet)
}

pub fn ip_deallocated(supernet: &SupernetNode) -> String {
    ip_change("deallocated", supernet)
}

fn ip_change(verb: &str, supernet: &SupernetNode) -> String {
    let Some((subnet, ip)) = only_ip(supernet) else {
        return String::new();
    };
    let tag = ip
        .tag
        .as_ref()
        .map(|t| format!(" (tag {t})"))
        .unwrap_or_default();
    format!(
        "{verb} ip {}/{}/{} = {}{tag}\n",
        supernet.name, subnet.name, ip.name, ip.address
    )
}

pub fn host_allocated(supernet: &SupernetNode) -> String {
    let Some((subnet, ip)) = only_ip(supernet) else {
        return String::new();
    };
    let host = ip.hosts.as_deref().unwrap_or_default().first();
    format!(
        "allocated host {} → ip {}/{}/{} ({})\n",
        host.map_or_else(String::new, ToString::to_string),
        supernet.name,
        subnet.name,
        ip.name,
        ip.address
    )
}

pub fn host_deallocated(supernet: &SupernetNode) -> String {
    let host = only_ip(supernet)
        .and_then(|(_, ip)| ip.hosts.as_deref().unwrap_or_default().first())
        .map_or_else(String::new, ToString::to_string);
    format!("deallocated host {host}\n")
}

/// A change's response is the path to what changed: its single subnet.
fn only_subnet(supernet: &SupernetNode) -> Option<&SubnetNode> {
    supernet.subnets.as_deref()?.first()
}

/// …and, a level down, its single ip.
fn only_ip(supernet: &SupernetNode) -> Option<(&SubnetNode, &IpNode)> {
    let subnet = only_subnet(supernet)?;
    Some((subnet, subnet.ips.as_deref()?.first()?))
}

// ----- helpers -----

fn subnet_cells(subnet: &SubnetNode) -> [String; 6] {
    [
        subnet.name.to_string(),
        kind(subnet.kind).to_string(),
        subnet.cidr.to_string(),
        or_dash(subnet.docker_range),
        or_dash(subnet.docker_gateway),
        size(subnet.cidr),
    ]
}

/// A row that starts with `filled` and is `-` for the rest.
fn dashes<const N: usize>(filled: &[String]) -> [String; N] {
    std::array::from_fn(|i| filled.get(i).cloned().unwrap_or_else(|| "-".to_string()))
}

fn or_dash(value: Option<impl ToString>) -> String {
    value.map_or_else(|| "-".to_string(), |v| v.to_string())
}

fn kind(kind: SubnetKind) -> &'static str {
    match kind {
        SubnetKind::Host => "host",
        SubnetKind::Docker => "docker",
    }
}

fn size(cidr: Ipv4Net) -> String {
    (1u64 << (32 - u32::from(cidr.prefix_len()))).to_string()
}

/// Left-aligned columns, two spaces apart.
///
/// `groups` splits the leading columns into tree levels by where each level ends (e.g. `[1, 3]`:
/// column 0 is the supernet, columns 1–2 the subnet). A level's cells are left blank when they
/// repeat the row above, along with everything before them, so each parent shows once.
fn table<const N: usize>(headers: &[&str; N], rows: &[[String; N]], groups: &[usize]) -> String {
    let mut shown: Vec<[String; N]> = rows.to_vec();
    for i in 1..rows.len() {
        for &end in groups {
            if rows[i][..end] == rows[i - 1][..end] {
                let start = groups
                    .iter()
                    .copied()
                    .filter(|&e| e < end)
                    .max()
                    .unwrap_or(0);
                for cell in &mut shown[i][start..end] {
                    cell.clear();
                }
            }
        }
    }

    let widths: Vec<usize> = (0..N)
        .map(|i| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain([headers[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: &[&str]| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:w$}"))
            .collect();
        format!("{}\n", padded.join("  ").trim_end())
    };
    let mut out = line(headers);
    for row in &shown {
        out.push_str(&line(&row.iter().map(String::as_str).collect::<Vec<_>>()));
    }
    out
}

#[cfg(test)]
mod tests {
    use obeliskd::model::{Hostname, Name};

    use super::*;

    fn name(s: &str) -> Name {
        Name::parse(s).unwrap()
    }

    fn ip(n: &str, address: &str, hosts: &[&str]) -> IpNode {
        IpNode {
            name: name(n),
            address: address.parse().unwrap(),
            tag: (n == "db-2").then(|| name("pr-42")),
            hosts: Some(hosts.iter().map(|h| Hostname::parse(h).unwrap()).collect()),
        }
    }

    /// ex1/1: a host subnet with three ips (one with two hosts); ex1/dev: a docker subnet with no
    /// ips; ex2: no subnets.
    fn tree() -> Vec<SupernetNode> {
        let one = SubnetNode {
            name: name("1"),
            kind: SubnetKind::Host,
            cidr: "10.57.0.0/24".parse().unwrap(),
            docker_range: None,
            docker_gateway: None,
            ips: Some(vec![
                ip("db-2", "10.57.0.2", &["a.obd.internal", "b.obd.internal"]),
                ip("db-3", "10.57.0.1", &[]),
                ip("db-4", "10.57.0.3", &[]),
            ]),
        };
        let dev = SubnetNode {
            name: name("dev"),
            kind: SubnetKind::Docker,
            cidr: "10.57.1.0/24".parse().unwrap(),
            docker_range: Some("10.57.1.0/26".parse().unwrap()),
            docker_gateway: Some("10.57.1.1".parse().unwrap()),
            ips: Some(Vec::new()),
        };
        vec![
            SupernetNode {
                name: name("ex1"),
                cidr: "10.57.0.0/16".parse().unwrap(),
                subnets: Some(vec![one, dev]),
            },
            SupernetNode {
                name: name("ex2"),
                cidr: "10.58.0.0/16".parse().unwrap(),
                subnets: Some(Vec::new()),
            },
        ]
    }

    #[test]
    fn empty_lists_are_empty_tables() {
        assert_eq!(supernets(&[]), "SUPERNET  CIDR  ADDRESSES\n");
        assert_eq!(
            subnets(&[]),
            "SUPERNET  SUBNET  KIND  CIDR  DOCKER RANGE  GATEWAY  ADDRESSES\n"
        );
        assert_eq!(ips(&[]), "SUPERNET  SUBNET  KIND  IP  ADDRESS  TAG\n");
        assert_eq!(hosts(&[]), "SUPERNET  SUBNET  KIND  IP  ADDRESS  HOST\n");
    }

    #[test]
    fn subnets_show_each_supernet_once() {
        assert_eq!(
            subnets(&tree()),
            "\
SUPERNET  SUBNET  KIND    CIDR          DOCKER RANGE  GATEWAY    ADDRESSES
ex1       1       host    10.57.0.0/24  -             -          256
          dev     docker  10.57.1.0/24  10.57.1.0/26  10.57.1.1  256
ex2       -       -       -             -             -          -
"
        );
    }

    #[test]
    fn ips_show_each_supernet_and_subnet_once() {
        assert_eq!(
            ips(&tree()),
            "\
SUPERNET  SUBNET  KIND    IP    ADDRESS    TAG
ex1       1       host    db-2  10.57.0.2  pr-42
                          db-3  10.57.0.1  -
                          db-4  10.57.0.3  -
          dev     docker  -     -          -
ex2       -       -       -     -          -
"
        );
    }

    #[test]
    fn hosts_show_every_ip_and_each_parent_once() {
        assert_eq!(
            hosts(&tree()),
            "\
SUPERNET  SUBNET  KIND    IP    ADDRESS    HOST
ex1       1       host    db-2  10.57.0.2  a.obd.internal
                                           b.obd.internal
                          db-3  10.57.0.1  -
                          db-4  10.57.0.3  -
          dev     docker  -     -          -
ex2       -       -       -     -          -
"
        );
    }
}
