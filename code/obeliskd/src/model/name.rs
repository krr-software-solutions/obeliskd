use std::fmt;

use serde::{Deserialize, Serialize};

/// A validated name for anything obeliskd allocates (supernets, and later subnets and ips).
///
/// The rules are the hostname-label ones, so a name is always usable as a DNS label, a docker
/// network or k3d cluster name, and a k8s object name: lowercase letters, digits and `-`;
/// 1 to 63 characters; no `-` at either end; and no `--` in positions 3–4 (reserved for
/// punycode, `xn--`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

const MAX_LEN: usize = 63;

impl Name {
    /// # Errors
    ///
    /// Why `name` isn't a valid name.
    pub fn parse(name: &str) -> Result<Self, String> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_LEN {
            return Err(format!("{name:?} must be 1 to {MAX_LEN} characters"));
        }
        if !bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        {
            return Err(format!(
                "{name:?} may only contain lowercase letters, digits and '-'"
            ));
        }
        if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
            return Err(format!("{name:?} can't start or end with '-'"));
        }
        if bytes.get(2..4) == Some(b"--") {
            return Err(format!("{name:?} can't have '--' in positions 3–4"));
        }
        Ok(Name(name.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Name {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Name::parse(&name)
    }
}

impl From<Name> for String {
    fn from(name: Name) -> Self {
        name.0
    }
}

impl std::str::FromStr for Name {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Name::parse(name)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_hostname_labels() {
        for ok in [
            "obd",
            "k3d-dev",
            "a",
            "x--y",
            "my-local-prod-db",
            &"a".repeat(63),
        ] {
            assert!(Name::parse(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn rejects_everything_else() {
        for bad in [
            "",
            "Obd",
            "-obd",
            "obd-",
            "ab--cd",
            "xn--obd",
            "obd.internal",
            "o_d",
            &"a".repeat(64),
        ] {
            assert!(Name::parse(bad).is_err(), "{bad}");
        }
    }
}
