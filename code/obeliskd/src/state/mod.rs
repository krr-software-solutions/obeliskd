//! The daemon's persisted state, in the state dir (see [`files`] for the layout).
//!
//! Only the daemon reads or writes it (dirs `0700`, files `0600`); everyone else goes through the
//! control API.

pub mod files;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::ObeliskdError;
use crate::dns::{HostTable, SharedHosts};
use crate::model::subnet::{self, PlaceError};
use crate::model::{
    Actor, Grant, Host, Hostname, Ip, Name, Principal, Role, Scope, Subnet, SubnetKind, Supernet,
    ip, supernet,
};
use crate::os::OsEffects;

pub use files::CONFIG_FILE;

/// Everything obeliskd has allocated, as one tree (on disk it's split across files).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Grants on the whole tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub supernets: Vec<Supernet>,
}

impl Config {
    fn ip_mut(&mut self, supernet: &Name, subnet: &Name, ip: &Name) -> Option<&mut Ip> {
        self.supernets
            .iter_mut()
            .find(|s| &s.name == supernet)?
            .subnets
            .iter_mut()
            .find(|s| &s.name == subnet)?
            .ips
            .iter_mut()
            .find(|i| &i.name == ip)
    }

    /// Every host (they live on their ips), with the address it resolves to.
    pub(crate) fn hosts(&self) -> impl Iterator<Item = (Host, std::net::Ipv4Addr)> + '_ {
        self.supernets.iter().flat_map(|supernet| {
            supernet.subnets.iter().flat_map(move |subnet| {
                subnet.ips.iter().flat_map(move |ip| {
                    ip.hosts.iter().map(move |name| {
                        let host = Host {
                            name: name.clone(),
                            supernet: supernet.name.clone(),
                            subnet: subnet.name.clone(),
                            ip: ip.name.clone(),
                        };
                        (host, ip.address)
                    })
                })
            })
        })
    }

    /// The whole tree, or just one supernet (and within it, one subnet); see [`Store::tree`].
    fn tree(
        &self,
        supernet: Option<&Name>,
        subnet: Option<&Name>,
    ) -> Result<Vec<Supernet>, ObeliskdError> {
        let Some(supernet) = supernet else {
            if subnet.is_some() {
                return Err(ObeliskdError::Invalid(
                    "--subnet needs --supernet (subnet names are per supernet)".to_string(),
                ));
            }
            return Ok(self.supernets.clone());
        };
        let mut found = self
            .supernets
            .iter()
            .find(|s| &s.name == supernet)
            .cloned()
            .ok_or_else(|| ObeliskdError::NotFound(format!("no supernet {supernet}")))?;
        if let Some(subnet) = subnet {
            found.subnets.retain(|s| &s.name == subnet);
            if found.subnets.is_empty() {
                return Err(ObeliskdError::NotFound(format!(
                    "no subnet {supernet}/{subnet}"
                )));
            }
        }
        Ok(vec![found])
    }

    /// What DNS answers with.
    fn host_table(&self) -> HostTable {
        self.hosts()
            .map(|(host, address)| (host.name.to_string(), address))
            .collect()
    }

    /// Refuses removing ips that hosts still point at; `matches` picks the ips being removed.
    fn refuse_if_hosts(
        &self,
        what: &str,
        matches: impl Fn(&Host) -> bool,
    ) -> Result<(), ObeliskdError> {
        let using: Vec<String> = self
            .hosts()
            .filter(|(h, _)| matches(h))
            .map(|(h, _)| h.name.to_string())
            .collect();
        if using.is_empty() {
            Ok(())
        } else {
            Err(ObeliskdError::Conflict(format!(
                "{what} is still used by host{} {}; dealloc {} first, or use --recursive",
                if using.len() == 1 { "" } else { "s" },
                using.join(", "),
                if using.len() == 1 { "it" } else { "them" }
            )))
        }
    }

    /// `NotFound` unless `scope` exists.
    fn exists(&self, scope: &Scope) -> Result<(), ObeliskdError> {
        if self.grants_on(scope).is_some() {
            return Ok(());
        }
        let supernet = scope.supernet().expect("the whole tree always exists");
        Err(ObeliskdError::NotFound(
            match (
                self.supernets.iter().any(|s| &s.name == supernet),
                scope.subnet(),
            ) {
                (true, Some(subnet)) => format!("no subnet {supernet}/{subnet}"),
                _ => format!("no supernet {supernet}"),
            },
        ))
    }

    /// The grants set on `scope` itself, if it exists.
    fn grants_on(&self, scope: &Scope) -> Option<&Vec<Grant>> {
        let supernet = |name: &Name| self.supernets.iter().find(|s| &s.name == name);
        match scope {
            Scope::All => Some(&self.grants),
            Scope::Supernet(name) => supernet(name).map(|s| &s.grants),
            Scope::Subnet(name, subnet) => supernet(name)?
                .subnets
                .iter()
                .find(|s| &s.name == subnet)
                .map(|s| &s.grants),
        }
    }

    fn grants_on_mut(&mut self, scope: &Scope) -> Option<&mut Vec<Grant>> {
        let supernets = self.supernets.iter_mut();
        match scope {
            Scope::All => Some(&mut self.grants),
            Scope::Supernet(name) => supernets
                .into_iter()
                .find(|s| &s.name == name)
                .map(|s| &mut s.grants),
            Scope::Subnet(name, subnet) => supernets
                .into_iter()
                .find(|s| &s.name == name)?
                .subnets
                .iter_mut()
                .find(|s| &s.name == subnet)
                .map(|s| &mut s.grants),
        }
    }

    /// Every grant with its scope, in tree order (a scope's own grants before its children's).
    fn all_grants(&self) -> Vec<(Scope, Grant)> {
        let mut out: Vec<(Scope, Grant)> = self.grants.iter().map(|g| (Scope::All, *g)).collect();
        for supernet in &self.supernets {
            let here = Scope::Supernet(supernet.name.clone());
            out.extend(supernet.grants.iter().map(|g| (here.clone(), *g)));
            for subnet in &supernet.subnets {
                let here = Scope::Subnet(supernet.name.clone(), subnet.name.clone());
                out.extend(subnet.grants.iter().map(|g| (here.clone(), *g)));
            }
        }
        out
    }

    /// The grants that give `actor` something on `scope`: on it, or on one of its parents.
    fn grants_for(&self, actor: &Actor, scope: &Scope) -> Vec<(Scope, Grant)> {
        self.all_grants()
            .into_iter()
            .filter(|(at, grant)| at.covers(scope) && actor.is(grant.who))
            .collect()
    }

    /// `actor`'s role on `scope`: root has every role everywhere.
    fn role(&self, actor: &Actor, scope: &Scope) -> Option<Role> {
        if actor.is_root() {
            return Some(Role::Admin);
        }
        self.grants_for(actor, scope)
            .iter()
            .map(|(_, g)| g.role)
            .max()
    }

    /// `NotFound` unless `scope` exists; then `Forbidden` unless `actor` has at least `role` on it.
    fn require(&self, actor: &Actor, scope: &Scope, role: Role) -> Result<(), ObeliskdError> {
        self.exists(scope)?;
        let has = self.role(actor, scope);
        if has >= Some(role) {
            Ok(())
        } else {
            Err(ObeliskdError::Forbidden(format!(
                "uid {} needs {role} on {} (has {}); ask an admin of it, or use sudo",
                actor.uid,
                scope.describe(),
                has.map_or_else(|| "nothing".to_string(), |r| r.to_string())
            )))
        }
    }
}

/// `Forbidden` unless `actor` is root: some changes are never granted (a supernet can overlap the
/// LAN's or a VPN's routes).
fn require_root(actor: &Actor, what: &str) -> Result<(), ObeliskdError> {
    if actor.is_root() {
        Ok(())
    } else {
        Err(ObeliskdError::Forbidden(format!(
            "uid {} isn't root, and {what} needs root; try again with sudo",
            actor.uid
        )))
    }
}

/// The state dir, the config loaded from it, and the OS it's realized on.
///
/// Every change is applied to the OS first, then saved (undoing the OS change if saving fails),
/// and only then visible. Removals go the other way: off the OS, then saved.
pub struct Store {
    dir: PathBuf,
    config: Mutex<Config>,
    os: Arc<dyn OsEffects>,
    hosts: SharedHosts,
}

impl Store {
    /// Loads the state from `dir` (see [`files`]); nothing there yet is an empty config. State in
    /// the older all-in-`config.toml` layout is moved into the new one.
    ///
    /// # Errors
    ///
    /// If the state can't be read, parsed, or doesn't add up (see [`files::load`]). The daemon
    /// refuses to start rather than risk overwriting state it couldn't understand.
    pub fn load(dir: &Path, os: Arc<dyn OsEffects>) -> Result<Self, ObeliskdError> {
        let (config, migrated) = files::load(dir)?;
        if migrated {
            tracing::info!(
                "moved allocations out of {CONFIG_FILE} into {}/",
                files::ALLOC_DIR
            );
        }
        let hosts = Arc::new(std::sync::RwLock::new(Arc::new(config.host_table())));
        Ok(Store {
            dir: dir.to_path_buf(),
            config: Mutex::new(config),
            os,
            hosts,
        })
    }

    /// The host table DNS reads; the store swaps in a new one after every change.
    #[must_use]
    pub fn host_table(&self) -> SharedHosts {
        Arc::clone(&self.hosts)
    }

    /// Makes `next` the state (already saved), and refreshes what DNS answers with.
    fn commit(&self, config: &mut Config, next: Config) {
        let table = Arc::new(next.host_table());
        *config = next;
        *self
            .hosts
            .write()
            .expect("the host table lock is never poisoned") = table;
    }

