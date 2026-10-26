//! Who may call what. Reads are open to anyone who can connect (no extractor). Handlers that change
//! things take [`Admin`] (root or the daemon's own uid), or [`Caller`] (any allowed user, once
//! access can be granted); the extractor rejects with 403 before the handler runs.

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;

use super::Peer;
use super::server::{ApiError, ConnInfo};
use crate::{Platform, PlatformImpl};

/// Root only, for now; granting access to other users comes later.
fn is_allowed(peer: Peer) -> bool {
    is_admin(peer)
}

/// Root, or the daemon's own user (so a daemon run as yourself in development is yours to manage).
fn is_admin(peer: Peer) -> bool {
    peer.uid == 0 || peer.uid == Platform::current_uid()
}

async fn peer_of<S: Send + Sync>(parts: &mut Parts, state: &S) -> Result<Peer, ApiError> {
    let ConnectInfo(conn) = ConnectInfo::<ConnInfo>::from_request_parts(parts, state)
        .await
        .map_err(|_| ApiError::internal("no connection info"))?;
    conn.peer
        .ok_or_else(|| ApiError::internal("the caller's credentials are unavailable"))
}

/// The request comes from a user allowed to use the control socket.
#[derive(Clone, Copy, Debug)]
pub struct Caller(pub Peer);

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let peer = peer_of(parts, state).await?;
        if is_allowed(peer) {
            Ok(Caller(peer))
        } else {
            Err(ApiError::forbidden(format!(
                "uid {} may not use the control socket",
                peer.uid
            )))
        }
    }
}

/// The request comes from root (e.g. via `sudo`).
#[derive(Clone, Copy, Debug)]
pub struct Admin(pub Peer);

impl<S: Send + Sync> FromRequestParts<S> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let peer = peer_of(parts, state).await?;
        if is_admin(peer) {
            Ok(Admin(peer))
        } else {
            Err(ApiError::forbidden(format!(
                "uid {} isn't root; try again with sudo",
                peer.uid
            )))
        }
    }
}
