//! Who may change what: a grant gives a user, a group or everyone a role on a scope (the whole
//! tree, a supernet, or a subnet), and covers everything inside it. Grants only ever add access.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::Name;

/// Who a grant is for. Stored by number, so renaming a user or group doesn't lose its grants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Principal {
    User(u32),
    Group(u32),
    /// Anyone who can connect to the control socket.
    Everyone,
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Principal::User(uid) => write!(f, "user:{uid}"),
            Principal::Group(gid) => write!(f, "group:{gid}"),
            Principal::Everyone => f.write_str("everyone"),
        }
    }
}

impl FromStr for Principal {
    type Err = String;

    /// `user:<uid>`, `group:<gid>` or `everyone`, as stored.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let id = |n: &str| {
            n.parse::<u32>()
                .map_err(|_| format!("{s:?}: expected a number after the colon"))
        };
        match s.split_once(':') {
            Some(("user", n)) => Ok(Principal::User(id(n)?)),
            Some(("group", n)) => Ok(Principal::Group(id(n)?)),
            None if s == "everyone" => Ok(Principal::Everyone),
            _ => Err(format!(
                "{s:?}: expected user:<uid>, group:<gid> or everyone"
            )),
        }
    }
}

impl Serialize for Principal {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Principal {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// What a grant allows, on everything inside its scope. Each role includes the ones before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Alloc and dealloc ips and hosts.
    Use,
    /// …and subnets.
    Manage,
    /// …and add and remove grants.
    Admin,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::Use => "use",
            Role::Manage => "manage",
            Role::Admin => "admin",
        })
    }
}

/// A role for someone; it lives on its scope (in that scope's state file).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub who: Principal,
    pub role: Role,
}

/// Where a grant applies: the whole tree, one supernet, or one subnet.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    All,
    Supernet(Name),
    Subnet(Name, Name),
}

impl Scope {
    /// From the CLI's / API's optional `--supernet` and `--subnet`.
    ///
    /// # Errors
    ///
    /// A subnet without its supernet.
    pub fn of(supernet: Option<Name>, subnet: Option<Name>) -> Result<Self, String> {
        match (supernet, subnet) {
            (None, None) => Ok(Scope::All),
            (Some(supernet), None) => Ok(Scope::Supernet(supernet)),
            (Some(supernet), Some(subnet)) => Ok(Scope::Subnet(supernet, subnet)),
            (None, Some(_)) => {
                Err("--subnet needs --supernet (subnet names are per supernet)".to_string())
            }
        }
    }

    #[must_use]
    pub fn supernet(&self) -> Option<&Name> {
        match self {
            Scope::All => None,
            Scope::Supernet(supernet) | Scope::Subnet(supernet, _) => Some(supernet),
        }
    }

    #[must_use]
    pub fn subnet(&self) -> Option<&Name> {
        match self {
            Scope::Subnet(_, subnet) => Some(subnet),
            _ => None,
        }
    }

    /// For messages: `the whole tree`, `supernet obd`, `subnet obd/dev`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Scope::All => "the whole tree".to_string(),
            Scope::Supernet(supernet) => format!("supernet {supernet}"),
            Scope::Subnet(supernet, subnet) => format!("subnet {supernet}/{subnet}"),
        }
    }

    /// Whether `self` is `other` or one of its parents, i.e. a grant on `self` covers `other`.
    #[must_use]
    pub fn covers(&self, other: &Scope) -> bool {
        match (self, other) {
            (Scope::All, _) => true,
            (Scope::Supernet(a), Scope::Supernet(b) | Scope::Subnet(b, _)) => a == b,
            (Scope::Subnet(a, x), Scope::Subnet(b, y)) => a == b && x == y,
            _ => false,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::All => f.write_str("*"),
            Scope::Supernet(supernet) => write!(f, "{supernet}"),
            Scope::Subnet(supernet, subnet) => write!(f, "{supernet}/{subnet}"),
        }
    }
}

/// Who is making a request: a user and every group they're in (looked up from the user database,
/// not just the one group the kernel reports for the connection).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actor {
    pub uid: u32,
    pub gids: Vec<u32>,
}

impl Actor {
    /// Root may do anything, with or without grants.
    pub const ROOT: Actor = Actor {
        uid: 0,
        gids: Vec::new(),
    };

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.uid == 0
    }

    /// Whether a grant for `who` applies to this actor.
    #[must_use]
    pub fn is(&self, who: Principal) -> bool {
        match who {
            Principal::User(uid) => uid == self.uid,
            Principal::Group(gid) => self.gids.contains(&gid),
            Principal::Everyone => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Name {
        Name::parse(s).unwrap()
    }

    #[test]
    fn principals_round_trip_as_text() {
        for who in [
            Principal::User(1000),
            Principal::Group(984),
            Principal::Everyone,
        ] {
            assert_eq!(who.to_string().parse::<Principal>(), Ok(who));
        }
        for bad in ["user", "user:kalin", "group:", "all", "everyone:1"] {
            assert!(bad.parse::<Principal>().is_err(), "{bad}");
        }
    }

    #[test]
    fn roles_include_the_ones_before() {
        assert!(Role::Use < Role::Manage && Role::Manage < Role::Admin);
    }

    #[test]
    fn scopes_cover_themselves_and_what_is_inside() {
        let all = Scope::All;
        let obd = Scope::Supernet(name("obd"));
        let dev = Scope::Subnet(name("obd"), name("dev"));
        let other = Scope::Subnet(name("ex"), name("dev"));
        assert!(all.covers(&all) && all.covers(&obd) && all.covers(&dev));
        assert!(obd.covers(&obd) && obd.covers(&dev) && !obd.covers(&other));
        assert!(dev.covers(&dev) && !dev.covers(&obd) && !dev.covers(&other));
        assert!(!obd.covers(&all));
    }

    #[test]
    fn actors_match_their_user_their_groups_and_everyone() {
        let actor = Actor {
            uid: 1000,
            gids: vec![1000, 984],
        };
        assert!(actor.is(Principal::User(1000)) && !actor.is(Principal::User(1001)));
        assert!(actor.is(Principal::Group(984)) && !actor.is(Principal::Group(27)));
        assert!(actor.is(Principal::Everyone));
    }
}
