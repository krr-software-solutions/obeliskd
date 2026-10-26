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
use crate::model::subnet::{self, PlaceError};
use crate::model::{Ip, Name, Subnet, SubnetKind, Supernet, ip, supernet};
use crate::os::OsEffects;

pub use files::CONFIG_FILE;

/// Everything obeliskd has allocated, as one tree (on disk it's split across files).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub supernets: Vec<Supernet>,
}

/// The state dir, the config loaded from it, and the OS it's realized on.
///
/// Every change is applied to the OS first, then saved (undoing the OS change if saving fails),
/// and only then visible. Removals go the other way: off the OS, then saved.
pub struct Store {
    dir: PathBuf,
    config: Mutex<Config>,
    os: Arc<dyn OsEffects>,
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
            eprintln!(
                "obeliskd: moved subnets out of {CONFIG_FILE} into {}/<supernet>/<subnet>.toml",
                files::ALLOC_DIR
            );
        }
        Ok(Store {
            dir: dir.to_path_buf(),
            config: Mutex::new(config),
            os,
        })
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
        name: Name,
        cidr: Ipv4Net,
    ) -> Result<Supernet, ObeliskdError> {
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
            subnets: Vec::new(),
        };
        let mut next = config.clone();
        next.supernets.push(supernet.clone());
        self.os.ensure_supernet(&supernet)?;
        if let Err(error) = files::persist(&self.dir, &config, &next) {
            let _ = self.os.remove_supernet(&supernet);
            return Err(error);
        }
        *config = next;
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
        supernet: &Name,
        name: Name,
        kind: SubnetKind,
        size: u8,
        reserve: Option<u8>,
    ) -> Result<Subnet, ObeliskdError> {
        let mut config = self.config.lock().await;
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
            ips: Vec::new(),
        };
        parent.subnets.push(subnet.clone());
        self.os.ensure_subnet(supernet, &subnet)?;
        if let Err(error) = files::persist(&self.dir, &config, &next) {
            let _ = self.os.remove_subnet(supernet, &subnet);
            return Err(error);
        }
        *config = next;
        Ok(subnet)
    }
}