    /// Makes the OS match the config again (routes don't survive a reboot; docker networks are
    /// checked). Keeps going past failures and returns them, so one broken subnet doesn't take
    /// the daemon down.
    pub async fn reconcile(&self) -> Vec<ObeliskdError> {
        let config = self.config.lock().await;
        let mut errors = Vec::new();
        for supernet in &config.supernets {
            errors.extend(self.os.ensure_supernet(supernet).err());
            for subnet in &supernet.subnets {
                errors.extend(self.os.ensure_subnet(&supernet.name, subnet).err());
                for ip in &subnet.ips {
                    errors.extend(self.os.ensure_ip(subnet, ip).err());
                }
            }
        }
        errors
    }

    /// How much is allocated, for the startup log.
    pub async fn counts(&self) -> Counts {
        let config = self.config.lock().await;
        let subnets = || config.supernets.iter().flat_map(|s| &s.subnets);
        Counts {
            supernets: config.supernets.len(),
            subnets: subnets().count(),
            ips: subnets().map(|s| s.ips.len()).sum(),
            hosts: config.hosts().count(),
        }
    }

    /// The whole tree, or just one supernet (and within it, one subnet).
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if a named supernet or subnet doesn't exist;
    /// [`ObeliskdError::Invalid`] for a subnet without its supernet.
    pub async fn tree(
        &self,
        supernet: Option<&Name>,
        subnet: Option<&Name>,
    ) -> Result<Vec<Supernet>, ObeliskdError> {
        self.config.lock().await.tree(supernet, subnet)
    }

    /// One supernet, as it is now.
    pub async fn supernet(&self, name: &Name) -> Option<Supernet> {
        self.config
            .lock()
            .await
            .supernets
            .iter()
            .find(|s| &s.name == name)
            .cloned()
    }

    pub async fn supernets(&self) -> Vec<Supernet> {
        self.config.lock().await.supernets.clone()
    }

    /// Adds a supernet, and saves.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::Invalid`] for a CIDR that isn't the start of its range,
    /// [`ObeliskdError::Conflict`] if the name is taken or the range overlaps another supernet,
    /// or a failure to save (then nothing changes).
    pub async fn alloc_supernet(
        &self,
        actor: &Actor,
        name: Name,
        cidr: Ipv4Net,
    ) -> Result<Supernet, ObeliskdError> {
        require_root(actor, "creating a supernet")?;
        let cidr = supernet::require_network(cidr).map_err(ObeliskdError::Invalid)?;
        let mut config = self.config.lock().await;
        if config.supernets.iter().any(|s| s.name == name) {
            return Err(ObeliskdError::Conflict(format!(
                "supernet {name} already exists"
            )));
        }
        if let Some(other) = config.supernets.iter().find(|s| s.overlaps(&cidr)) {
            return Err(ObeliskdError::Conflict(format!(
                "{cidr} overlaps supernet {} ({})",
                other.name, other.cidr
            )));
        }

        let supernet = Supernet {
            name,
            cidr,
            grants: Vec::new(),
            subnets: Vec::new(),
        };
        let mut next = config.clone();
        next.supernets.push(supernet.clone());
        self.os.ensure_supernet(&supernet)?;
        if let Err(error) = files::persist(&self.dir, &config, &next) {
            let _ = self.os.remove_supernet(&supernet);
            return Err(error);
        }
        self.commit(&mut config, next);
        tracing::info!("allocated supernet {} ({})", supernet.name, supernet.cidr);
        Ok(supernet)
    }
}

impl Store {
    /// Every subnet, with the name of the supernet it's in; optionally only one supernet's.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if `supernet` names one that doesn't exist.
    pub async fn subnets(
        &self,
        supernet: Option<&Name>,
    ) -> Result<Vec<(Name, Subnet)>, ObeliskdError> {
        let config = self.config.lock().await;
        if let Some(name) = supernet {
            if !config.supernets.iter().any(|s| &s.name == name) {
                return Err(ObeliskdError::NotFound(format!("no supernet {name}")));
            }
        }
        Ok(config
            .supernets
            .iter()
            .filter(|s| supernet.is_none_or(|name| &s.name == name))
            .flat_map(|s| s.subnets.iter().map(|sub| (s.name.clone(), sub.clone())))
            .collect())
    }

    /// Carves the first free `/size` out of `supernet`, and saves.
    ///
    /// `reserve` is the prefix length of Docker's slice: required for docker subnets, refused for
    /// host ones.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] for an unknown supernet; [`ObeliskdError::Invalid`] for a size
    /// or reserve that can't work; [`ObeliskdError::Conflict`] if the name is taken in that
    /// supernet, the range overlaps a sibling, or no range of that size is free; or a failure to
    /// save (then nothing changes).
    pub async fn alloc_subnet(
        &self,
        actor: &Actor,
        supernet: &Name,
        name: Name,
        kind: SubnetKind,
        size: u8,
        reserve: Option<u8>,
    ) -> Result<Subnet, ObeliskdError> {
        let mut config = self.config.lock().await;
        config.require(actor, &Scope::Supernet(supernet.clone()), Role::Manage)?;
        let mut next = config.clone();
        let parent = next
            .supernets
            .iter_mut()
            .find(|s| &s.name == supernet)
            .ok_or_else(|| ObeliskdError::NotFound(format!("no supernet {supernet}")))?;
        if parent.subnets.iter().any(|s| s.name == name) {
            return Err(ObeliskdError::Conflict(format!(
                "subnet {supernet}/{name} already exists"
            )));
        }
        match (kind, reserve) {
            (SubnetKind::Host, Some(_)) => {
                return Err(ObeliskdError::Invalid(
                    "--reserve-size is only for docker subnets".to_string(),
                ));
            }
            (SubnetKind::Docker, None) => {
                return Err(ObeliskdError::Invalid(
                    "docker subnets need --reserve-size (the size of Docker's own slice)"
                        .to_string(),
                ));
            }
            _ => {}
        }

        let taken: Vec<Ipv4Net> = parent.subnets.iter().map(|s| s.cidr).collect();
        let cidr = subnet::place(parent.cidr, &taken, size).map_err(|e| match e {
            PlaceError::Invalid(m) => ObeliskdError::Invalid(m),
            PlaceError::Conflict(m) => ObeliskdError::Conflict(m),
        })?;
        let docker_range = match reserve {
            None => None,
            Some(reserve) => {
                Some(subnet::docker_range(cidr, reserve).map_err(ObeliskdError::Invalid)?)
            }
        };

        let subnet = Subnet {
            name,
            kind,
            cidr,
            docker_range,
            docker_gateway: docker_range.map(|_| subnet::docker_gateway(cidr)),
            grants: Vec::new(),
            ips: Vec::new(),
        };
        parent.subnets.push(subnet.clone());
        self.os.ensure_subnet(supernet, &subnet)?;
        if let Err(error) = files::persist(&self.dir, &config, &next) {
            let _ = self.os.remove_subnet(supernet, &subnet);
            return Err(error);
        }
        self.commit(&mut config, next);
        tracing::info!(
            "allocated {} subnet {supernet}/{} ({})",
            subnet.kind,
            subnet.name,
            subnet.cidr
        );
        Ok(subnet)
    }
}

impl Store {
    /// Removes a supernet, and saves. With `recursive`, everything in it goes too (subnets, their
    /// ips and those ips' hosts); without, it must have no subnets. The removed supernet, with
    /// everything it took along, is returned.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if it doesn't exist; [`ObeliskdError::Conflict`] if it still has
    /// subnets and `recursive` is off; or a failure to save (then nothing changes).
    pub async fn dealloc_supernet(
        &self,
        actor: &Actor,
        name: &Name,
        recursive: bool,
    ) -> Result<Supernet, ObeliskdError> {
        require_root(actor, "removing a supernet")?;
        let mut config = self.config.lock().await;
        let mut next = config.clone();
        let index = next
            .supernets
            .iter()
            .position(|s| &s.name == name)
            .ok_or_else(|| ObeliskdError::NotFound(format!("no supernet {name}")))?;
        let count = next.supernets[index].subnets.len();
        if count > 0 && !recursive {
            return Err(ObeliskdError::Conflict(format!(
                "supernet {name} still has {count} subnet{}; dealloc them first, or use --recursive",
                if count == 1 { "" } else { "s" }
            )));
        }
        let removed = next.supernets.remove(index);
        for subnet in &removed.subnets {
            for ip in &subnet.ips {
                self.os.remove_ip(subnet, ip)?;
            }
            self.os.remove_subnet(&removed.name, subnet)?;
        }
        self.os.remove_supernet(&removed)?;
        files::persist(&self.dir, &config, &next)?;
        self.commit(&mut config, next);
        tracing::info!(
            subnets = removed.subnets.len(),
            hosts = removed
                .subnets
                .iter()
                .flat_map(|s| &s.ips)
                .map(|i| i.hosts.len())
                .sum::<usize>(),
            "deallocated supernet {} ({})",
            removed.name,
            removed.cidr
        );
        Ok(removed)
    }

    /// Removes a subnet from its supernet, and saves.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if the supernet or subnet doesn't exist; or a failure to save
    /// (then nothing changes).
    pub async fn dealloc_subnet(
        &self,
        actor: &Actor,
        supernet: &Name,
        name: &Name,
        recursive: bool,
    ) -> Result<Subnet, ObeliskdError> {
        let mut config = self.config.lock().await;
        config.require(actor, &Scope::Supernet(supernet.clone()), Role::Manage)?;
        let mut next = config.clone();
        let parent = next
            .supernets
            .iter_mut()
            .find(|s| &s.name == supernet)
            .ok_or_else(|| ObeliskdError::NotFound(format!("no supernet {supernet}")))?;
        let index = parent
            .subnets
            .iter()
            .position(|s| &s.name == name)
            .ok_or_else(|| ObeliskdError::NotFound(format!("no subnet {supernet}/{name}")))?;
        let count = parent.subnets[index].ips.len();
        if count > 0 && !recursive {
            return Err(ObeliskdError::Conflict(format!(
                "subnet {supernet}/{name} still has {count} ip{}; dealloc them first, or use --recursive",
                if count == 1 { "" } else { "s" }
            )));
        }
        let removed = parent.subnets.remove(index);
        for ip in &removed.ips {
            self.os.remove_ip(&removed, ip)?;
        }
        self.os.remove_subnet(supernet, &removed)?;
        files::persist(&self.dir, &config, &next)?;
        self.commit(&mut config, next);
        tracing::info!(
            ips = removed.ips.len(),
            hosts = removed.ips.iter().map(|i| i.hosts.len()).sum::<usize>(),
            "deallocated subnet {supernet}/{} ({})",
            removed.name,
            removed.cidr
        );
        Ok(removed)
    }
}

