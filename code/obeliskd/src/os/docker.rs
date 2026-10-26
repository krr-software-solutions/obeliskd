//! Docker networks for docker subnets, through the `docker` CLI.

use std::collections::BTreeMap;

use ipnet::Ipv4Net;
use serde::Deserialize;

use super::command::{self, args};
use crate::ObeliskdError;
use crate::model::{Name, Subnet};

const DOCKER: &str = "docker";

/// Every network obeliskd creates carries this label, so it never touches anyone else's.
pub const MANAGED_LABEL: &str = "obeliskd.managed";

/// The network a docker subnet becomes: `obd-<supernet>-<subnet>`.
#[must_use]
pub fn network_name(supernet: &Name, subnet: &Subnet) -> String {
    format!("obd-{supernet}-{}", subnet.name)
}

/// `docker network create …` for this subnet.
#[must_use]
pub fn create_args(supernet: &Name, subnet: &Subnet) -> Vec<String> {
    let mut a = args(&["network", "create", "--driver", "bridge"]);
    a.extend(["--subnet".to_string(), subnet.cidr.to_string()]);
    if let Some(gateway) = subnet.docker_gateway {
        a.extend(["--gateway".to_string(), gateway.to_string()]);
    }
    if let Some(range) = subnet.docker_range {
        a.extend(["--ip-range".to_string(), range.to_string()]);
    }
    for label in [
        format!("{MANAGED_LABEL}=true"),
        format!("obeliskd.supernet={supernet}"),
        format!("obeliskd.subnet={}", subnet.name),
    ] {
        a.extend(["--label".to_string(), label]);
    }
    a.push(network_name(supernet, subnet));
    a
}

/// Creates the network, or checks that the one already there is obeliskd's and matches.
///
/// # Errors
///
/// [`ObeliskdError::Conflict`] if a network of that name exists but isn't obeliskd's or doesn't
/// match; [`ObeliskdError::Os`] if Docker refuses (e.g. the range overlaps another network).
pub fn ensure_network(supernet: &Name, subnet: &Subnet) -> Result<(), ObeliskdError> {
    let name = network_name(supernet, subnet);
    let inspect = args(&["network", "inspect", &name]);
    let output = command::run(DOCKER, &inspect)?;
    if output.success {
        let existing = parse_inspect(&output.stdout).map_err(|message| ObeliskdError::Os {
            command: format!("docker network inspect {name}"),
            message,
        })?;
        return check_matches(&name, subnet, &existing);
    }
    if !output.stderr.contains("not found") {
        return Err(command::failed(DOCKER, &inspect, &output));
    }
    command::run_ok(DOCKER, &create_args(supernet, subnet), &[])
}

/// Removes the network; already gone is fine.
///
/// # Errors
///
/// [`ObeliskdError::Conflict`] if containers are still attached; [`ObeliskdError::Os`] otherwise.
pub fn remove_network(supernet: &Name, subnet: &Subnet) -> Result<(), ObeliskdError> {
    let name = network_name(supernet, subnet);
    let rm = args(&["network", "rm", &name]);
    let output = command::run(DOCKER, &rm)?;
    if output.success || output.stderr.contains("not found") {
        Ok(())
    } else if output.stderr.contains("active endpoints") {
        Err(ObeliskdError::Conflict(format!(
            "docker network {name} still has containers attached (remove them first): {}",
            output.stderr
        )))
    } else {
        Err(command::failed(DOCKER, &rm, &output))
    }
}

