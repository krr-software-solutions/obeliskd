use std::fmt;

use serde::{Deserialize, Serialize};

use super::Name;
use crate::dns::ZONE;

/// A full hostname under the zone, e.g. `app.obd.internal` or `api.app.obd.internal`.
///
/// Every label before the zone follows [`Name`]'s rules (lowercase LDH, at most 63 characters, no
/// `--` in positions 3–4), there's at least one, and the whole name is at most 253 characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Hostname(String);

const MAX_LEN: usize = 253;

impl Hostname {
    /// # Errors
    ///
    /// Why `name` isn't a valid hostname under the zone.
    pub fn parse(name: &str) -> Result<Self, String> {
        let suffix = format!(".{ZONE}");
        let Some(labels) = name.strip_suffix(&suffix) else {
            return Err(format!("{name:?} must be a full name ending in {suffix}"));
        };
        if name.len() > MAX_LEN {
            return Err(format!("{name:?} is longer than {MAX_LEN} characters"));
        }
        for label in labels.split('.') {
            Name::parse(label).map_err(|e| format!("{name:?}: {e}"))?;
        }
        Ok(Hostname(name.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Hostname {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Hostname::parse(&name)
    }
}

impl From<Hostname> for String {
    fn from(name: Hostname) -> Self {
        name.0
    }
}

impl std::str::FromStr for Hostname {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Hostname::parse(name)
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A hostname and the ip it resolves to, by name. On disk, hosts live on their ip (see
/// [`Ip::hosts`](super::Ip::hosts)); this is how the API names one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    pub name: Hostname,
    pub supernet: Name,
    pub subnet: Name,
    pub ip: Name,
}

impl Host {
    /// Whether it points at this ip.
    #[must_use]
    pub fn targets(&self, supernet: &Name, subnet: &Name, ip: &Name) -> bool {
        &self.supernet == supernet && &self.subnet == subnet && &self.ip == ip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_names_under_the_zone() {
        for ok in [
            "app.obd.internal",
            "api.app.obd.internal",
            "x--y.obd.internal",
        ] {
            assert!(Hostname::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "app",
            "obd.internal",
            ".obd.internal",
            "App.obd.internal",
            "a..b.obd.internal",
            "ab--cd.obd.internal",
            "app.obd.internal.",
            "app.example.com",
        ] {
            assert!(Hostname::parse(bad).is_err(), "{bad}");
        }
        let long = format!("{}.obd.internal", vec!["a".repeat(60); 4].join("."));
        assert!(
            Hostname::parse(&long)
                .unwrap_err()
                .contains("longer than 253")
        );
    }
}
