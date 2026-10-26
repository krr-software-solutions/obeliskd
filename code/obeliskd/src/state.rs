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
use crate::model::{Name, Supernet, supernet};

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
        let config = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|source| ObeliskdError::StateParse { path, source })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(source) => return Err(ObeliskdError::StateRead { path, source }),
        };
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

        let supernet = Supernet { name, cidr };
        let mut next = config.clone();
        next.supernets.push(supernet.clone());
        save(&self.dir, &next)?;
        *config = next;
        Ok(supernet)
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
                cidr: "10.57.0.0/16".parse().unwrap()
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
