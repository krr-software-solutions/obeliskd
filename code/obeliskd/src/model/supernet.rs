use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};

use super::Name;

/// A logical range that subnets are carved from. Just a name and a CIDR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Supernet {
    pub name: Name,
    pub cidr: Ipv4Net,
}

impl Supernet {
    /// Whether the two ranges share any address.
    #[must_use]
    pub fn overlaps(&self, other: &Ipv4Net) -> bool {
        self.cidr.contains(&other.network()) || other.contains(&self.cidr.network())
    }
}

/// `cidr` names a range, not an address inside one: its host bits are all zero.
///
/// # Errors
///
/// The CIDR it should have been, e.g. `10.57.3.4/16` → `10.57.0.0/16`.
pub fn require_network(cidr: Ipv4Net) -> Result<Ipv4Net, String> {
    if cidr.addr() == cidr.network() {
        Ok(cidr)
    } else {
        Err(format!(
            "{cidr} isn't the start of its range (did you mean {}?)",
            cidr.trunc()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    #[test]
    fn overlap_is_symmetric_and_covers_nesting() {
        let obd = Supernet {
            name: Name::parse("obd").unwrap(),
            cidr: net("10.57.0.0/16"),
        };
        assert!(obd.overlaps(&net("10.57.22.0/24"))); // inside
        assert!(obd.overlaps(&net("10.0.0.0/8"))); // around
        assert!(obd.overlaps(&net("10.57.0.0/16"))); // equal
        assert!(!obd.overlaps(&net("10.58.0.0/16")));
        assert!(!obd.overlaps(&net("10.56.255.0/24")));
    }

    #[test]
    fn host_bits_must_be_zero() {
        assert!(require_network(net("10.57.0.0/16")).is_ok());
        assert_eq!(
            require_network(net("10.57.3.4/16")).unwrap_err(),
            "10.57.3.4/16 isn't the start of its range (did you mean 10.57.0.0/16?)"
        );
    }
}