/// How much is allocated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub supernets: usize,
    pub subnets: usize,
    pub ips: usize,
    pub hosts: usize,
}

/// An ip with where it lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpIn {
    pub supernet: Name,
    pub subnet: Name,
    pub kind: SubnetKind,
    pub ip: Ip,
}

impl Store {
    /// Every ip, optionally only one supernet's (and within it, one subnet's).
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if a named supernet or subnet doesn't exist;
    /// [`ObeliskdError::Invalid`] for a subnet without its supernet.
    pub async fn ips(
        &self,
        supernet: Option<&Name>,
        subnet: Option<&Name>,
    ) -> Result<Vec<IpIn>, ObeliskdError> {
        let config = self.config.lock().await;
        if subnet.is_some() && supernet.is_none() {
            return Err(ObeliskdError::Invalid(
                "--subnet needs --supernet (subnet names are per supernet)".to_string(),
            ));
        }
        if let Some(supernet) = supernet {
            let parent = config
                .supernets
                .iter()
                .find(|s| &s.name == supernet)
                .ok_or_else(|| ObeliskdError::NotFound(format!("no supernet {supernet}")))?;
            if let Some(subnet) = subnet {
                if !parent.subnets.iter().any(|s| &s.name == subnet) {
                    return Err(ObeliskdError::NotFound(format!(
                        "no subnet {supernet}/{subnet}"
                    )));
                }
            }
        }
        Ok(config
            .supernets
            .iter()
            .filter(|s| supernet.is_none_or(|n| &s.name == n))
            .flat_map(|s| {
                s.subnets
                    .iter()
                    .filter(move |sub| subnet.is_none_or(|n| &sub.name == n))
                    .flat_map(move |sub| {
                        sub.ips.iter().map(move |ip| IpIn {
                            supernet: s.name.clone(),
                            subnet: sub.name.clone(),
                            kind: sub.kind,
                            ip: ip.clone(),
                        })
                    })
            })
            .collect())
    }

    /// Gives `name` the lowest free address in the subnet, makes it usable on the OS, and saves.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] for an unknown supernet or subnet; [`ObeliskdError::Conflict`]
    /// if the name is taken in that subnet or it's full; or an OS/save failure (then nothing
    /// changes).
    pub async fn alloc_ip(
        &self,
        actor: &Actor,
        supernet: &Name,
        subnet: &Name,
        name: Name,
        tag: Option<Name>,
    ) -> Result<IpIn, ObeliskdError> {
        let mut config = self.config.lock().await;
        config.require(actor, &subnet_scope(supernet, subnet), Role::Use)?;
        let mut next = config.clone();
        let parent = find_subnet(&mut next, supernet, subnet)?;
        if parent.ips.iter().any(|ip| ip.name == name) {
            return Err(ObeliskdError::Conflict(format!(
                "ip {supernet}/{subnet}/{name} already exists"
            )));
        }
        let address = ip::pick(parent).ok_or_else(|| {
            ObeliskdError::Conflict(format!("subnet {supernet}/{subnet} is full"))
        })?;
        let new = Ip {
            name,
            address,
            tag,
            hosts: Vec::new(),
        };
        parent.ips.push(new.clone());
        let parent = parent.clone();

        self.os.ensure_ip(&parent, &new)?;
        if let Err(error) = files::persist(&self.dir, &config, &next) {
            let _ = self.os.remove_ip(&parent, &new);
            return Err(error);
        }
        self.commit(&mut config, next);
        tracing::info!(
            tag = ?new.tag,
            "allocated ip {supernet}/{subnet}/{} = {}",
            new.name,
            new.address
        );
        Ok(IpIn {
            supernet: supernet.clone(),
            subnet: subnet.clone(),
            kind: parent.kind,
            ip: new,
        })
    }

    /// Frees an ip (its address can be handed out again), and saves. With `recursive`, the hosts
    /// pointing at it go too; without, it must have none.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if it (or its supernet/subnet) doesn't exist;
    /// [`ObeliskdError::Conflict`] if it has hosts and `recursive` is off; or an OS/save failure
    /// (then nothing changes).
    pub async fn dealloc_ip(
        &self,
        actor: &Actor,
        supernet: &Name,
        subnet: &Name,
        name: &Name,
        recursive: bool,
    ) -> Result<IpIn, ObeliskdError> {
        let mut config = self.config.lock().await;
        config.require(actor, &subnet_scope(supernet, subnet), Role::Use)?;
        let mut next = config.clone();
        if !recursive {
            config.refuse_if_hosts(&format!("ip {supernet}/{subnet}/{name}"), |h| {
                h.targets(supernet, subnet, name)
            })?;
        }
        let parent = find_subnet(&mut next, supernet, subnet)?;
        let index = parent
            .ips
            .iter()
            .position(|ip| &ip.name == name)
            .ok_or_else(|| ObeliskdError::NotFound(format!("no ip {supernet}/{subnet}/{name}")))?;
        let removed = parent.ips.remove(index);
        self.os.remove_ip(parent, &removed)?;
        let kind = parent.kind;
        files::persist(&self.dir, &config, &next)?;
        self.commit(&mut config, next);
        tracing::info!(
            hosts = removed.hosts.len(),
            "deallocated ip {supernet}/{subnet}/{} ({})",
            removed.name,
            removed.address
        );
        Ok(IpIn {
            supernet: supernet.clone(),
            subnet: subnet.clone(),
            kind,
            ip: removed,
        })
    }

    /// Frees every ip tagged `tag`, optionally only in one supernet (and within it, one subnet),
    /// and saves, all at once. With `recursive`, the hosts pointing at them go too; without, none
    /// may have any (then nothing is removed). What was removed is returned as a tree, holding
    /// only the branches that had any.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if a named supernet or subnet doesn't exist, or no ip there has
    /// the tag; [`ObeliskdError::Invalid`] for a subnet without its supernet;
    /// [`ObeliskdError::Conflict`] if any of them has hosts and `recursive` is off; or an OS/save
    /// failure.
    pub async fn dealloc_ips_tagged(
        &self,
        actor: &Actor,
        supernet: Option<&Name>,
        subnet: Option<&Name>,
        tag: &Name,
        recursive: bool,
    ) -> Result<Vec<Supernet>, ObeliskdError> {
        let mut config = self.config.lock().await;
        // Checks the names, and gives the tree to pick from.
        let mut removed = config.tree(supernet, subnet)?;
        let tagged = |ip: &Ip| ip.tag.as_ref() == Some(tag);
        for parent in &mut removed {
            for sub in &mut parent.subnets {
                sub.ips.retain(|ip| tagged(ip));
            }
            parent.subnets.retain(|sub| !sub.ips.is_empty());
        }
        removed.retain(|parent| !parent.subnets.is_empty());
        if removed.is_empty() {
            let place = match (supernet, subnet) {
                (Some(supernet), Some(subnet)) => format!(" in subnet {supernet}/{subnet}"),
                (Some(supernet), None) => format!(" in supernet {supernet}"),
                _ => String::new(),
            };
            return Err(ObeliskdError::NotFound(format!(
                "no ips tagged {tag}{place}"
            )));
        }
        // Needs use on every subnet it would touch.
        let denied: Vec<String> = removed
            .iter()
            .flat_map(|s| {
                s.subnets
                    .iter()
                    .map(move |sub| subnet_scope(&s.name, &sub.name))
            })
            .filter(|scope| config.role(actor, scope) < Some(Role::Use))
            .map(|scope| scope.to_string())
            .collect();
        if !denied.is_empty() {
            return Err(ObeliskdError::Forbidden(format!(
                "uid {} needs use on subnet{} {} to dealloc the ips tagged {tag} there; nothing was \
                 removed (narrow it with --supernet/--subnet, ask an admin, or use sudo)",
                actor.uid,
                if denied.len() == 1 { "" } else { "s" },
                denied.join(", ")
            )));
        }
        let goes = |s: &Name, n: &Name, i: &Name| {
            removed.iter().any(|parent| {
                &parent.name == s
                    && parent
                        .subnets
                        .iter()
                        .any(|sub| &sub.name == n && sub.ips.iter().any(|ip| &ip.name == i))
            })
        };
        if !recursive {
            config.refuse_if_hosts(&format!("an ip tagged {tag}"), |h| {
                goes(&h.supernet, &h.subnet, &h.ip)
            })?;
        }

        let mut next = config.clone();
        for parent in &mut next.supernets {
            for sub in &mut parent.subnets {
                let (gone, kept) = std::mem::take(&mut sub.ips)
                    .into_iter()
                    .partition(|ip| goes(&parent.name, &sub.name, &ip.name));
                sub.ips = kept;
                for ip in &gone {
                    self.os.remove_ip(sub, ip)?;
                }
            }
        }
        files::persist(&self.dir, &config, &next)?;
        self.commit(&mut config, next);
        let ips = removed.iter().flat_map(|s| &s.subnets).flat_map(|s| &s.ips);
        tracing::info!(
            ips = ips.clone().count(),
            hosts = ips.map(|i| i.hosts.len()).sum::<usize>(),
            "deallocated ips tagged {tag}"
        );
        Ok(removed)
    }
}

/// What a name resolves to (see [`Store::resolve`]): the supernet and subnet without their
/// children, the ip, the host that matched, and the hosts it shadows (best first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub supernet: Supernet,
    pub subnet: Subnet,
    pub ip: Ip,
    pub host: Hostname,
    pub shadowed: Vec<Hostname>,
}

/// A host, with the address it currently resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostIn {
    pub host: Host,
    pub address: std::net::Ipv4Addr,
}

