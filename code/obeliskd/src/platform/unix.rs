//! The Unix-socket implementation, shared by Linux and macOS.

use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;

use tokio::net::{UnixListener, UnixStream};

use crate::ObeliskdError;
use crate::control::Peer;

pub fn bind_control(path: &Path) -> Result<UnixListener, ObeliskdError> {
    let bind_error = |source: io::Error| ObeliskdError::ControlBind {
        path: path.to_path_buf(),
        source,
    };

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(bind_error)?;
    }

    // A socket left behind by a daemon that didn't shut down cleanly: replace it, unless something
    // still answers on it.
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() {
            return Err(bind_error(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "exists and isn't a socket",
            )));
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(ObeliskdError::AlreadyRunning {
                path: path.to_path_buf(),
            });
        }
        std::fs::remove_file(path).map_err(bind_error)?;
    }

    let listener = UnixListener::bind(path).map_err(bind_error)?;
    // Anyone may connect: the daemon checks each caller's credentials itself (see control::auth).
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).map_err(bind_error)?;
    Ok(listener)
}

pub fn current_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

pub fn prepare_state_dir(dir: &Path) -> Result<(), ObeliskdError> {
    use nix::sys::stat::{Mode, umask};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    let error = |message: String| ObeliskdError::StateDir {
        path: dir.to_path_buf(),
        message,
    };

    umask(Mode::from_bits_truncate(0o077));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| error(format!("couldn't create it: {e}")))?;

    let uid = current_uid();
    for path in [dir.to_path_buf(), dir.join(crate::state::CONFIG_FILE)] {
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(error(format!("couldn't inspect {}: {e}", path.display()))),
        };
        if metadata.uid() != uid {
            return Err(error(format!(
                "{} is owned by uid {}, not the daemon's uid {uid}",
                path.display(),
                metadata.uid()
            )));
        }
        let mode = metadata.mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(error(format!(
                "{} is writable by others (mode {mode:o}); fix with chmod go-w",
                path.display()
            )));
        }
        if mode & 0o077 != 0 {
            let private = if metadata.is_dir() { 0o700 } else { 0o600 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(private))
                .map_err(|e| error(format!("couldn't tighten {}: {e}", path.display())))?;
            tracing::warn!(
                path = %path.display(),
                "was readable by others (mode {mode:o}); tightened to {private:o}"
            );
        }
    }
    Ok(())
}

/// Whether `address` can be bound here, i.e. the host owns it.
pub fn is_local(address: Ipv4Addr) -> bool {
    UdpSocket::bind((address, 0)).is_ok()
}

pub fn unbind_control(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        tracing::warn!(path = %path.display(), error = %e, "couldn't remove the control socket");
    }
}

pub fn peer(io: &UnixStream) -> io::Result<Peer> {
    let cred = io.peer_cred()?;
    Ok(Peer {
        uid: cred.uid(),
        gid: cred.gid(),
        pid: cred.pid().and_then(|pid| u32::try_from(pid).ok()),
    })
}

pub fn control_client(path: &Path) -> Result<reqwest::Client, ObeliskdError> {
    reqwest::Client::builder()
        .unix_socket(path)
        .build()
        .map_err(ObeliskdError::ControlClient)
}

pub async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).expect("install the SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

pub fn user_id(name: &str) -> Option<u32> {
    let user = nix::unistd::User::from_name(name).ok()??;
    Some(user.uid.as_raw())
}

pub fn user_name(uid: u32) -> Option<String> {
    let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)).ok()??;
    Some(user.name)
}

pub fn group_id(name: &str) -> Option<u32> {
    let group = nix::unistd::Group::from_name(name).ok()??;
    Some(group.gid.as_raw())
}

pub fn group_name(gid: u32) -> Option<String> {
    let group = nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(gid)).ok()??;
    Some(group.name)
}

/// A user's account: its name and primary group.
pub fn account(uid: u32) -> Option<nix::unistd::User> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)).ok()?
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;
    use crate::control::api::DaemonInfo;
    use crate::control::client::ControlClient;
    use crate::control::server::{AppState, serve};
    use crate::model::{Actor, Name, SubnetKind};
    use crate::state::Store;
    use crate::{Platform, PlatformImpl};

    #[tokio::test]
    async fn health_round_trip_over_a_real_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("obeliskd.sock");
        let daemon = DaemonInfo {
            started_at: jiff::Timestamp::now(),
            dns_address: Platform::default_dns_address(),
        };

        let listener = bind_control(&socket).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let store = std::sync::Arc::new(
            Store::load(
                dir.path(),
                std::sync::Arc::new(crate::os::fake::RecordingOs::default()),
            )
            .unwrap(),
        );
        let state = AppState {
            daemon,
            store: store.clone(),
            users: std::sync::Arc::new(crate::users::SystemUsers),
        };
        let server = tokio::spawn(serve(listener, state, async {
            let _ = stopped.await;
        }));

        let health = ControlClient::new(&socket).unwrap().health().await.unwrap();
        // We created the socket, so it's owned by our uid: the caller the daemon saw.
        assert_eq!(health.caller.uid, std::fs::metadata(&socket).unwrap().uid());
        assert_eq!(health.daemon, daemon);

        // One ip: found is its path, missing anything is None (not an error).
        let (root, name) = (&Actor::ROOT, |s: &str| Name::parse(s).unwrap());
        let obd = name("obd");
        store
            .alloc_supernet(root, obd.clone(), "10.57.0.0/16".parse().unwrap())
            .await
            .unwrap();
        store
            .alloc_subnet(root, &obd, name("a"), SubnetKind::Host, 24, None)
            .await
            .unwrap();
        store
            .alloc_ip(root, &obd, &name("a"), name("db"), None)
            .await
            .unwrap();
        let client = ControlClient::new(&socket).unwrap();
        let found = client
            .ip(&obd, &name("a"), &name("db"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.ip.address, Ipv4Addr::new(10, 57, 0, 1));
        assert_eq!(found.subnet.name, name("a"));
        assert_eq!(client.ip(&obd, &name("a"), &name("x")).await.unwrap(), None);
        assert_eq!(
            client.ip(&obd, &name("x"), &name("db")).await.unwrap(),
            None
        );

        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
    }

    #[test]
    fn prepare_state_dir_creates_privately_and_refuses_writable_dirs() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        prepare_state_dir(&dir).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);

        // Readable by others: tightened.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        prepare_state_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);

        // Writable by others: refused.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
        let error = prepare_state_dir(&dir).unwrap_err();
        assert!(error.to_string().contains("writable by others"), "{error}");
    }

    #[test]
    fn is_local_knows_loopback_from_foreign_addresses() {
        assert!(is_local(Ipv4Addr::LOCALHOST));
        // TEST-NET-1: reserved for documentation, never assigned to a host.
        assert!(!is_local(Ipv4Addr::new(192, 0, 2, 1)));
    }

    #[tokio::test]
    async fn bind_refuses_a_live_socket_and_replaces_a_stale_one() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("obeliskd.sock");

        let live = bind_control(&socket).unwrap();
        assert!(matches!(
            bind_control(&socket),
            Err(ObeliskdError::AlreadyRunning { .. })
        ));

        drop(live); // the file stays behind, like after a crash
        assert!(socket.exists());
        bind_control(&socket).unwrap();
    }
}
