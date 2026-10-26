//! `--format pretty`: what each client command prints for a person.

use std::fmt::Write;

use obeliskd::control::api::Health;
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

fn addresses(supernet: &Supernet) -> u64 {
    1u64 << (32 - u32::from(supernet.cidr.prefix_len()))
}
