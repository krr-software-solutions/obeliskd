//! The daemon's persisted state: `config.toml` in the state dir.
//!
//! Only the daemon reads or writes it (dir `0700`, file `0600`); everyone else goes through the
//! control API. Writes are atomic: a temp file in the same dir, synced, then renamed over.

use std::io::Write;
use std::path::{Path, PathBuf};

use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::ObeliskdError;
use crate::model::subnet::{self, PlaceError};
use crate::model::{Name, Subnet, SubnetKind, Supernet, supernet};

pub const CONFIG_FILE: &str = "config.toml";

const HEADER: &str = "# Managed by obeliskd. Don't edit by hand: use the `obeliskd` CLI.\n\n";

/// Everything in `config.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub supernets: Vec<Supernet>,
}

/// The state dir, and the config loaded from it. Every change is saved before it's visible.
pub struct Store {
    dir: PathBuf,
    config: Mutex<Config>,
}

impl Store {
    /// Loads `config.toml` from `dir`; a missing file is an empty config.
    ///
    /// # Errors
    ///
    /// If the file exists but can't be read or parsed. The daemon refuses to start rather than
    /// risk overwriting state it couldn't understand.
    pub fn load(dir: &Path) -> Result<Self, ObeliskdError> {
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
        })
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
        save(&self.dir, &next)?;
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
        save(&self.dir, &next)?;
        *config = next;
        Ok(subnet)
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

    fn name(s: &str) -> Name {
        Name::parse(s).unwrap()
    }

    #[tokio::test]
    async fn supernets_persist_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path()).unwrap();
        assert_eq!(store.supernets().await, Vec::new());

        store
            .alloc_supernet(name("obd"), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        let reloaded = Store::load(dir.path()).unwrap();
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
        let store = Store::load(dir.path()).unwrap();
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
        let store = Store::load(dir.path()).unwrap();
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

        let reloaded = Store::load(dir.path()).unwrap();
        let all = reloaded.subnets(None).await.unwrap();
        assert_eq!(all, vec![(obd.clone(), host), (obd.clone(), dev)]);
        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(text.contains("[[supernets.subnets]]"), "{text}");
    }

    #[tokio::test]
    async fn subnet_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path()).unwrap();
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
        let store = Store::load(dir.path()).unwrap();
        let (_, subnet) = &store.subnets(None).await.unwrap()[0];
        assert_eq!(subnet.docker_gateway, Some("10.57.16.1".parse().unwrap()));
    }

    #[test]
    fn refuses_a_config_it_cant_parse() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "supernets = 7").unwrap();
        assert!(matches!(
            Store::load(dir.path()),
            Err(ObeliskdError::StateParse { .. })
        ));
    }
}
