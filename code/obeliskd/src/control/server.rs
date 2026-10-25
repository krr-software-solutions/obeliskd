//! The daemon's side of the control API.

use std::path::Path;

use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::{IncomingStream, Listener};
use axum::{Json, Router};
use jiff::Timestamp;

use super::Peer;
use super::api::{DaemonInfo, ErrorBody, Health};
use crate::{ObeliskdError, Platform, PlatformImpl};

/// Per-connection info, captured once when the connection is accepted.
#[derive(Clone, Copy, Debug)]
pub struct ConnInfo {
    /// `None` if the OS wouldn't say; the [`auth`](super::auth) extractors then reject.
    pub peer: Option<Peer>,
}

impl Connected<IncomingStream<'_, <Platform as PlatformImpl>::ControlListener>> for ConnInfo {
    fn connect_info(
        stream: IncomingStream<'_, <Platform as PlatformImpl>::ControlListener>,
    ) -> Self {
        let peer = Platform::peer(stream.io())
            .inspect_err(|e| eprintln!("obeliskd: couldn't read a caller's credentials: {e}"))
            .ok();
        ConnInfo { peer }
    }
}

/// An error response: a status, and an [`ErrorBody`].
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn forbidden(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: self.message,
        };
        (self.status, Json(body)).into_response()
    }
}

//

pub fn router(daemon: DaemonInfo) -> Router {
    Router::new()
        .route("/health", get(health))
        .with_state(daemon)
}

async fn health(
    ConnectInfo(conn): ConnectInfo<ConnInfo>,
    State(daemon): State<DaemonInfo>,
) -> Result<Json<Health>, ApiError> {
    let caller = conn
        .peer
        .ok_or_else(|| ApiError::internal("the caller's credentials are unavailable"))?;
    Ok(Json(Health { caller, daemon }))
}

//

/// Serves the control API on `listener` until `shutdown` resolves.
///
/// # Errors
///
/// [`ObeliskdError::ControlServe`] if the server fails.
pub async fn serve<L>(
    listener: L,
    daemon: DaemonInfo,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ObeliskdError>
where
    L: Listener,
    L::Addr: std::fmt::Debug,
    ConnInfo: for<'a> Connected<IncomingStream<'a, L>>,
{
    axum::serve(
        listener,
        router(daemon).into_make_service_with_connect_info::<ConnInfo>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .map_err(ObeliskdError::ControlServe)
}

/// Runs the daemon: binds the control socket at `socket` and serves it until a shutdown signal.
///
/// # Errors
///
/// If the socket can't be bound, or the server fails.
pub async fn run(socket: &Path) -> Result<(), ObeliskdError> {
    let listener = Platform::bind_control(socket)?;
    let daemon = DaemonInfo {
        started_at: Timestamp::now(),
    };
    eprintln!("obeliskd: listening on {}", socket.display());

    let result = serve(listener, daemon, Platform::shutdown_signal()).await;
    Platform::unbind_control(socket);
    eprintln!("obeliskd: stopped");
    result
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::control::auth::Admin;

    const ROOT: Peer = Peer {
        uid: 0,
        gid: 0,
        pid: Some(1),
    };
    const USER: Peer = Peer {
        uid: 1000,
        gid: 1000,
        pid: None,
    };

    async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn started_at() -> DaemonInfo {
        DaemonInfo {
            started_at: "2026-10-25T12:00:00Z".parse().unwrap(),
        }
    }

    #[tokio::test]
    async fn health_reports_the_caller_and_the_daemon() {
        let app = router(started_at()).layer(MockConnectInfo(ConnInfo { peer: Some(USER) }));
        let (status, body) = get_json(app, "/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "caller": { "uid": 1000, "gid": 1000, "pid": null },
                "daemon": { "startedAt": "2026-10-25T12:00:00Z" },
            })
        );
    }

    #[tokio::test]
    async fn admin_routes_reject_non_root() {
        let app = |peer| {
            Router::new()
                .route("/admin", get(|_: Admin| async { Json("ok") }))
                .layer(MockConnectInfo(ConnInfo { peer: Some(peer) }))
        };
        let (status, body) = get_json(app(USER), "/admin").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "uid 1000 isn't root; try again with sudo");

        let (status, body) = get_json(app(ROOT), "/admin").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }
}