impl Store {
    /// Every host, sorted by name.
    pub async fn hosts(&self) -> Vec<HostIn> {
        let config = self.config.lock().await;
        let mut hosts: Vec<HostIn> = config
            .hosts()
            .map(|(host, address)| HostIn { host, address })
            .collect();
        hosts.sort_by(|a, b| a.host.name.cmp(&b.host.name));
        hosts
    }

    /// What DNS answers `name` with: the host that wins (exact, else the most specific pattern),
    /// where its ip lives, and the other hosts that match but lose to it. `None` if nothing
    /// matches.
    ///
    /// # Panics
    ///
    /// Never: the host table is rebuilt from the config on every change, under the same lock.
    pub async fn resolve(&self, name: &Hostname) -> Option<Resolved> {
        let config = self.config.lock().await;
        let table = Arc::clone(
            &self
                .hosts
                .read()
                .expect("the host table lock is never poisoned"),
        );
        let mut matching = table.matching(name.as_str()).into_iter();
        let winner = matching.next()?;
        let (host, _) = config
            .hosts()
            .find(|(h, _)| h.name.as_str() == winner)
            .expect("every host in the table is in the config");
        let supernet = config
            .supernets
            .iter()
            .find(|s| s.name == host.supernet)
            .expect("a host's supernet exists");
        let subnet = supernet
            .subnets
            .iter()
            .find(|s| s.name == host.subnet)
            .expect("a host's subnet exists");
        let ip = subnet
            .ips
            .iter()
            .find(|i| i.name == host.ip)
            .expect("a host's ip exists");
        Some(Resolved {
            supernet: Supernet {
                subnets: Vec::new(),
                ..supernet.clone()
            },
            subnet: Subnet {
                ips: Vec::new(),
                ..subnet.clone()
            },
            ip: ip.clone(),
            host: host.name,
            shadowed: matching
                .map(|h| Hostname::parse(&h).expect("hosts in the table are valid"))
                .collect(),
        })
    }

    /// Points a hostname at an ip (it's saved on the ip, in its subnet's file), and saves. DNS
    /// answers with it right away.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::Conflict`] if the hostname is taken (by any ip); [`ObeliskdError::NotFound`]
    /// if the ip doesn't exist; or a failure to save (then nothing changes).
    pub async fn alloc_host(&self, actor: &Actor, host: Host) -> Result<HostIn, ObeliskdError> {
        let mut config = self.config.lock().await;
        config.require(
            actor,
            &subnet_scope(&host.supernet, &host.subnet),
            Role::Use,
        )?;
        if let Some((taken, _)) = config.hosts().find(|(h, _)| h.name == host.name) {
            return Err(ObeliskdError::Conflict(format!(
                "host {} already points at ip {}/{}/{}",
                host.name, taken.supernet, taken.subnet, taken.ip
            )));
        }
        let mut next = config.clone();
        let ip = next
            .ip_mut(&host.supernet, &host.subnet, &host.ip)
            .ok_or_else(|| {
                ObeliskdError::NotFound(format!(
                    "no ip {}/{}/{}",
                    host.supernet, host.subnet, host.ip
                ))
            })?;
        ip.hosts.push(host.name.clone());
        ip.hosts.sort();
        let address = ip.address;
        files::persist(&self.dir, &config, &next)?;
        self.commit(&mut config, next);
        tracing::info!(
            "allocated host {} → ip {}/{}/{} ({address})",
            host.name,
            host.supernet,
            host.subnet,
            host.ip
        );
        Ok(HostIn { host, address })
    }

    /// Removes a hostname, and saves. DNS answers NXDOMAIN for it right away.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if it doesn't exist; or a failure to save.
    ///
    /// # Panics
    ///
    /// Never: the host was just found on that ip.
    pub async fn dealloc_host(
        &self,
        actor: &Actor,
        name: &Hostname,
    ) -> Result<HostIn, ObeliskdError> {
        let mut config = self.config.lock().await;
        let (host, address) = config
            .hosts()
            .find(|(h, _)| &h.name == name)
            .ok_or_else(|| ObeliskdError::NotFound(format!("no host {name}")))?;
        config.require(
            actor,
            &subnet_scope(&host.supernet, &host.subnet),
            Role::Use,
        )?;
        let mut next = config.clone();
        let ip = next
            .ip_mut(&host.supernet, &host.subnet, &host.ip)
            .expect("the host was found on this ip");
        ip.hosts.retain(|h| h != name);
        files::persist(&self.dir, &config, &next)?;
        self.commit(&mut config, next);
        tracing::info!("deallocated host {}", host.name);
        Ok(HostIn { host, address })
    }
}

/// What someone may do on a scope, and the grants that say so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effective {
    pub scope: Scope,
    pub role: Role,
    /// The grants that apply, on the scope or a parent (none for root).
    pub from: Vec<(Scope, Grant)>,
}

impl Store {
    /// The grants set on `scope` and everything inside it, optionally only those for `who`.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if `scope` doesn't exist.
    pub async fn grants(
        &self,
        scope: &Scope,
        who: Option<Principal>,
    ) -> Result<Vec<(Scope, Grant)>, ObeliskdError> {
        let config = self.config.lock().await;
        config.exists(scope)?;
        Ok(config
            .all_grants()
            .into_iter()
            .filter(|(at, grant)| scope.covers(at) && who.is_none_or(|w| grant.who == w))
            .collect())
    }

    /// Gives `grant.who` `grant.role` on `scope` (replacing the role they had there), and saves.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if `scope` doesn't exist; [`ObeliskdError::Forbidden`] unless
    /// `actor` is an admin of it; or a failure to save (then nothing changes).
    ///
    /// # Panics
    ///
    /// Never: the scope was just checked.
    pub async fn grant(
        &self,
        actor: &Actor,
        scope: Scope,
        grant: Grant,
    ) -> Result<Grant, ObeliskdError> {
        let mut config = self.config.lock().await;
        config.require(actor, &scope, Role::Admin)?;
        let mut next = config.clone();
        let grants = next.grants_on_mut(&scope).expect("the scope exists");
        grants.retain(|g| g.who != grant.who);
        grants.push(grant);
        grants.sort_by_key(|g| g.who);
        files::persist(&self.dir, &config, &next)?;
        self.commit(&mut config, next);
        tracing::info!("granted {} on {} to {}", grant.role, scope, grant.who);
        Ok(grant)
    }

    /// Takes away whatever role `who` has on `scope` itself (not what they get from a parent), and
    /// saves.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if `scope` doesn't exist or `who` has no grant on it;
    /// [`ObeliskdError::Forbidden`] unless `actor` is an admin of it; or a failure to save.
    ///
    /// # Panics
    ///
    /// Never: the scope was just checked.
    pub async fn revoke(
        &self,
        actor: &Actor,
        scope: Scope,
        who: Principal,
    ) -> Result<Grant, ObeliskdError> {
        let mut config = self.config.lock().await;
        config.require(actor, &scope, Role::Admin)?;
        let mut next = config.clone();
        let grants = next.grants_on_mut(&scope).expect("the scope exists");
        let index = grants.iter().position(|g| g.who == who).ok_or_else(|| {
            ObeliskdError::NotFound(format!("{who} has no grant on {}", scope.describe()))
        })?;
        let removed = grants.remove(index);
        files::persist(&self.dir, &config, &next)?;
        self.commit(&mut config, next);
        tracing::info!("revoked {} on {} from {}", removed.role, scope, removed.who);
        Ok(removed)
    }

    /// What `actor` may do: on `scope` (counting its parents), or else at every scope where a grant
    /// applies to them. Root may do everything everywhere.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if `scope` doesn't exist.
    pub async fn effective(
        &self,
        actor: &Actor,
        scope: Option<&Scope>,
    ) -> Result<Vec<Effective>, ObeliskdError> {
        let config = self.config.lock().await;
        if let Some(scope) = scope {
            config.exists(scope)?;
        }
        let at = |scope: &Scope| {
            let from = if actor.is_root() {
                Vec::new()
            } else {
                config.grants_for(actor, scope)
            };
            config.role(actor, scope).map(|role| Effective {
                scope: scope.clone(),
                role,
                from,
            })
        };
        Ok(match scope {
            Some(scope) => at(scope).into_iter().collect(),
            None if actor.is_root() => at(&Scope::All).into_iter().collect(),
            None => {
                let mut scopes: Vec<Scope> = Vec::new();
                for (scope, grant) in config.all_grants() {
                    if actor.is(grant.who) && !scopes.contains(&scope) {
                        scopes.push(scope);
                    }
                }
                scopes.iter().filter_map(at).collect()
            }
        })
    }
}

fn subnet_scope(supernet: &Name, subnet: &Name) -> Scope {
    Scope::Subnet(supernet.clone(), subnet.clone())
}

