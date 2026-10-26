//! Making the allocation model real on the OS.
//!
//! `config.toml` is the desired state; [`OsEffects`] makes the OS match it. Every operation is
//! idempotent (ensure present / ensure absent), so it's safe to re-run, e.g. at every daemon start.
//! The real implementation is [`SystemOs`]; tests use a recording fake.

pub mod command;
pub mod docker;

use crate::model::{Name, Subnet, SubnetKind, Supernet};
use crate::{ObeliskdError, Platform, PlatformImpl};

/// What the store asks of the OS.
pub trait OsEffects: Send + Sync {
    /// # Errors
    ///
    /// If the OS refuses (see [`ObeliskdError::Os`]).
    fn ensure_supernet(&self, supernet: &Supernet) -> Result<(), ObeliskdError>;

    /// # Errors
    ///
    /// If the OS refuses (see [`ObeliskdError::Os`]).
    fn remove_supernet(&self, supernet: &Supernet) -> Result<(), ObeliskdError>;

    /// # Errors
    ///
    /// If the OS refuses (see [`ObeliskdError::Os`]), or a docker network of that name exists but
    /// isn't obeliskd's or doesn't match ([`ObeliskdError::Conflict`]).
    fn ensure_subnet(&self, supernet: &Name, subnet: &Subnet) -> Result<(), ObeliskdError>;

    /// # Errors
    ///
    /// If the OS refuses, e.g. Docker won't remove a network that still has containers
    /// ([`ObeliskdError::Conflict`]).
    fn remove_subnet(&self, supernet: &Name, subnet: &Subnet) -> Result<(), ObeliskdError>;
}

/// The real thing: routes through [`PlatformImpl`], docker networks through the `docker` CLI.
pub struct SystemOs;

impl OsEffects for SystemOs {
    fn ensure_supernet(&self, supernet: &Supernet) -> Result<(), ObeliskdError> {
        Platform::ensure_supernet_route(supernet.cidr)
    }

    fn remove_supernet(&self, supernet: &Supernet) -> Result<(), ObeliskdError> {
        Platform::remove_supernet_route(supernet.cidr)
    }

    fn ensure_subnet(&self, supernet: &Name, subnet: &Subnet) -> Result<(), ObeliskdError> {
        match subnet.kind {
            SubnetKind::Host => Platform::ensure_host_subnet(subnet.cidr),
            SubnetKind::Docker => docker::ensure_network(supernet, subnet),
        }
    }

    fn remove_subnet(&self, supernet: &Name, subnet: &Subnet) -> Result<(), ObeliskdError> {
        match subnet.kind {
            SubnetKind::Host => Platform::remove_host_subnet(subnet.cidr),
            SubnetKind::Docker => docker::remove_network(supernet, subnet),
        }
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)] // a test fake: a poisoned mutex should panic
pub mod fake {
    use std::sync::Mutex;

    use super::*;

    /// Records what it was asked to do; can be told to fail the next call.
    #[derive(Default)]
    pub struct RecordingOs {
        pub calls: Mutex<Vec<String>>,
        pub fail_next: Mutex<Option<ObeliskdError>>,
    }

    impl RecordingOs {
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, call: String) -> Result<(), ObeliskdError> {
            if let Some(error) = self.fail_next.lock().unwrap().take() {
                return Err(error);
            }
            self.calls.lock().unwrap().push(call);
            Ok(())
        }
    }

    impl OsEffects for RecordingOs {
        fn ensure_supernet(&self, supernet: &Supernet) -> Result<(), ObeliskdError> {
            self.record(format!("ensure supernet {}", supernet.cidr))
        }

        fn remove_supernet(&self, supernet: &Supernet) -> Result<(), ObeliskdError> {
            self.record(format!("remove supernet {}", supernet.cidr))
        }

        fn ensure_subnet(&self, supernet: &Name, subnet: &Subnet) -> Result<(), ObeliskdError> {
            self.record(format!(
                "ensure subnet {supernet}/{} {}",
                subnet.name, subnet.cidr
            ))
        }

        fn remove_subnet(&self, supernet: &Name, subnet: &Subnet) -> Result<(), ObeliskdError> {
            self.record(format!(
                "remove subnet {supernet}/{} {}",
                subnet.name, subnet.cidr
            ))
        }
    }
}
