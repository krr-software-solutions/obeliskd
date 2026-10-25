//! Who may call what. Handlers take [`Caller`] (any allowed user) or [`Admin`] (root) as an
//! argument; the extractor rejects with 403 before the handler runs.

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;

use super::Peer;
use super::server::{ApiError, ConnInfo};

/// Root only, for now; granting access to other users comes later.
fn is_allowed(peer: Peer) -> bool {
    is_admin(peer)
}

fn is_admin(peer: Peer) -> bool {
    peer.uid == 0
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
