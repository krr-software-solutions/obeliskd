use std::net::Ipv4Addr;

use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};

use super::Name;

/// What a subnet's addresses are for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubnetKind {
    /// Owned by this machine: host apps bind them.
    Host,
    /// A Docker network: containers (k3d nodes) and MetalLB-pinned Services claim them.
    Docker,
}

/// A range carved from a supernet, with a kind. obeliskd always picks the range.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subnet {
    pub name: Name,
    pub kind: SubnetKind,
    pub cidr: Ipv4Net,
    /// Docker only: the slice Docker assigns to containers itself (`--ip-range`), at the start of
    /// the subnet. It also holds the gateway (`.1`). obeliskd only hands out addresses outside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_range: Option<Ipv4Net>,
    /// Docker only: the gateway, the host's address on the network (`--gateway`). Always the
    /// subnet's first usable address (`.1`), inside Docker's slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_gateway: Option<Ipv4Addr>,
}

/// The longest prefix a docker subnet's slice may have: a /30 still holds a gateway and one
/// container.
const MAX_DOCKER_PREFIX: u8 = 30;

/// The first free, aligned range of prefix length `size` inside `within`, avoiding `taken`.
///
/// # Errors
///
/// [`PlaceError::Invalid`] if `size` doesn't fit in `within`; [`PlaceError::Conflict`] if no range
/// of that size is free.
pub fn place(within: Ipv4Net, taken: &[Ipv4Net], size: u8) -> Result<Ipv4Net, PlaceError> {
    let Ok(mut candidates) = within.subnets(size) else {
        return Err(PlaceError::Invalid(format!(
            "--size {size} doesn't fit in {within} (use {} to 32)",
            within.prefix_len()
        )));
    };
    if size < within.prefix_len() {
        return Err(PlaceError::Invalid(format!(
            "--size {size} doesn't fit in {within} (use {} to 32)",
            within.prefix_len()
        )));
    }
    candidates
        .find(|candidate| !taken.iter().any(|t| overlaps(*t, *candidate)))
        .ok_or_else(|| PlaceError::Conflict(format!("no free /{size} left in {within}")))
}

/// A docker subnet's gateway: its first usable address (`.1` for a /24).
#[must_use]
pub fn docker_gateway(subnet: Ipv4Net) -> Ipv4Addr {
    Ipv4Addr::from_bits(subnet.network().to_bits() + 1)
}

/// Docker's slice of a docker subnet: the first `/reserve` of it.
///
/// # Errors
///
/// If the slice isn't strictly smaller than the subnet, or is smaller than a /30.
pub fn docker_range(subnet: Ipv4Net, reserve: u8) -> Result<Ipv4Net, String> {
    if reserve <= subnet.prefix_len() || reserve > MAX_DOCKER_PREFIX {
        let smallest = subnet.prefix_len() + 1;
        return Err(if smallest > MAX_DOCKER_PREFIX {
            format!(
                "{subnet} is too small for a docker subnet (use --size {} or less)",
                MAX_DOCKER_PREFIX - 1
            )
        } else {
            format!(
                "--reserve-size must be {smallest} to {MAX_DOCKER_PREFIX} for a /{} subnet",
                subnet.prefix_len()
            )
        });
    }
    Ipv4Net::new(subnet.network(), reserve).map_err(|e| e.to_string())
}

fn overlaps(a: Ipv4Net, b: Ipv4Net) -> bool {
    a.contains(&b.network()) || b.contains(&a.network())
}

/// Why [`place`] failed: bad input (400), or no room (409).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaceError {
    Invalid(String),
    Conflict(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    const OBD: &str = "10.57.0.0/16";

    #[test]
    fn takes_the_first_free_aligned_range() {
        let taken = [net("10.57.0.0/24"), net("10.57.2.0/23")];
        assert_eq!(place(net(OBD), &taken, 24), Ok(net("10.57.1.0/24")));
        assert_eq!(place(net(OBD), &taken, 23), Ok(net("10.57.4.0/23")));
        assert_eq!(place(net(OBD), &taken, 25), Ok(net("10.57.1.0/25")));
        assert_eq!(place(net(OBD), &[], 16), Ok(net(OBD)));
    }

    #[test]
    fn size_must_fit_and_have_room() {
        assert!(matches!(
            place(net(OBD), &[], 15),
            Err(PlaceError::Invalid(_))
        ));
        assert!(matches!(
            place(net(OBD), &[], 33),
            Err(PlaceError::Invalid(_))
        ));
        assert!(matches!(
            place(net(OBD), &[net(OBD)], 24),
            Err(PlaceError::Conflict(_))
        ));
    }

    #[test]
    fn docker_range_is_the_start_of_the_subnet() {
        assert_eq!(
            docker_range(net("10.57.22.0/24"), 26),
            Ok(net("10.57.22.0/26"))
        );
        assert_eq!(
            docker_range(net("10.57.22.0/24"), 30),
            Ok(net("10.57.22.0/30"))
        );
        assert!(docker_range(net("10.57.22.0/24"), 24).is_err());
        assert!(docker_range(net("10.57.22.0/24"), 31).is_err());
        assert!(
            docker_range(net("10.57.22.0/30"), 31)
                .unwrap_err()
                .contains("too small")
        );
    }

    #[test]
    fn docker_gateway_is_the_first_usable_address_inside_the_slice() {
        assert_eq!(
            docker_gateway(net("10.57.16.0/22")),
            Ipv4Addr::new(10, 57, 16, 1)
        );
        let range = docker_range(net("10.57.16.0/22"), 30).unwrap();
        assert!(range.contains(&docker_gateway(net("10.57.16.0/22"))));
    }
}