impl Store {
    /// Removes a supernet, and saves. With `recursive`, its subnets go with it; without, it must
    /// have none. The removed supernet (with any subnets it took along) is returned.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if it doesn't exist; [`ObeliskdError::Conflict`] if it still has
    /// subnets and `recursive` is off; or a failure to save (then nothing changes).
    pub async fn dealloc_supernet(
        &self,
        name: &Name,
        recursive: bool,
    ) -> Result<Supernet, ObeliskdError> {
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
        *config = next;
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
        supernet: &Name,
        name: &Name,
        recursive: bool,
    ) -> Result<Subnet, ObeliskdError> {
        let mut config = self.config.lock().await;
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
        *config = next;
        Ok(removed)
    }
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
        supernet: &Name,
        subnet: &Name,
        name: Name,
    ) -> Result<IpIn, ObeliskdError> {
        let mut config = self.config.lock().await;
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
        let new = Ip { name, address };
        parent.ips.push(new.clone());
        let parent = parent.clone();

        self.os.ensure_ip(&parent, &new)?;
        if let Err(error) = files::persist(&self.dir, &config, &next) {
            let _ = self.os.remove_ip(&parent, &new);
            return Err(error);
        }
        *config = next;
        Ok(IpIn {
            supernet: supernet.clone(),
            subnet: subnet.clone(),
            kind: parent.kind,
            ip: new,
        })
    }

    /// Frees an ip (its address can be handed out again), and saves.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::NotFound`] if it (or its supernet/subnet) doesn't exist; or an OS/save
    /// failure (then nothing changes).
    pub async fn dealloc_ip(
        &self,
        supernet: &Name,
        subnet: &Name,
        name: &Name,
    ) -> Result<IpIn, ObeliskdError> {
        let mut config = self.config.lock().await;
        let mut next = config.clone();
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
        *config = next;
        Ok(IpIn {
            supernet: supernet.clone(),
            subnet: subnet.clone(),
            kind,
            ip: removed,
        })
    }
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
            .alloc_supernet(name("obd"), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        let reloaded = load(dir.path());
        assert_eq!(
            reloaded.supernets().await,
            vec![Supernet {
                name: name("obd"),
                cidr: "10.57.0.0/16".parse().unwrap(),
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
            .alloc_supernet(name("obd"), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();

        let taken = store
            .alloc_supernet(name("obd"), "10.58.0.0/16".parse().unwrap())
            .await;
        assert!(matches!(taken, Err(ObeliskdError::Conflict(_))));
        let overlap = store
            .alloc_supernet(name("other"), "10.57.22.0/24".parse().unwrap())
            .await;
        assert!(
            matches!(overlap, Err(ObeliskdError::Conflict(m)) if m.contains("overlaps supernet obd"))
        );
        let unaligned = store
            .alloc_supernet(name("other"), "10.58.1.2/16".parse().unwrap())
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
            .alloc_supernet(obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();

        let host = store
            .alloc_subnet(&obd, name("host"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        assert_eq!(host.cidr, "10.57.0.0/24".parse().unwrap());
        assert_eq!(host.docker_range, None);

        let dev = store
            .alloc_subnet(&obd, name("k3d-dev"), SubnetKind::Docker, 24, Some(26))
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
            .alloc_supernet(obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(&obd, name("host"), SubnetKind::Host, 24, None)
            .await
            .unwrap();

        let unknown = store
            .alloc_subnet(&name("nope"), name("x"), SubnetKind::Host, 24, None)
            .await;
        assert!(matches!(unknown, Err(ObeliskdError::NotFound(_))));
        let taken = store
            .alloc_subnet(&obd, name("host"), SubnetKind::Host, 24, None)
            .await;
        assert!(matches!(taken, Err(ObeliskdError::Conflict(_))));
        let host_reserve = store
            .alloc_subnet(&obd, name("x"), SubnetKind::Host, 24, Some(26))
            .await;
        assert!(matches!(host_reserve, Err(ObeliskdError::Invalid(_))));
        let docker_no_reserve = store
            .alloc_subnet(&obd, name("x"), SubnetKind::Docker, 24, None)
            .await;
        assert!(matches!(docker_no_reserve, Err(ObeliskdError::Invalid(_))));
        let tiny_docker = store
            .alloc_subnet(&obd, name("x"), SubnetKind::Docker, 30, Some(31))
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
            .alloc_supernet(obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        for n in ["a", "b"] {
            store
                .alloc_subnet(&obd, name(n), SubnetKind::Host, 24, None)
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

        store.alloc_ip(&obd, &name("a"), name("db")).await.unwrap();
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
        store.dealloc_supernet(&obd, true).await.unwrap();
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
            .alloc_supernet(obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(&obd, name("a"), SubnetKind::Host, 24, None)
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
            .alloc_supernet(obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        let first = store
            .alloc_subnet(&obd, name("a"), SubnetKind::Host, 24, None)
            .await
            .unwrap();

        // Not empty: refused, and nothing changes.
        let busy = store.dealloc_supernet(&obd, false).await;
        assert!(
            matches!(busy, Err(ObeliskdError::Conflict(m)) if m.contains("still has 1 subnet;"))
        );

        let removed = store.dealloc_subnet(&obd, &name("a"), false).await.unwrap();
        assert_eq!(removed, first);
        // The range is free again: the next /24 lands in the same place.
        let again = store
            .alloc_subnet(&obd, name("b"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        assert_eq!(again.cidr, first.cidr);
        store.dealloc_subnet(&obd, &name("b"), false).await.unwrap();

        store.dealloc_supernet(&obd, false).await.unwrap();
        let reloaded = load(dir.path());
        assert_eq!(reloaded.supernets().await, Vec::new(), "persisted");
    }

    #[tokio::test]
    async fn recursive_dealloc_takes_the_subnets_along() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        for n in ["a", "b"] {
            store
                .alloc_subnet(&obd, name(n), SubnetKind::Host, 24, None)
                .await
                .unwrap();
        }
        let removed = store.dealloc_supernet(&obd, true).await.unwrap();
        assert_eq!(removed.subnets.len(), 2);
        assert_eq!(store.subnets(None).await.unwrap(), Vec::new());
        assert_eq!(load(dir.path()).supernets().await, Vec::new());
    }

    #[tokio::test]
    async fn dealloc_unknown_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path());
        assert!(matches!(
            store.dealloc_supernet(&name("nope"), false).await,
            Err(ObeliskdError::NotFound(_))
        ));
        store
            .alloc_supernet(name("obd"), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        assert!(matches!(
            store.dealloc_subnet(&name("nope"), &name("a"), false).await,
            Err(ObeliskdError::NotFound(_))
        ));
        assert!(matches!(
            store.dealloc_subnet(&name("obd"), &name("a"), false).await,
            Err(ObeliskdError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn changes_hit_the_os_in_order_and_failures_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (store, os) = load_with(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(&obd, name("a"), SubnetKind::Host, 24, None)
            .await
            .unwrap();

        // The OS refuses: nothing is saved.
        *os.fail_next.lock().unwrap() = Some(ObeliskdError::Os {
            command: "docker network create".to_string(),
            message: "boom".to_string(),
        });
        let refused = store
            .alloc_subnet(&obd, name("b"), SubnetKind::Docker, 24, Some(26))
            .await;
        assert!(matches!(refused, Err(ObeliskdError::Os { .. })));
        assert_eq!(load(dir.path()).subnets(None).await.unwrap().len(), 1);

        store.dealloc_supernet(&obd, true).await.unwrap();
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
                .alloc_supernet(obd.clone(), "10.57.0.0/16".parse().unwrap())
                .await
                .unwrap();
            store
                .alloc_subnet(&obd, name("a"), SubnetKind::Host, 24, None)
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
            .alloc_supernet(obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(&obd, name("host"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        store
            .alloc_subnet(&obd, name("dev"), SubnetKind::Docker, 24, Some(26))
            .await
            .unwrap();
        let (host, dev) = (name("host"), name("dev"));

        let db = store.alloc_ip(&obd, &host, name("db")).await.unwrap();
        assert_eq!(
            db.ip.address,
            "10.57.0.1".parse::<std::net::Ipv4Addr>().unwrap()
        );
        let web = store.alloc_ip(&obd, &host, name("web")).await.unwrap();
        assert_eq!(web.ip.address.to_string(), "10.57.0.2");
        let ingress = store.alloc_ip(&obd, &dev, name("ingress")).await.unwrap();
        assert_eq!(
            ingress.ip.address.to_string(),
            "10.57.1.64",
            "after Docker's slice"
        );

        let taken = store.alloc_ip(&obd, &host, name("db")).await;
        assert!(matches!(taken, Err(ObeliskdError::Conflict(_))));

        // Freed addresses are handed out again, lowest first.
        store.dealloc_ip(&obd, &host, &name("db")).await.unwrap();
        let again = store.alloc_ip(&obd, &host, name("cache")).await.unwrap();
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
    async fn subnets_with_ips_need_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let (store, os) = load_with(dir.path());
        let obd = name("obd");
        store
            .alloc_supernet(obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(&obd, name("host"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        store
            .alloc_ip(&obd, &name("host"), name("db"))
            .await
            .unwrap();

        let busy = store.dealloc_subnet(&obd, &name("host"), false).await;
        assert!(matches!(busy, Err(ObeliskdError::Conflict(m)) if m.contains("still has 1 ip;")));
        let removed = store
            .dealloc_subnet(&obd, &name("host"), true)
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
        let missing = store.alloc_ip(&name("x"), &name("y"), name("z")).await;
        assert!(matches!(missing, Err(ObeliskdError::NotFound(_))));
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
