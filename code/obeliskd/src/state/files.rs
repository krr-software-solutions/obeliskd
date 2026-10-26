//! How the state is laid out on disk: one file per allocation, found by scanning.
//!
//! ```text
//! <state dir>/
//! ├── config.toml                  daemon settings (none yet); never allocations
//! └── alloc/
//!     ├── <supernet>.toml          a supernet: name, cidr
//!     └── <supernet>/
//!         └── <subnet>.toml        a subnet (kind, cidr, docker range/gateway) and its [[ips]],
//!                                  each with the hostnames that resolve to it
//! ```
//!
//! A file's `name` must match its filename, and a `<supernet>/` dir needs its `<supernet>.toml`.
//! Only what changed is written, each file atomically (a temp file, synced, renamed over) with
//! mode `0600` in `0700` dirs. Removals go first (subnet files, then a supernet's dir, then its
//! file) and additions after (a supernet's file before its subnets'), so a crash part-way leaves
//! something smaller but consistent, never subnets without their supernet. Hostnames are unique
//! across all subnet files.

use std::io::Write;
use std::path::{Path, PathBuf};

use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};

use super::Config;
use crate::ObeliskdError;
use crate::model::{Name, Subnet, Supernet, subnet};

pub const CONFIG_FILE: &str = "config.toml";
pub const ALLOC_DIR: &str = "alloc";

const HEADER: &str = "# Managed by obeliskd. Don't edit by hand: use the `obeliskd` CLI.\n\n";
const TMP_SUFFIX: &str = ".tmp";
const TOML_SUFFIX: &str = ".toml";

/// `alloc/<supernet>.toml`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SupernetFile {
    name: Name,
    cidr: Ipv4Net,
}

impl SupernetFile {
    fn of(supernet: &Supernet) -> Self {
        SupernetFile {
            name: supernet.name.clone(),
            cidr: supernet.cidr,
        }
    }
}

/// Older versions kept allocations in `config.toml`: supernets, with subnets nested in them.
#[derive(Debug, Default, Deserialize)]
struct LegacyConfig {
    #[serde(default)]
    supernets: Vec<LegacySupernet>,
}

#[derive(Debug, Deserialize)]
struct LegacySupernet {
    name: Name,
    cidr: Ipv4Net,
    #[serde(default)]
    subnets: Vec<Subnet>,
}

fn alloc(dir: &Path) -> PathBuf {
    dir.join(ALLOC_DIR)
}

fn supernet_path(dir: &Path, supernet: &Name) -> PathBuf {
    alloc(dir).join(format!("{supernet}{TOML_SUFFIX}"))
}

fn subnets_dir(dir: &Path, supernet: &Name) -> PathBuf {
    alloc(dir).join(supernet.as_str())
}

fn subnet_path(dir: &Path, supernet: &Name, subnet: &Name) -> PathBuf {
    subnets_dir(dir, supernet).join(format!("{subnet}{TOML_SUFFIX}"))
}