/// The parts of `docker network inspect` obeliskd checks.
#[derive(Debug, Deserialize)]
pub struct Inspected {
    #[serde(rename = "IPAM")]
    pub ipam: Ipam,
    #[serde(rename = "Labels", default)]
    pub labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
pub struct Ipam {
    #[serde(rename = "Config", default)]
    pub config: Vec<IpamConfig>,
}

#[derive(Debug, Deserialize)]
pub struct IpamConfig {
    #[serde(rename = "Subnet")]
    pub subnet: Option<String>,
    #[serde(rename = "Gateway")]
    pub gateway: Option<String>,
    #[serde(rename = "IPRange")]
    pub ip_range: Option<String>,
}

/// `docker network inspect` prints a JSON array with one network.
///
/// # Errors
///
/// If it isn't that.
pub fn parse_inspect(json: &str) -> Result<Inspected, String> {
    let mut networks: Vec<Inspected> =
        serde_json::from_str(json).map_err(|e| format!("unexpected output: {e}"))?;
    if networks.len() == 1 {
        Ok(networks.remove(0))
    } else {
        Err(format!("expected one network, got {}", networks.len()))
    }
}

/// # Errors
///
/// [`ObeliskdError::Conflict`] describing how the existing network differs.
pub fn check_matches(
    name: &str,
    subnet: &Subnet,
    existing: &Inspected,
) -> Result<(), ObeliskdError> {
    let managed = existing
        .labels
        .as_ref()
        .is_some_and(|l| l.get(MANAGED_LABEL).is_some_and(|v| v == "true"));
    if !managed {
        return Err(ObeliskdError::Conflict(format!(
            "a docker network named {name} already exists and isn't obeliskd's"
        )));
    }
    let config = existing.ipam.config.first();
    let found = |f: fn(&IpamConfig) -> &Option<String>| config.and_then(|c| f(c).clone());
    let want = |v: Option<String>| v;
    let checks = [
        (
            "subnet",
            found(|c| &c.subnet),
            want(Some(subnet.cidr.to_string())),
        ),
        (
            "gateway",
            found(|c| &c.gateway),
            want(subnet.docker_gateway.map(|g| g.to_string())),
        ),
        (
            "ip-range",
            found(|c| &c.ip_range),
            want(subnet.docker_range.map(|r: Ipv4Net| r.to_string())),
        ),
    ];
    for (what, found, want) in checks {
        if found != want {
            return Err(ObeliskdError::Conflict(format!(
                "docker network {name} has {what} {} but obeliskd expects {}; remove it with `docker network rm {name}` and restart the daemon",
                found.as_deref().unwrap_or("(none)"),
                want.as_deref().unwrap_or("(none)")
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SubnetKind;

    fn dev() -> (Name, Subnet) {
        (
            Name::parse("ex1").unwrap(),
            Subnet {
                name: Name::parse("k3d-dev").unwrap(),
                kind: SubnetKind::Docker,
                cidr: "10.57.22.0/24".parse().unwrap(),
                docker_range: Some("10.57.22.0/26".parse().unwrap()),
                docker_gateway: Some("10.57.22.1".parse().unwrap()),
                ips: Vec::new(),
            },
        )
    }

    #[test]
    fn create_args_spell_out_the_whole_network() {
        let (supernet, subnet) = dev();
        assert_eq!(
            create_args(&supernet, &subnet).join(" "),
            "network create --driver bridge --subnet 10.57.22.0/24 --gateway 10.57.22.1 \
             --ip-range 10.57.22.0/26 --label obeliskd.managed=true --label obeliskd.supernet=ex1 \
             --label obeliskd.subnet=k3d-dev obd-ex1-k3d-dev"
        );
    }

    fn inspected(labels: &str, ip_range: &str) -> Inspected {
        parse_inspect(&format!(
            r#"[{{"Name":"obd-ex1-k3d-dev","IPAM":{{"Driver":"default","Config":[{{"Subnet":"10.57.22.0/24","IPRange":"{ip_range}","Gateway":"10.57.22.1"}}]}},"Labels":{labels}}}]"#
        ))
        .unwrap()
    }

    #[test]
    fn an_existing_matching_network_is_fine() {
        let (_, subnet) = dev();
        let existing = inspected(r#"{"obeliskd.managed":"true"}"#, "10.57.22.0/26");
        assert!(check_matches("obd-ex1-k3d-dev", &subnet, &existing).is_ok());
    }

    #[test]
    fn someone_elses_or_a_drifted_network_is_a_conflict() {
        let (_, subnet) = dev();
        let foreign = inspected(r#"{"app":"k3d"}"#, "10.57.22.0/26");
        let error = check_matches("obd-ex1-k3d-dev", &subnet, &foreign).unwrap_err();
        assert!(error.to_string().contains("isn't obeliskd's"), "{error}");

        let drifted = inspected(r#"{"obeliskd.managed":"true"}"#, "10.57.22.0/25");
        let error = check_matches("obd-ex1-k3d-dev", &subnet, &drifted).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("ip-range 10.57.22.0/25 but obeliskd expects 10.57.22.0/26"),
            "{error}"
        );
    }

    /// Against the real Docker daemon: `cargo test -- --ignored` (needs docker access).
    #[test]
    #[ignore = "needs a Docker daemon"]
    fn real_docker_round_trip() {
        let supernet = Name::parse("obdtest").unwrap();
        let subnet = Subnet {
            name: Name::parse("roundtrip").unwrap(),
            kind: SubnetKind::Docker,
            cidr: "10.255.251.0/24".parse().unwrap(),
            docker_range: Some("10.255.251.0/26".parse().unwrap()),
            docker_gateway: Some("10.255.251.1".parse().unwrap()),
            ips: Vec::new(),
        };
        let _ = remove_network(&supernet, &subnet); // leftovers from a failed run

        ensure_network(&supernet, &subnet).unwrap();
        ensure_network(&supernet, &subnet).unwrap(); // idempotent: exists and matches
        let drifted = Subnet {
            docker_range: Some("10.255.251.0/25".parse().unwrap()),
            ..subnet.clone()
        };
        let error = ensure_network(&supernet, &drifted).unwrap_err();
        assert!(
            error.to_string().contains("ip-range 10.255.251.0/26"),
            "{error}"
        );

        remove_network(&supernet, &subnet).unwrap();
        remove_network(&supernet, &subnet).unwrap(); // idempotent: already gone
    }
}
