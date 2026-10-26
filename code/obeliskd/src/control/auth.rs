//! Who is calling. Reads are open to anyone who can connect (no extractor). Handlers that change
//! things take an [`Actor`] (the caller's uid and every group they're in) and hand it to the store,
//! which checks it against the grants (see [`crate::model::grant`]) under the same lock as the
//! change itself.

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;

use super::Peer;
use super::server::{ApiError, AppState, ConnInfo};
use crate::model::Actor;

/// The caller's kernel-reported credentials.
///
/// # Errors
///
/// A 500 if the connection has none (the OS wouldn't say).
pub async fn peer_of<S: Send + Sync>(parts: &mut Parts, state: &S) -> Result<Peer, ApiError> {
    let ConnectInfo(conn) = ConnectInfo::<ConnInfo>::from_request_parts(parts, state)
        .await
        .map_err(|_| ApiError::internal("no connection info"))?;
    conn.peer
        .ok_or_else(|| ApiError::internal("the caller's credentials are unavailable"))
}

/// The kernel only reports the connection's primary group, so the rest are looked up by uid.
impl FromRequestParts<AppState> for Actor {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = peer_of(parts, state).await?;
        let mut gids = state.users.groups_of(peer.uid);
        if !gids.contains(&peer.gid) {
            gids.push(peer.gid);
        }
        Ok(Actor {
            uid: peer.uid,
            gids,
        })
    }
}