/// Loads every allocation. Returns whether state had to be migrated from `config.toml` (already
/// saved in the new layout).
///
/// # Errors
///
/// If a file can't be read or parsed, a file's name doesn't match its filename, something is
/// defined twice, a `<supernet>/` dir has no `<supernet>.toml`, or `alloc/` holds anything else.
/// The daemon refuses to start rather than risk overwriting state it couldn't make sense of.
pub fn load(dir: &Path) -> Result<(Config, bool), ObeliskdError> {
    let legacy = read_legacy(dir)?;
    let migrated = !legacy.supernets.is_empty();

    let (files, subnet_dirs) = scan_alloc(dir)?;
    let mut config = Config::default();
    for file in files {
        let subnets = read_subnets(dir, &file.name)?;
        config.supernets.push(Supernet {
            name: file.name,
            cidr: file.cidr,
            subnets,
        });
    }
    for legacy in legacy.supernets {
        if config.supernets.iter().any(|s| s.name == legacy.name) {
            return Err(state_error(
                &supernet_path(dir, &legacy.name),
                format!(
                    "supernet {} is defined both here and in {CONFIG_FILE}",
                    legacy.name
                ),
            ));
        }
        config.supernets.push(Supernet {
            name: legacy.name,
            cidr: legacy.cidr,
            subnets: legacy.subnets,
        });
    }
    for name in subnet_dirs {
        if !config.supernets.iter().any(|s| s.name.as_str() == name) {
            return Err(state_error(
                &alloc(dir).join(&name),
                format!("has no {name}{TOML_SUFFIX} next to it"),
            ));
        }
    }

    config.supernets.sort_by_key(|s| s.cidr.network());
    let mut seen = std::collections::HashMap::new();
    for (host, _) in config.hosts() {
        let at = format!("{}/{}/{}", host.supernet, host.subnet, host.ip);
        if let Some(first) = seen.insert(host.name.clone(), at.clone()) {
            return Err(state_error(
                &subnet_path(dir, &host.supernet, &host.subnet),
                format!("host {} is on both {first} and {at}", host.name),
            ));
        }
    }
    for supernet in &mut config.supernets {
        supernet.subnets.sort_by_key(|s| s.cidr.network());
        // Docker subnets saved before the gateway was recorded: it's always derivable.
        for subnet in &mut supernet.subnets {
            if subnet.docker_range.is_some() && subnet.docker_gateway.is_none() {
                subnet.docker_gateway = Some(subnet::docker_gateway(subnet.cidr));
            }
        }
    }

    if migrated {
        persist(dir, &Config::default(), &config)?;
        strip_legacy(dir)?;
    }
    Ok((config, migrated))
}

fn read_legacy(dir: &Path) -> Result<LegacyConfig, ObeliskdError> {
    let path = dir.join(CONFIG_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            toml::from_str(&text).map_err(|source| ObeliskdError::StateParse { path, source })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LegacyConfig::default()),
        Err(source) => Err(ObeliskdError::StateRead { path, source }),
    }
}

/// Drops `supernets` from `config.toml` once they're saved under `alloc/`: the file is removed
/// if that was all it held.
fn strip_legacy(dir: &Path) -> Result<(), ObeliskdError> {
    let path = dir.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&path).map_err(|source| ObeliskdError::StateRead {
        path: path.clone(),
        source,
    })?;
    let mut table: toml::Table =
        toml::from_str(&text).map_err(|source| ObeliskdError::StateParse {
            path: path.clone(),
            source,
        })?;
    table.remove("supernets");
    if table.is_empty() {
        remove(&path, |p| std::fs::remove_file(p))
    } else {
        write_atomic(&path, &toml::to_string(&table).expect("a table serializes"))
    }
}

/// `alloc/`'s supernet files, and the names of its subdirectories.
fn scan_alloc(dir: &Path) -> Result<(Vec<SupernetFile>, Vec<String>), ObeliskdError> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for (path, file_name, is_dir) in entries(&alloc(dir))? {
        if is_dir {
            dirs.push(file_name);
        } else if let Some(stem) = file_name.strip_suffix(TOML_SUFFIX) {
            let file: SupernetFile = read_toml(&path)?;
            check_name(&path, file.name.as_str(), stem)?;
            files.push(file);
        } else if !file_name.ends_with(TMP_SUFFIX) {
            return Err(state_error(
                &path,
                "not a supernet file (expected <supernet>.toml)",
            ));
        }
    }
    Ok((files, dirs))
}

fn read_subnets(dir: &Path, supernet: &Name) -> Result<Vec<Subnet>, ObeliskdError> {
    let mut subnets = Vec::new();
    for (path, file_name, is_dir) in entries(&subnets_dir(dir, supernet))? {
        if let (false, Some(stem)) = (is_dir, file_name.strip_suffix(TOML_SUFFIX)) {
            let subnet: Subnet = read_toml(&path)?;
            check_name(&path, subnet.name.as_str(), stem)?;
            subnets.push(subnet);
        } else if !file_name.ends_with(TMP_SUFFIX) {
            return Err(state_error(
                &path,
                "not a subnet file (expected <subnet>.toml)",
            ));
        }
    }
    Ok(subnets)
}

/// `(path, file name, is a dir)` for each entry; a missing dir has none. Leftover `*.tmp` files
/// (a write interrupted before its rename; the real file is still the truth) are reported too,
/// for callers to skip.
fn entries(path: &Path) -> Result<Vec<(PathBuf, String, bool)>, ObeliskdError> {
    let read_error = |source| ObeliskdError::StateRead {
        path: path.to_path_buf(),
        source,
    };
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(read_error(source)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(read_error)?;
        let is_dir = entry.file_type().map_err(read_error)?.is_dir();
        out.push((
            entry.path(),
            entry.file_name().to_string_lossy().into_owned(),
            is_dir,
        ));
    }
    Ok(out)
}

fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ObeliskdError> {
    let text = std::fs::read_to_string(path).map_err(|source| ObeliskdError::StateRead {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ObeliskdError::StateParse {
        path: path.to_path_buf(),
        source,
    })
}

fn check_name(path: &Path, name: &str, stem: &str) -> Result<(), ObeliskdError> {
    if name == stem {
        Ok(())
    } else {
        Err(state_error(
            path,
            format!("holds {name}, but its filename says {stem}"),
        ))
    }
}

/// Saves the difference between `old` and `next`.
///
/// # Errors
///
/// [`ObeliskdError::StateWrite`] if a file can't be written or removed.
///
/// # Panics
///
/// Never in practice: the state types always serialize to TOML.
pub fn persist(dir: &Path, old: &Config, next: &Config) -> Result<(), ObeliskdError> {
    // 1. Removals: gone subnets' files; for a gone supernet, then its dir and its file.
    for before in &old.supernets {
        let after = next.supernets.iter().find(|s| s.name == before.name);
        for subnet in &before.subnets {
            if after.is_none_or(|a| !a.subnets.iter().any(|s| s.name == subnet.name)) {
                remove(&subnet_path(dir, &before.name, &subnet.name), |p| {
                    std::fs::remove_file(p)
                })?;
            }
        }
        if after.is_none() {
            remove(&subnets_dir(dir, &before.name), |p| std::fs::remove_dir(p))?;
            remove(&supernet_path(dir, &before.name), |p| {
                std::fs::remove_file(p)
            })?;
        }
    }

    // 2. Additions and changes, only where something differs: a supernet's file before its
    //    subnets'.
    for supernet in &next.supernets {
        let before = old.supernets.iter().find(|s| s.name == supernet.name);
        if before.map(SupernetFile::of) != Some(SupernetFile::of(supernet)) {
            create_private_dir(&alloc(dir))?;
            let body = toml::to_string(&SupernetFile::of(supernet)).expect("a supernet serializes");
            write_atomic(&supernet_path(dir, &supernet.name), &body)?;
        }
        for subnet in &supernet.subnets {
            if before.and_then(|b| b.subnets.iter().find(|s| s.name == subnet.name)) != Some(subnet)
            {
                create_private_dir(&subnets_dir(dir, &supernet.name))?;
                let body = toml::to_string(subnet).expect("a subnet serializes");
                write_atomic(&subnet_path(dir, &supernet.name, &subnet.name), &body)?;
            }
        }
    }
    Ok(())
}

fn remove(path: &Path, op: fn(&Path) -> std::io::Result<()>) -> Result<(), ObeliskdError> {
    match op(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ObeliskdError::StateWrite {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn create_private_dir(path: &Path) -> Result<(), ObeliskdError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(path)
        .map_err(|source| ObeliskdError::StateWrite {
            path: path.to_path_buf(),
            source,
        })
}

/// Writes `HEADER + body` to `path` atomically: a temp file next to it (`0600`), synced, renamed
/// over, and the dir synced so the rename itself is durable.
fn write_atomic(path: &Path, body: &str) -> Result<(), ObeliskdError> {
    let tmp = PathBuf::from(format!("{}{TMP_SUFFIX}", path.display()));
    let write_error = |source| ObeliskdError::StateWrite {
        path: path.to_path_buf(),
        source,
    };

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp).map_err(write_error)?;
    file.write_all(HEADER.as_bytes()).map_err(write_error)?;
    file.write_all(body.as_bytes()).map_err(write_error)?;
    file.sync_all().map_err(write_error)?;
    std::fs::rename(&tmp, path).map_err(write_error)?;
    let parent = path.parent().expect("state files live in a dir");
    std::fs::File::open(parent)
        .and_then(|d| d.sync_all())
        .map_err(write_error)?;
    Ok(())
}

fn state_error(path: &Path, message: impl Into<String>) -> ObeliskdError {
    ObeliskdError::StateDir {
        path: path.to_path_buf(),
        message: message.into(),
    }
}
