//! The OS's users and groups, as the daemon needs them for grants: names ↔ ids, and which groups
//! a user is in. Behind a trait so tests can make up their own.

use crate::{Platform, PlatformImpl};

pub trait Users: Send + Sync {
    fn user_id(&self, name: &str) -> Option<u32>;
    fn user_name(&self, uid: u32) -> Option<String>;
    fn group_id(&self, name: &str) -> Option<u32>;
    fn group_name(&self, gid: u32) -> Option<String>;
    /// Every group `uid` is in, primary included; empty for a uid with no account.
    fn groups_of(&self, uid: u32) -> Vec<u32>;
}

/// The real user and group databases (see [`PlatformImpl::groups_of`]).
pub struct SystemUsers;

impl Users for SystemUsers {
    fn user_id(&self, name: &str) -> Option<u32> {
        Platform::user_id(name)
    }

    fn user_name(&self, uid: u32) -> Option<String> {
        Platform::user_name(uid)
    }

    fn group_id(&self, name: &str) -> Option<u32> {
        Platform::group_id(name)
    }

    fn group_name(&self, gid: u32) -> Option<String> {
        Platform::group_name(gid)
    }

    fn groups_of(&self, uid: u32) -> Vec<u32> {
        Platform::groups_of(uid)
    }
}

#[cfg(test)]
pub mod fake {
    use super::Users;

    /// Made-up users (uid, name, groups) and groups (gid, name).
    #[derive(Default)]
    pub struct FakeUsers {
        pub users: Vec<(u32, &'static str, Vec<u32>)>,
        pub groups: Vec<(u32, &'static str)>,
    }

    impl Users for FakeUsers {
        fn user_id(&self, name: &str) -> Option<u32> {
            self.users.iter().find(|u| u.1 == name).map(|u| u.0)
        }

        fn user_name(&self, uid: u32) -> Option<String> {
            self.users
                .iter()
                .find(|u| u.0 == uid)
                .map(|u| u.1.to_string())
        }

        fn group_id(&self, name: &str) -> Option<u32> {
            self.groups.iter().find(|g| g.1 == name).map(|g| g.0)
        }

        fn group_name(&self, gid: u32) -> Option<String> {
            self.groups
                .iter()
                .find(|g| g.0 == gid)
                .map(|g| g.1.to_string())
        }

        fn groups_of(&self, uid: u32) -> Vec<u32> {
            self.users
                .iter()
                .find(|u| u.0 == uid)
                .map(|u| u.2.clone())
                .unwrap_or_default()
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn root_resolves_both_ways() {
        let users = SystemUsers;
        assert_eq!(users.user_id("root"), Some(0));
        assert_eq!(users.user_name(0).as_deref(), Some("root"));
        assert_eq!(users.group_name(0).as_deref(), Some("root"));
        assert!(users.groups_of(0).contains(&0));
        assert_eq!(users.user_id("no-such-user-obeliskd"), None);
        assert_eq!(users.groups_of(4_242_424), Vec::<u32>::new());
    }
}
