//! The Unix-socket implementation, shared by Linux and macOS.

use std::io;
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

pub fn unbind_control(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        eprintln!("obeliskd: couldn't remove {}: {e}", path.display());
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;
    use crate::control::api::DaemonInfo;
    use crate::control::client::ControlClient;
    use crate::control::server::serve;
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
        let server = tokio::spawn(serve(listener, daemon, async {
            let _ = stopped.await;
        }));

        let health = ControlClient::new(&socket).unwrap().health().await.unwrap();
        // We created the socket, so it's owned by our uid: the caller the daemon saw.
        assert_eq!(health.caller.uid, std::fs::metadata(&socket).unwrap().uid());
        assert_eq!(health.daemon, daemon);

        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
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
