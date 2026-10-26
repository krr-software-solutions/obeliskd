//! The daemon's persisted state: `config.toml` in the state dir.
//!
//! Only the daemon reads or writes it (dir `0700`, file `0600`); everyone else goes through the
//! control API. Writes are atomic: a temp file in the same dir, synced, then renamed over.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::ObeliskdError;
use crate::model::subnet::{self, PlaceError};
use crate::model::{Name, Subnet, SubnetKind, Supernet, supernet};
use crate::os::OsEffects;

pub const CONFIG_FILE: &str = "config.toml";

const HEADER: &str = "# Managed by obeliskd. Don't edit by hand: use the `obeliskd` CLI.\n\n";

/// Everything in `config.toml`.
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
    /// Loads `config.toml` from `dir`; a missing file is an empty config.
    ///
    /// # Errors
    ///
    /// If the file exists but can't be read or parsed. The daemon refuses to start rather than
    /// risk overwriting state it couldn't understand.
    pub fn load(dir: &Path, os: Arc<dyn OsEffects>) -> Result<Self, ObeliskdError> {
        let path = dir.join(CONFIG_FILE);
        let mut config: Config = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|source| ObeliskdError::StateParse { path, source })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(source) => return Err(ObeliskdError::StateRead { path, source }),
        };
        // Docker subnets saved before the gateway was recorded: it's always derivable.
        for subnet in config
            .supernets
            .iter_mut()
            .flat_map(|s| s.subnets.iter_mut())
        {
            if subnet.docker_range.is_some() && subnet.docker_gateway.is_none() {
                subnet.docker_gateway = Some(subnet::docker_gateway(subnet.cidr));
            }
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
        if let Err(error) = save(&self.dir, &next) {
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
        };
        parent.subnets.push(subnet.clone());
        self.os.ensure_subnet(supernet, &subnet)?;
        if let Err(error) = save(&self.dir, &next) {
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
            self.os.remove_subnet(&removed.name, subnet)?;
        }
        self.os.remove_supernet(&removed)?;
        save(&self.dir, &next)?;
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
        let removed = parent.subnets.remove(index);
        self.os.remove_subnet(supernet, &removed)?;
        save(&self.dir, &next)?;
        *config = next;
        Ok(removed)
    }
}

/// Writes `config` to `dir/config.toml` atomically.
fn save(dir: &Path, config: &Config) -> Result<(), ObeliskdError> {
    let path = dir.join(CONFIG_FILE);
    let tmp = dir.join(format!("{CONFIG_FILE}.tmp"));
    let write_error = |source| ObeliskdError::StateWrite {
        path: path.clone(),
        source,
    };

    let body = toml::to_string(config).expect("the config serializes");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp).map_err(write_error)?;
    file.write_all(HEADER.as_bytes()).map_err(write_error)?;
    file.write_all(body.as_bytes()).map_err(write_error)?;
    file.sync_all().map_err(write_error)?;
    std::fs::rename(&tmp, &path).map_err(write_error)?;
    // The rename itself is only durable once the directory is synced.
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(write_error)?;
    Ok(())
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

        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(text.starts_with("# Managed by obeliskd"));
        assert!(text.contains("[[supernets]]"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(CONFIG_FILE))
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
        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(text.contains("[[supernets.subnets]]"), "{text}");
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

        let removed = store.dealloc_subnet(&obd, &name("a")).await.unwrap();
        assert_eq!(removed, first);
        // The range is free again: the next /24 lands in the same place.
        let again = store
            .alloc_subnet(&obd, name("b"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        assert_eq!(again.cidr, first.cidr);
        store.dealloc_subnet(&obd, &name("b")).await.unwrap();

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
            store.dealloc_subnet(&name("nope"), &name("a")).await,
            Err(ObeliskdError::NotFound(_))
        ));
        assert!(matches!(
            store.dealloc_subnet(&name("obd"), &name("a")).await,
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