fn find_subnet<'a>(
    config: &'a mut Config,
    supernet: &Name,
    subnet: &Name,
) -> Result<&'a mut Subnet, ObeliskdError> {
    config
        .supernets
        .iter_mut()
        .find(|s| &s.name == supernet)
        .ok_or_else(|| ObeliskdError::NotFound(format!("no supernet {supernet}")))?
        .subnets
        .iter_mut()
        .find(|s| &s.name == subnet)
        .ok_or_else(|| ObeliskdError::NotFound(format!("no subnet {supernet}/{subnet}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os::fake::RecordingOs;

    fn load(dir: &Path) -> Store {
        Store::load(dir, Arc::new(RecordingOs::default())).unwrap()
    }

    fn load_with(dir: &Path) -> (Store, Arc<RecordingOs>) {
        let os = Arc::new(RecordingOs::default());
        (Store::load(dir, os.clone()).unwrap(), os)
    }

    fn name(s: &str) -> Name {
        Name::parse(s).unwrap()
    }

    #[tokio::test]
    async fn supernets_persist_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        assert_eq!(store.supernets().await, Vec::new());

        store
            .alloc_supernet(&Actor::ROOT, name("obd"), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        let reloaded = load(dir.path());
        assert_eq!(
            reloaded.supernets().await,
            vec![Supernet {
                name: name("obd"),
                cidr: "10.57.0.0/16".parse().unwrap(),
                grants: Vec::new(),
                subnets: Vec::new(),
            }]
        );

        let text = std::fs::read_to_string(dir.path().join("alloc/obd.toml")).unwrap();
        assert!(text.starts_with("# Managed by obeliskd"));
        assert!(text.contains("name = \"obd\"") && text.contains("cidr = \"10.57.0.0/16\""));
        assert!(
            !dir.path().join(CONFIG_FILE).exists(),
            "allocations never go in config.toml"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("alloc/obd.toml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn rejects_duplicates_overlaps_and_unaligned_cidrs() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        store
            .alloc_supernet(&Actor::ROOT, name("obd"), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();

        let taken = store
            .alloc_supernet(&Actor::ROOT, name("obd"), "10.58.0.0/16".parse().unwrap())
            .await;
        assert!(matches!(taken, Err(ObeliskdError::Conflict(_))));
        let overlap = store
            .alloc_supernet(
                &Actor::ROOT,
                name("other"),
                "10.57.22.0/24".parse().unwrap(),
            )
            .await;
        assert!(
            matches!(overlap, Err(ObeliskdError::Conflict(m)) if m.contains("overlaps supernet obd"))
        );
        let unaligned = store
            .alloc_supernet(&Actor::ROOT, name("other"), "10.58.1.2/16".parse().unwrap())
            .await;
        assert!(matches!(unaligned, Err(ObeliskdError::Invalid(_))));
        assert_eq!(store.supernets().await.len(), 1);
    }

    #[tokio::test]
    async fn subnets_nest_in_their_supernet_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();

        let host = store
            .alloc_subnet(&Actor::ROOT, &obd, name("host"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        assert_eq!(host.cidr, "10.57.0.0/24".parse().unwrap());
        assert_eq!(host.docker_range, None);

        let dev = store
            .alloc_subnet(
                &Actor::ROOT,
                &obd,
                name("k3d-dev"),
                SubnetKind::Docker,
                24,
                Some(26),
            )
            .await
            .unwrap();
        assert_eq!(
            dev.cidr,
            "10.57.1.0/24".parse().unwrap(),
            "the next free /24"
        );
        assert_eq!(dev.docker_range, Some("10.57.1.0/26".parse().unwrap()));
        assert_eq!(dev.docker_gateway, Some("10.57.1.1".parse().unwrap()));
        assert_eq!(host.docker_gateway, None);

        let reloaded = load(dir.path());
        let all = reloaded.subnets(None).await.unwrap();
        assert_eq!(all, vec![(obd.clone(), host), (obd.clone(), dev)]);
        let supernet = std::fs::read_to_string(dir.path().join("alloc/obd.toml")).unwrap();
        assert!(
            !supernet.contains("k3d-dev"),
            "the supernet file holds no subnets: {supernet}"
        );
        let dev_file = std::fs::read_to_string(dir.path().join("alloc/obd/k3d-dev.toml")).unwrap();
        assert!(dev_file.starts_with("# Managed by obeliskd"), "{dev_file}");
        assert!(
            dev_file.contains("dockerRange = \"10.57.1.0/26\""),
            "{dev_file}"
        );
        assert!(dir.path().join("alloc/obd/host.toml").exists());
    }

    #[tokio::test]
    async fn subnet_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(&Actor::ROOT, &obd, name("host"), SubnetKind::Host, 24, None)
            .await
            .unwrap();

        let unknown = store
            .alloc_subnet(
                &Actor::ROOT,
                &name("nope"),
                name("x"),
                SubnetKind::Host,
                24,
                None,
            )
            .await;
        assert!(matches!(unknown, Err(ObeliskdError::NotFound(_))));
        let taken = store
            .alloc_subnet(&Actor::ROOT, &obd, name("host"), SubnetKind::Host, 24, None)
            .await;
        assert!(matches!(taken, Err(ObeliskdError::Conflict(_))));
        let host_reserve = store
            .alloc_subnet(
                &Actor::ROOT,
                &obd,
                name("x"),
                SubnetKind::Host,
                24,
                Some(26),
            )
            .await;
        assert!(matches!(host_reserve, Err(ObeliskdError::Invalid(_))));
        let docker_no_reserve = store
            .alloc_subnet(&Actor::ROOT, &obd, name("x"), SubnetKind::Docker, 24, None)
            .await;
        assert!(matches!(docker_no_reserve, Err(ObeliskdError::Invalid(_))));
        let tiny_docker = store
            .alloc_subnet(
                &Actor::ROOT,
                &obd,
                name("x"),
                SubnetKind::Docker,
                30,
                Some(31),
            )
            .await;
        assert!(matches!(tiny_docker, Err(ObeliskdError::Invalid(_))));
        assert!(matches!(
            store.subnets(Some(&name("nope"))).await,
            Err(ObeliskdError::NotFound(_))
        ));
        assert_eq!(
            store.subnets(None).await.unwrap().len(),
            1,
            "failed allocs change nothing"
        );
    }

    #[tokio::test]
    async fn fills_in_gateways_missing_from_older_configs() {
        let dir = tempfile::tempdir().unwrap();
        let older = "[[supernets]]\nname = \"ex1\"\ncidr = \"10.57.0.0/16\"\n\n[[supernets.subnets]]\nname = \"ex1-6\"\nkind = \"docker\"\ncidr = \"10.57.16.0/22\"\ndockerRange = \"10.57.16.0/28\"\n";
        std::fs::write(dir.path().join(CONFIG_FILE), older).unwrap();
        let store = load(dir.path());
        let (_, subnet) = &store.subnets(None).await.unwrap()[0];
        assert_eq!(subnet.docker_gateway, Some("10.57.16.1".parse().unwrap()));

        // ...and moved out of config.toml into alloc/ (config.toml held nothing else, so it's
        // gone), which a fresh load reads back.
        assert!(!dir.path().join(CONFIG_FILE).exists());
        assert!(dir.path().join("alloc/ex1.toml").exists());
        let moved = std::fs::read_to_string(dir.path().join("alloc/ex1/ex1-6.toml")).unwrap();
        assert!(moved.contains("dockerGateway = \"10.57.16.1\""), "{moved}");
        assert_eq!(load(dir.path()).subnets(None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn migrating_keeps_other_settings_in_config_toml() {
        let dir = tempfile::tempdir().unwrap();
        let older =
            "future_setting = 1\n\n[[supernets]]\nname = \"ex1\"\ncidr = \"10.57.0.0/16\"\n";
        std::fs::write(dir.path().join(CONFIG_FILE), older).unwrap();
        let store = load(dir.path());
        assert_eq!(store.supernets().await.len(), 1);
        let config = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(config.contains("future_setting = 1"), "{config}");
        assert!(!config.contains("supernets"), "{config}");
        assert!(dir.path().join("alloc/ex1.toml").exists());
        // Loading again is a no-op: nothing left to migrate, nothing defined twice.
        assert_eq!(load(dir.path()).supernets().await.len(), 1);
    }

    #[tokio::test]
    async fn only_the_changed_subnet_file_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        for n in ["a", "b"] {
            store
                .alloc_subnet(&Actor::ROOT, &obd, name(n), SubnetKind::Host, 24, None)
                .await
                .unwrap();
        }
        let modified = |p: &str| {
            std::fs::metadata(dir.path().join(p))
                .unwrap()
                .modified()
                .unwrap()
        };
        let (supernet_before, b_before) =
            (modified("alloc/obd.toml"), modified("alloc/obd/b.toml"));
        std::thread::sleep(std::time::Duration::from_millis(20));

        store
            .alloc_ip(&Actor::ROOT, &obd, &name("a"), name("db"), None)
            .await
            .unwrap();
        assert_eq!(
            modified("alloc/obd.toml"),
            supernet_before,
            "the supernet file untouched"
        );
        assert_eq!(
            modified("alloc/obd/b.toml"),
            b_before,
            "the other subnet untouched"
        );
        let a = std::fs::read_to_string(dir.path().join("alloc/obd/a.toml")).unwrap();
        assert!(a.contains("[[ips]]") && a.contains("10.57.0.1"), "{a}");

        // Removing the supernet takes its files and dir with it.
        store
            .dealloc_supernet(&Actor::ROOT, &obd, true)
            .await
            .unwrap();
        assert!(!dir.path().join("alloc/obd").exists());
        assert!(!dir.path().join("alloc/obd.toml").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn state_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(&Actor::ROOT, &obd, name("a"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        let mode = |p: &str| {
            std::fs::metadata(dir.path().join(p))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("alloc/obd/a.toml"), 0o600);
        assert_eq!(mode("alloc/obd"), 0o700);
    }

    #[tokio::test]
    async fn refuses_state_that_doesnt_add_up() {
        let obd = "name = \"obd\"\ncidr = \"10.57.0.0/16\"\n";
        let subnet =
            |name: &str| format!("name = \"{name}\"\nkind = \"host\"\ncidr = \"10.57.0.0/24\"\n");
        let try_load = |files: &[(&str, String)]| {
            let dir = tempfile::tempdir().unwrap();
            for (path, body) in files {
                let path = dir.path().join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, body).unwrap();
            }
            Store::load(dir.path(), Arc::new(RecordingOs::default())).map(|_| ())
        };

        let mismatched = try_load(&[
            ("alloc/obd.toml", obd.into()),
            ("alloc/obd/a.toml", subnet("b")),
        ]);
        assert!(
            mismatched
                .unwrap_err()
                .to_string()
                .contains("filename says a")
        );
        let orphan = try_load(&[
            ("alloc/obd.toml", obd.into()),
            ("alloc/gone/a.toml", subnet("a")),
        ]);
        assert!(
            orphan
                .unwrap_err()
                .to_string()
                .contains("has no gone.toml next to it")
        );
        let renamed = try_load(&[("alloc/obd.toml", obd.replace("obd", "other"))]);
        assert!(
            renamed
                .unwrap_err()
                .to_string()
                .contains("filename says obd")
        );
        let stray = try_load(&[
            ("alloc/obd.toml", obd.into()),
            ("alloc/obd/notes.txt", String::new()),
        ]);
        assert!(stray.unwrap_err().to_string().contains("not a subnet file"));
        // An interrupted write leaves a .tmp next to the real file: ignored.
        let interrupted = try_load(&[
            ("alloc/obd.toml", obd.into()),
            ("alloc/obd/a.toml", subnet("a")),
            ("alloc/obd/a.toml.tmp", "garbage".into()),
        ]);
        assert!(interrupted.is_ok());
    }

    #[tokio::test]
    async fn dealloc_frees_names_and_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        let first = store
            .alloc_subnet(&Actor::ROOT, &obd, name("a"), SubnetKind::Host, 24, None)
            .await
            .unwrap();

        // Not empty: refused, and nothing changes.
        let busy = store.dealloc_supernet(&Actor::ROOT, &obd, false).await;
        assert!(
            matches!(busy, Err(ObeliskdError::Conflict(m)) if m.contains("still has 1 subnet;"))
        );

        let removed = store
            .dealloc_subnet(&Actor::ROOT, &obd, &name("a"), false)
            .await
            .unwrap();
        assert_eq!(removed, first);
        // The range is free again: the next /24 lands in the same place.
        let again = store
            .alloc_subnet(&Actor::ROOT, &obd, name("b"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        assert_eq!(again.cidr, first.cidr);
        store
            .dealloc_subnet(&Actor::ROOT, &obd, &name("b"), false)
            .await
            .unwrap();

        store
            .dealloc_supernet(&Actor::ROOT, &obd, false)
            .await
            .unwrap();
        let reloaded = load(dir.path());
        assert_eq!(reloaded.supernets().await, Vec::new(), "persisted");
    }

    #[tokio::test]
    async fn recursive_dealloc_takes_the_subnets_along() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        for n in ["a", "b"] {
            store
                .alloc_subnet(&Actor::ROOT, &obd, name(n), SubnetKind::Host, 24, None)
                .await
                .unwrap();
        }
        let removed = store
            .dealloc_supernet(&Actor::ROOT, &obd, true)
            .await
            .unwrap();
        assert_eq!(removed.subnets.len(), 2);
        assert_eq!(store.subnets(None).await.unwrap(), Vec::new());
        assert_eq!(load(dir.path()).supernets().await, Vec::new());
    }

    #[tokio::test]
    async fn dealloc_unknown_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        assert!(matches!(
            store
                .dealloc_supernet(&Actor::ROOT, &name("nope"), false)
                .await,
            Err(ObeliskdError::NotFound(_))
        ));
        store
            .alloc_supernet(&Actor::ROOT, name("obd"), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        assert!(matches!(
            store
                .dealloc_subnet(&Actor::ROOT, &name("nope"), &name("a"), false)
                .await,
            Err(ObeliskdError::NotFound(_))
        ));
        assert!(matches!(
            store
                .dealloc_subnet(&Actor::ROOT, &name("obd"), &name("a"), false)
                .await,
            Err(ObeliskdError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn changes_hit_the_os_in_order_and_failures_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (store, os) = load_with(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(&Actor::ROOT, &obd, name("a"), SubnetKind::Host, 24, None)
            .await
            .unwrap();

        // The OS refuses: nothing is saved.
        *os.fail_next.lock().unwrap() = Some(ObeliskdError::Os {
            command: "docker network create".to_string(),
            message: "boom".to_string(),
        });
        let refused = store
            .alloc_subnet(
                &Actor::ROOT,
                &obd,
                name("b"),
                SubnetKind::Docker,
                24,
                Some(26),
            )
            .await;
        assert!(matches!(refused, Err(ObeliskdError::Os { .. })));
        assert_eq!(load(dir.path()).subnets(None).await.unwrap().len(), 1);

        store
            .dealloc_supernet(&Actor::ROOT, &obd, true)
            .await
            .unwrap();
        assert_eq!(
            os.calls(),
            [
                "ensure supernet 10.57.0.0/16",
                "ensure subnet obd/a 10.57.0.0/24",
                "remove subnet obd/a 10.57.0.0/24",
                "remove supernet 10.57.0.0/16",
            ]
        );
    }

    #[tokio::test]
    async fn reconcile_reapplies_everything_and_collects_failures() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = load(dir.path());
            let obd = name("obd");
            store
                .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
                .await
                .unwrap();
            store
                .alloc_subnet(&Actor::ROOT, &obd, name("a"), SubnetKind::Host, 24, None)
                .await
                .unwrap();
        }
        let (store, os) = load_with(dir.path());
        assert!(store.reconcile().await.is_empty());
        assert_eq!(
            os.calls(),
            [
                "ensure supernet 10.57.0.0/16",
                "ensure subnet obd/a 10.57.0.0/24"
            ]
        );

        *os.fail_next.lock().unwrap() = Some(ObeliskdError::Os {
            command: "ip".to_string(),
            message: "nope".to_string(),
        });
        assert_eq!(
            store.reconcile().await.len(),
            1,
            "keeps going past a failure"
        );
    }

    #[tokio::test]
    async fn ips_take_the_lowest_free_address_and_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let (store, os) = load_with(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(&Actor::ROOT, &obd, name("host"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        store
            .alloc_subnet(
                &Actor::ROOT,
                &obd,
                name("dev"),
                SubnetKind::Docker,
                24,
                Some(26),
            )
            .await
            .unwrap();
        let (host, dev) = (name("host"), name("dev"));

        let db = store
            .alloc_ip(&Actor::ROOT, &obd, &host, name("db"), None)
            .await
            .unwrap();
        assert_eq!(
            db.ip.address,
            "10.57.0.1".parse::<std::net::Ipv4Addr>().unwrap()
        );
        let web = store
            .alloc_ip(&Actor::ROOT, &obd, &host, name("web"), None)
            .await
            .unwrap();
        assert_eq!(web.ip.address.to_string(), "10.57.0.2");
        let ingress = store
            .alloc_ip(&Actor::ROOT, &obd, &dev, name("ingress"), None)
            .await
            .unwrap();
        assert_eq!(
            ingress.ip.address.to_string(),
            "10.57.1.64",
            "after Docker's slice"
        );

        let taken = store
            .alloc_ip(&Actor::ROOT, &obd, &host, name("db"), None)
            .await;
        assert!(matches!(taken, Err(ObeliskdError::Conflict(_))));

        // Freed addresses are handed out again, lowest first.
        store
            .dealloc_ip(&Actor::ROOT, &obd, &host, &name("db"), false)
            .await
            .unwrap();
        let again = store
            .alloc_ip(&Actor::ROOT, &obd, &host, name("cache"), None)
            .await
            .unwrap();
        assert_eq!(again.ip.address.to_string(), "10.57.0.1");

        let listed = load(dir.path()).ips(Some(&obd), Some(&host)).await.unwrap();
        let names: Vec<_> = listed.iter().map(|i| i.ip.name.to_string()).collect();
        assert_eq!(names, ["web", "cache"], "persisted");
        assert!(
            os.calls()
                .contains(&"ensure ip host/db 10.57.0.1".to_string())
        );
        assert!(
            os.calls()
                .contains(&"remove ip host/db 10.57.0.1".to_string())
        );
    }

    #[tokio::test]
    async fn an_ip_tag_persists() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(&Actor::ROOT, &obd, name("host"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        let tagged = store
            .alloc_ip(
                &Actor::ROOT,
                &obd,
                &name("host"),
                name("db"),
                Some(name("pr-42")),
            )
            .await
            .unwrap();
        assert_eq!(tagged.ip.tag, Some(name("pr-42")));
        let file = std::fs::read_to_string(dir.path().join("alloc/obd/host.toml")).unwrap();
        assert!(file.contains("tag = \"pr-42\""), "{file}");
        let reloaded = load(dir.path()).ips(None, None).await.unwrap();
        assert_eq!(reloaded[0].ip.tag, Some(name("pr-42")));
    }

    #[tokio::test]
    async fn dealloc_by_tag_takes_every_tagged_ip_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let (store, os) = load_with(dir.path());
        let (obd, other) = (name("obd"), name("other"));
        let tag = name("pr-42");
        for (supernet, cidr) in [(&obd, "10.57.0.0/16"), (&other, "10.58.0.0/16")] {
            store
                .alloc_supernet(&Actor::ROOT, supernet.clone(), cidr.parse().unwrap())
                .await
                .unwrap();
            for subnet in ["a", "b"] {
                store
                    .alloc_subnet(
                        &Actor::ROOT,
                        supernet,
                        name(subnet),
                        SubnetKind::Host,
                        24,
                        None,
                    )
                    .await
                    .unwrap();
                store
                    .alloc_ip(
                        &Actor::ROOT,
                        supernet,
                        &name(subnet),
                        name("db"),
                        Some(tag.clone()),
                    )
                    .await
                    .unwrap();
                store
                    .alloc_ip(&Actor::ROOT, supernet, &name(subnet), name("web"), None)
                    .await
                    .unwrap();
            }
        }
        let paths = |tree: &[Supernet]| -> Vec<String> {
            tree.iter()
                .flat_map(|s| {
                    s.subnets.iter().flat_map(move |sub| {
                        sub.ips
                            .iter()
                            .map(move |ip| format!("{}/{}/{}", s.name, sub.name, ip.name))
                    })
                })
                .collect()
        };

        // Narrowed to one subnet, then one supernet, then everywhere left.
        let removed = store
            .dealloc_ips_tagged(&Actor::ROOT, Some(&obd), Some(&name("a")), &tag, false)
            .await
            .unwrap();
        assert_eq!(paths(&removed), ["obd/a/db"]);
        assert_eq!(os.calls().last().unwrap(), "remove ip a/db 10.57.0.1");
        let removed = store
            .dealloc_ips_tagged(&Actor::ROOT, Some(&obd), None, &tag, false)
            .await
            .unwrap();
        assert_eq!(paths(&removed), ["obd/b/db"]);
        let removed = store
            .dealloc_ips_tagged(&Actor::ROOT, None, None, &tag, false)
            .await
            .unwrap();
        assert_eq!(paths(&removed), ["other/a/db", "other/b/db"]);

        // Nothing tagged is left; the untagged ips stayed, also on disk.
        let none = store
            .dealloc_ips_tagged(&Actor::ROOT, None, None, &tag, false)
            .await;
        assert!(matches!(none, Err(ObeliskdError::NotFound(m)) if m == "no ips tagged pr-42"));
        let left = load(dir.path()).ips(None, None).await.unwrap();
        assert_eq!(left.len(), 4);
        assert!(left.iter().all(|i| i.ip.name == name("web")));
    }

    #[tokio::test]
    async fn dealloc_by_tag_needs_real_parents() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        with_ip(&store).await;
        let tag = name("pr-42");
        assert!(matches!(
            store
                .dealloc_ips_tagged(&Actor::ROOT, None, Some(&name("host")), &tag, false)
                .await,
            Err(ObeliskdError::Invalid(_))
        ));
        assert!(matches!(
            store
                .dealloc_ips_tagged(&Actor::ROOT, Some(&name("x")), None, &tag, false)
                .await,
            Err(ObeliskdError::NotFound(_))
        ));
        let elsewhere = store
            .dealloc_ips_tagged(
                &Actor::ROOT,
                Some(&name("obd")),
                Some(&name("host")),
                &name("pr-7"),
                false,
            )
            .await;
        assert!(
            matches!(&elsewhere, Err(ObeliskdError::NotFound(m)) if m == "no ips tagged pr-7 in subnet obd/host"),
            "{elsewhere:?}"
        );
        assert_eq!(store.ips(None, None).await.unwrap().len(), 1);
    }

    const KALIN: fn() -> Actor = || Actor {
        uid: 1000,
        gids: vec![1000, 984],
    };

    /// obd (10.57.0.0/16) with host subnets a and b, as root.
    async fn with_subnets(store: &Store) {
        let obd = name("obd");
        store
            .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        for subnet in ["a", "b"] {
            store
                .alloc_subnet(&Actor::ROOT, &obd, name(subnet), SubnetKind::Host, 24, None)
                .await
                .unwrap();
        }
    }

    fn forbidden<T>(result: &Result<T, ObeliskdError>) -> bool {
        matches!(result, Err(ObeliskdError::Forbidden(_)))
    }

    fn obd() -> Scope {
        Scope::Supernet(name("obd"))
    }

    fn obd_a() -> Scope {
        Scope::Subnet(name("obd"), name("a"))
    }

    fn grant(who: Principal, role: Role) -> Grant {
        Grant { who, role }
    }

    #[tokio::test]
    async fn grants_persist_with_their_scope_and_go_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        with_subnets(&store).await;
        let root = &Actor::ROOT;
        store
            .grant(root, Scope::All, grant(Principal::Group(984), Role::Use))
            .await
            .unwrap();
        store
            .grant(root, obd(), grant(Principal::User(1000), Role::Manage))
            .await
            .unwrap();
        store
            .grant(root, obd_a(), grant(Principal::Everyone, Role::Use))
            .await
            .unwrap();

        let read = |file: &str| std::fs::read_to_string(dir.path().join(file)).unwrap();
        assert!(read("config.toml").contains("who = \"group:984\""));
        assert!(read("alloc/obd.toml").contains("who = \"user:1000\""));
        assert!(read("alloc/obd/a.toml").contains("who = \"everyone\""));
        let all = load(dir.path()).grants(&Scope::All, None).await.unwrap();
        assert_eq!(
            all,
            [
                (Scope::All, grant(Principal::Group(984), Role::Use)),
                (obd(), grant(Principal::User(1000), Role::Manage)),
                (obd_a(), grant(Principal::Everyone, Role::Use)),
            ],
            "reloaded, in tree order"
        );
        assert_eq!(
            store.grants(&obd(), None).await.unwrap().len(),
            2,
            "obd and inside it"
        );
        assert_eq!(
            store
                .grants(&Scope::All, Some(Principal::Everyone))
                .await
                .unwrap()
                .len(),
            1
        );

        // A subnet's grants go with it; the whole tree's file goes when it has none.
        store
            .dealloc_subnet(root, &name("obd"), &name("a"), false)
            .await
            .unwrap();
        assert_eq!(store.grants(&Scope::All, None).await.unwrap().len(), 2);
        store
            .revoke(root, Scope::All, Principal::Group(984))
            .await
            .unwrap();
        assert!(!dir.path().join("config.toml").exists());
    }

    #[tokio::test]
    async fn roles_inherit_down_and_gate_changes() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        with_subnets(&store).await;
        let (obd_name, a) = (name("obd"), name("a"));

        let denied = store
            .alloc_ip(&KALIN(), &obd_name, &a, name("db"), None)
            .await;
        assert!(
            matches!(&denied, Err(ObeliskdError::Forbidden(m)) if m == "uid 1000 needs use on subnet obd/a (has nothing); ask an admin of it, or use sudo"),
            "{denied:?}"
        );

        // Through a group, granted on the supernet: use reaches its subnets.
        store
            .grant(&Actor::ROOT, obd(), grant(Principal::Group(984), Role::Use))
            .await
            .unwrap();
        store
            .alloc_ip(&KALIN(), &obd_name, &a, name("db"), None)
            .await
            .unwrap();
        store
            .alloc_host(&KALIN(), host_on("app.obd.internal", "a", "db"))
            .await
            .unwrap();
        assert!(forbidden(
            &store
                .alloc_subnet(&KALIN(), &obd_name, name("c"), SubnetKind::Host, 24, None)
                .await
        ));

        // Manage on the supernet: subnets too; supernets stay root-only, whatever the grant.
        store
            .grant(
                &Actor::ROOT,
                Scope::All,
                grant(Principal::User(1000), Role::Admin),
            )
            .await
            .unwrap();
        store
            .alloc_subnet(&KALIN(), &obd_name, name("c"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        assert!(forbidden(
            &store
                .alloc_supernet(&KALIN(), name("ex"), "10.58.0.0/16".parse().unwrap())
                .await
        ));
        assert!(forbidden(
            &store.dealloc_supernet(&KALIN(), &obd_name, true).await
        ));

        // Unknown places are still 404, not 403.
        let missing = store
            .alloc_ip(&KALIN(), &obd_name, &name("zz"), name("x"), None)
            .await;
        assert!(
            matches!(missing, Err(ObeliskdError::NotFound(_))),
            "{missing:?}"
        );
    }

    #[tokio::test]
    async fn admins_grant_only_within_their_scope() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        with_subnets(&store).await;
        assert!(forbidden(
            &store
                .grant(&KALIN(), obd_a(), grant(Principal::Everyone, Role::Use))
                .await
        ));
        store
            .grant(
                &Actor::ROOT,
                obd_a(),
                grant(Principal::User(1000), Role::Admin),
            )
            .await
            .unwrap();

        store
            .grant(&KALIN(), obd_a(), grant(Principal::User(4242), Role::Use))
            .await
            .unwrap();
        store
            .grant(
                &KALIN(),
                obd_a(),
                grant(Principal::User(4242), Role::Manage),
            )
            .await
            .unwrap();
        let theirs = store
            .grants(&obd_a(), Some(Principal::User(4242)))
            .await
            .unwrap();
        assert_eq!(
            theirs,
            [(obd_a(), grant(Principal::User(4242), Role::Manage))],
            "replaced"
        );

        assert!(forbidden(
            &store
                .grant(&KALIN(), obd(), grant(Principal::User(4242), Role::Use))
                .await
        ));
        assert!(forbidden(
            &store
                .revoke(&KALIN(), Scope::All, Principal::User(1000))
                .await
        ));
        store
            .revoke(&KALIN(), obd_a(), Principal::User(4242))
            .await
            .unwrap();
        let gone = store.revoke(&KALIN(), obd_a(), Principal::User(4242)).await;
        assert!(
            matches!(&gone, Err(ObeliskdError::NotFound(m)) if m == "user:4242 has no grant on subnet obd/a"),
            "{gone:?}"
        );
        let nowhere = store
            .grant(
                &Actor::ROOT,
                Scope::Supernet(name("nope")),
                grant(Principal::Everyone, Role::Use),
            )
            .await;
        assert!(matches!(nowhere, Err(ObeliskdError::NotFound(_))));
    }

    #[tokio::test]
    async fn dealloc_by_tag_needs_use_everywhere_it_reaches() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        with_subnets(&store).await;
        let (obd_name, tag) = (name("obd"), name("pr-42"));
        for subnet in ["a", "b"] {
            store
                .alloc_ip(
                    &Actor::ROOT,
                    &obd_name,
                    &name(subnet),
                    name("db"),
                    Some(tag.clone()),
                )
                .await
                .unwrap();
        }
        store
            .grant(
                &Actor::ROOT,
                obd_a(),
                grant(Principal::User(1000), Role::Use),
            )
            .await
            .unwrap();

        let denied = store
            .dealloc_ips_tagged(&KALIN(), None, None, &tag, false)
            .await;
        assert!(
            matches!(&denied, Err(ObeliskdError::Forbidden(m)) if m.starts_with("uid 1000 needs use on subnet obd/b ")),
            "{denied:?}"
        );
        assert_eq!(
            store.ips(None, None).await.unwrap().len(),
            2,
            "nothing was removed"
        );
        let removed = store
            .dealloc_ips_tagged(&KALIN(), Some(&obd_name), Some(&name("a")), &tag, false)
            .await
            .unwrap();
        assert_eq!(removed[0].subnets[0].name, name("a"));
    }

    #[tokio::test]
    async fn effective_roles_count_groups_everyone_and_parents() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        with_subnets(&store).await;
        let root = &Actor::ROOT;
        store
            .grant(root, Scope::All, grant(Principal::Everyone, Role::Use))
            .await
            .unwrap();
        store
            .grant(root, obd(), grant(Principal::Group(984), Role::Manage))
            .await
            .unwrap();
        store
            .grant(root, obd_a(), grant(Principal::User(4242), Role::Admin))
            .await
            .unwrap();

        let mine = store.effective(&KALIN(), None).await.unwrap();
        let summary: Vec<(Scope, Role, usize)> = mine
            .iter()
            .map(|e| (e.scope.clone(), e.role, e.from.len()))
            .collect();
        assert_eq!(
            summary,
            [(Scope::All, Role::Use, 1), (obd(), Role::Manage, 2)]
        );

        let on_a = store.effective(&KALIN(), Some(&obd_a())).await.unwrap();
        assert_eq!(on_a[0].role, Role::Manage, "inherited from obd");
        let stranger = Actor {
            uid: 4242,
            gids: vec![4242],
        };
        assert_eq!(
            store.effective(&stranger, Some(&obd_a())).await.unwrap()[0].role,
            Role::Admin
        );
        let rooted = store.effective(root, None).await.unwrap();
        assert_eq!(
            (rooted[0].scope.clone(), rooted[0].role),
            (Scope::All, Role::Admin)
        );
        assert!(matches!(
            store
                .effective(&KALIN(), Some(&Scope::Supernet(name("nope"))))
                .await,
            Err(ObeliskdError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn subnets_with_ips_need_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let (store, os) = load_with(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(&Actor::ROOT, &obd, name("host"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        store
            .alloc_ip(&Actor::ROOT, &obd, &name("host"), name("db"), None)
            .await
            .unwrap();

        let busy = store
            .dealloc_subnet(&Actor::ROOT, &obd, &name("host"), false)
            .await;
        assert!(matches!(busy, Err(ObeliskdError::Conflict(m)) if m.contains("still has 1 ip;")));
        let removed = store
            .dealloc_subnet(&Actor::ROOT, &obd, &name("host"), true)
            .await
            .unwrap();
        assert_eq!(removed.ips.len(), 1);
        assert!(os.calls().ends_with(&[
            "remove ip host/db 10.57.0.1".to_string(),
            "remove subnet obd/host 10.57.0.0/24".to_string(),
        ]));
    }

    #[tokio::test]
    async fn ip_lookups_need_real_parents() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        assert!(matches!(
            store.ips(None, Some(&name("x"))).await,
            Err(ObeliskdError::Invalid(_))
        ));
        assert!(matches!(
            store.ips(Some(&name("x")), None).await,
            Err(ObeliskdError::NotFound(_))
        ));
        let missing = store
            .alloc_ip(&Actor::ROOT, &name("x"), &name("y"), name("z"), None)
            .await;
        assert!(matches!(missing, Err(ObeliskdError::NotFound(_))));
    }

    async fn with_ip(store: &Store) {
        let obd = name("obd");
        store
            .alloc_supernet(&Actor::ROOT, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(&Actor::ROOT, &obd, name("host"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        store
            .alloc_ip(
                &Actor::ROOT,
                &obd,
                &name("host"),
                name("web"),
                Some(name("pr-42")),
            )
            .await
            .unwrap();
    }

    fn host_on(hostname: &str, subnet: &str, ip: &str) -> Host {
        Host {
            name: Hostname::parse(hostname).unwrap(),
            supernet: name("obd"),
            subnet: name(subnet),
            ip: name(ip),
        }
    }

    fn host(hostname: &str) -> Host {
        Host {
            name: Hostname::parse(hostname).unwrap(),
            supernet: name("obd"),
            subnet: name("host"),
            ip: name("web"),
        }
    }

    #[tokio::test]
    async fn hosts_resolve_right_away_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        with_ip(&store).await;
        let table = store.host_table();

        let app = store
            .alloc_host(&Actor::ROOT, host("app.obd.internal"))
            .await
            .unwrap();
        assert_eq!(app.address.to_string(), "10.57.0.1");
        store
            .alloc_host(&Actor::ROOT, host("admin.obd.internal"))
            .await
            .unwrap();
        assert_eq!(
            table
                .read()
                .unwrap()
                .lookup("app.obd.internal")
                .map(|a| a.to_string()),
            Some("10.57.0.1".to_string()),
            "DNS sees it without a restart"
        );

        let taken = store
            .alloc_host(&Actor::ROOT, host("app.obd.internal"))
            .await;
        assert!(matches!(taken, Err(ObeliskdError::Conflict(_))));
        let mut missing = host("x.obd.internal");
        missing.ip = name("nope");
        assert!(matches!(
            store.alloc_host(&Actor::ROOT, missing).await,
            Err(ObeliskdError::NotFound(_))
        ));

        let reloaded = load(dir.path());
        let names: Vec<_> = reloaded
            .hosts()
            .await
            .iter()
            .map(|h| h.host.name.to_string())
            .collect();
        assert_eq!(
            names,
            ["admin.obd.internal", "app.obd.internal"],
            "persisted, sorted"
        );
        let file = std::fs::read_to_string(dir.path().join("alloc/obd/host.toml")).unwrap();
        assert!(
            file.contains("hosts = [\"admin.obd.internal\", \"app.obd.internal\"]"),
            "hosts live on their ip, sorted: {file}"
        );

        store
            .dealloc_host(&Actor::ROOT, &Hostname::parse("app.obd.internal").unwrap())
            .await
            .unwrap();
        assert!(table.read().unwrap().lookup("app.obd.internal").is_none());
        let file = std::fs::read_to_string(dir.path().join("alloc/obd/host.toml")).unwrap();
        assert!(!file.contains("app.obd.internal"), "{file}");
        let gone = store
            .dealloc_host(&Actor::ROOT, &Hostname::parse("app.obd.internal").unwrap())
            .await;
        assert!(matches!(gone, Err(ObeliskdError::NotFound(_))));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // a table of cases
    async fn recursive_deallocs_take_hosts_along() {
        type Dealloc =
            fn(&Store) -> std::pin::Pin<Box<dyn Future<Output = Result<(), ObeliskdError>> + '_>>;
        let levels: [(&str, Dealloc, Dealloc); 4] = [
            (
                "ip",
                |s| {
                    Box::pin(async {
                        s.dealloc_ip(
                            &Actor::ROOT,
                            &name("obd"),
                            &name("host"),
                            &name("web"),
                            false,
                        )
                        .await
                        .map(|_| ())
                    })
                },
                |s| {
                    Box::pin(async {
                        s.dealloc_ip(
                            &Actor::ROOT,
                            &name("obd"),
                            &name("host"),
                            &name("web"),
                            true,
                        )
                        .await
                        .map(|_| ())
                    })
                },
            ),
            (
                "subnet",
                |s| {
                    Box::pin(async {
                        s.dealloc_subnet(&Actor::ROOT, &name("obd"), &name("host"), false)
                            .await
                            .map(|_| ())
                    })
                },
                |s| {
                    Box::pin(async {
                        s.dealloc_subnet(&Actor::ROOT, &name("obd"), &name("host"), true)
                            .await
                            .map(|_| ())
                    })
                },
            ),
            (
                "tag",
                |s| {
                    Box::pin(async {
                        s.dealloc_ips_tagged(&Actor::ROOT, None, None, &name("pr-42"), false)
                            .await
                            .map(|_| ())
                    })
                },
                |s| {
                    Box::pin(async {
                        s.dealloc_ips_tagged(&Actor::ROOT, None, None, &name("pr-42"), true)
                            .await
                            .map(|_| ())
                    })
                },
            ),
            (
                "supernet",
                |s| {
                    Box::pin(async {
                        s.dealloc_supernet(&Actor::ROOT, &name("obd"), false)
                            .await
                            .map(|_| ())
                    })
                },
                |s| {
                    Box::pin(async {
                        s.dealloc_supernet(&Actor::ROOT, &name("obd"), true)
                            .await
                            .map(|_| ())
                    })
                },
            ),
        ];
        for (level, plain, recursive) in levels {
            let dir = tempfile::tempdir().unwrap();
            let store = load(dir.path());
            with_ip(&store).await;
            store
                .alloc_host(&Actor::ROOT, host("app.obd.internal"))
                .await
                .unwrap();
            let table = store.host_table();

            let error = plain(&store).await.unwrap_err();
            assert!(
                matches!(error, ObeliskdError::Conflict(_)),
                "{level}: {error}"
            );
            assert!(
                error.to_string().contains("--recursive"),
                "{level}: {error}"
            );
            assert_eq!(store.hosts().await.len(), 1, "{level}: nothing changed");

            recursive(&store).await.unwrap();
            assert!(store.hosts().await.is_empty(), "{level}: the host went too");
            assert!(
                table.read().unwrap().lookup("app.obd.internal").is_none(),
                "{level}: DNS forgot it"
            );
            assert!(
                load(dir.path()).hosts().await.is_empty(),
                "{level}: persisted"
            );
        }
    }

    #[tokio::test]
    async fn refuses_a_hostname_on_two_ips() {
        let dir = tempfile::tempdir().unwrap();
        let file = |subnet: &str, cidr: &str, address: &str| {
            format!(
                "name = \"{subnet}\"\nkind = \"host\"\ncidr = \"{cidr}\"\n\n[[ips]]\nname = \"web\"\naddress = \"{address}\"\nhosts = [\"app.obd.internal\"]\n"
            )
        };
        std::fs::create_dir_all(dir.path().join("alloc/obd")).unwrap();
        std::fs::write(
            dir.path().join("alloc/obd.toml"),
            "name = \"obd\"\ncidr = \"10.57.0.0/16\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("alloc/obd/a.toml"),
            file("a", "10.57.0.0/24", "10.57.0.1"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("alloc/obd/b.toml"),
            file("b", "10.57.1.0/24", "10.57.1.1"),
        )
        .unwrap();
        let error = Store::load(dir.path(), Arc::new(RecordingOs::default()))
            .map(|_| ())
            .unwrap_err();
        assert!(
            error.to_string().contains("app.obd.internal is on both"),
            "{error}"
        );
    }

    #[test]
    fn refuses_a_config_it_cant_parse() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "supernets = 7").unwrap();
        assert!(matches!(
            Store::load(dir.path(), Arc::new(RecordingOs::default())),
            Err(ObeliskdError::StateParse { .. })
        ));
    }
}
