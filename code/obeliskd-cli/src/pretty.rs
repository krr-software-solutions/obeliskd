//! `--format pretty`: what each client command prints for a person.

use std::fmt::Write;

use obeliskd::control::api::{Health, HostEntry, IpEntry, SubnetEntry};
use obeliskd::model::SubnetKind;
use obeliskd::model::Supernet;

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

pub fn supernets(supernets: &[Supernet]) -> String {
    if supernets.is_empty() {
        return "no supernets yet (create one with `obeliskd supernet alloc`)\n".to_string();
    }
    let name_width = supernets
        .iter()
        .map(|s| s.name.as_str().len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let cidr_width = supernets
        .iter()
        .map(|s| s.cidr.to_string().len())
        .max()
        .unwrap_or(0)
        .max("CIDR".len());

    let mut out = format!(
        "{:name_width$}  {:cidr_width$}  ADDRESSES\n",
        "NAME", "CIDR"
    );
    for s in supernets {
        let _ = writeln!(
            out,
            "{:name_width$}  {:cidr_width$}  {}",
            s.name.as_str(),
            s.cidr.to_string(),
            addresses(s)
        );
    }
    out
}

pub fn supernet_allocated(supernet: &Supernet) -> String {
    format!(
        "allocated supernet {} ({}, {} addresses)\n",
        supernet.name,
        supernet.cidr,
        addresses(supernet)
    )
}

pub fn subnets(subnets: &[SubnetEntry]) -> String {
    if subnets.is_empty() {
        return "no subnets yet (create one with `obeliskd subnet alloc`)\n".to_string();
    }
    let rows: Vec<[String; 6]> = subnets
        .iter()
        .map(|e| {
            [
                format!("{}/{}", e.supernet, e.subnet.name),
                kind(e.subnet.kind).to_string(),
                e.subnet.cidr.to_string(),
                e.subnet
                    .docker_range
                    .map_or_else(|| "-".to_string(), |r| r.to_string()),
                e.subnet
                    .docker_gateway
                    .map_or_else(|| "-".to_string(), |g| g.to_string()),
                size(e.subnet.cidr).to_string(),
            ]
        })
        .collect();
    table(
        &[
            "SUBNET",
            "KIND",
            "CIDR",
            "DOCKER RANGE",
            "GATEWAY",
            "ADDRESSES",
        ],
        &rows,
    )
}

pub fn subnet_allocated(entry: &SubnetEntry) -> String {
    let subnet = &entry.subnet;
    let docker = match (subnet.docker_range, subnet.docker_gateway) {
        (Some(range), Some(gateway)) => {
            format!("; gateway {gateway}, Docker assigns {range}, obeliskd the rest")
        }
        _ => String::new(),
    };
    format!(
        "allocated {} subnet {}/{} ({}, {} addresses{docker})\n",
        kind(subnet.kind),
        entry.supernet,
        subnet.name,
        subnet.cidr,
        size(subnet.cidr)
    )
}

pub fn supernet_deallocated(supernet: &Supernet) -> String {
    let mut out = format!(
        "deallocated supernet {} ({})\n",
        supernet.name, supernet.cidr
    );
    for subnet in &supernet.subnets {
        let _ = writeln!(
            out,
            "  and its {} subnet {}/{} ({})",
            kind(subnet.kind),
            supernet.name,
            subnet.name,
            subnet.cidr
        );
    }
    out
}

pub fn subnet_deallocated(entry: &SubnetEntry) -> String {
    let subnet = &entry.subnet;
    let mut out = format!(
        "deallocated {} subnet {}/{} ({})\n",
        kind(subnet.kind),
        entry.supernet,
        subnet.name,
        subnet.cidr
    );
    for ip in &subnet.ips {
        let _ = writeln!(
            out,
            "  and its ip {}/{}/{} ({})",
            entry.supernet, subnet.name, ip.name, ip.address
        );
    }
    out
}

pub fn ips(ips: &[IpEntry]) -> String {
    if ips.is_empty() {
        return "no ips yet (create one with `obeliskd ip alloc`)\n".to_string();
    }
    let rows: Vec<[String; 3]> = ips
        .iter()
        .map(|ip| {
            [
                format!("{}/{}/{}", ip.supernet, ip.subnet, ip.name),
                ip.address.to_string(),
                kind(ip.kind).to_string(),
            ]
        })
        .collect();
    table(&["IP", "ADDRESS", "KIND"], &rows)
}

pub fn ip_allocated(ip: &IpEntry) -> String {
    format!(
        "allocated ip {}/{}/{} = {}\n",
        ip.supernet, ip.subnet, ip.name, ip.address
    )
}

pub fn ip_deallocated(ip: &IpEntry) -> String {
    format!(
        "deallocated ip {}/{}/{} ({})\n",
        ip.supernet, ip.subnet, ip.name, ip.address
    )
}

pub fn hosts(hosts: &[HostEntry]) -> String {
    if hosts.is_empty() {
        return "no hosts yet (create one with `obeliskd host alloc`)\n".to_string();
    }
    let rows: Vec<[String; 3]> = hosts
        .iter()
        .map(|h| {
            [
                h.name.to_string(),
                format!("{}/{}/{}", h.supernet, h.subnet, h.ip),
                h.address.to_string(),
            ]
        })
        .collect();
    table(&["HOST", "IP", "ADDRESS"], &rows)
}

pub fn host_allocated(host: &HostEntry) -> String {
    format!(
        "allocated host {} → ip {}/{}/{} ({})\n",
        host.name, host.supernet, host.subnet, host.ip, host.address
    )
}

pub fn host_deallocated(host: &HostEntry) -> String {
    format!("deallocated host {}\n", host.name)
}

fn kind(kind: SubnetKind) -> &'static str {
    match kind {
        SubnetKind::Host => "host",
        SubnetKind::Docker => "docker",
    }
}

/// Left-aligned columns, two spaces apart.
fn table<const N: usize>(headers: &[&str; N], rows: &[[String; N]]) -> String {
    let widths: Vec<usize> = (0..N)
        .map(|i| {
            rows.iter()
                .map(|r| r[i].len())
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
    for row in rows {
        out.push_str(&line(&row.iter().map(String::as_str).collect::<Vec<_>>()));
    }
    out
}

fn addresses(supernet: &Supernet) -> u64 {
    size(supernet.cidr)
}

fn size(cidr: ipnet::Ipv4Net) -> u64 {
    1u64 << (32 - u32::from(cidr.prefix_len()))
}
