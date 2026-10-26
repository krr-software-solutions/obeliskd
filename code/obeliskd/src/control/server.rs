//! The daemon's side of the control API.

use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::serve::{IncomingStream, Listener};
use axum::{Json, Router};
use std::sync::Arc;

use super::Peer;
use super::api::{
    DaemonInfo, ErrorBody, Health, HostAlloc, HostEntry, IpAlloc, IpEntry, IpQuery, SubnetAlloc,
    SubnetDealloc, SubnetEntry, SubnetQuery, SupernetAlloc, SupernetDealloc,
};
use super::auth::Admin;
use crate::model::{Host, Hostname};
use crate::model::{Name, Supernet};
use crate::state::{HostIn, IpIn, Store};
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

    pub fn bad_request(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::BAD_REQUEST,
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

impl From<ObeliskdError> for ApiError {
    fn from(error: ObeliskdError) -> Self {
        let status = match error {
            ObeliskdError::Invalid(_) => StatusCode::BAD_REQUEST,
            ObeliskdError::Conflict(_) => StatusCode::CONFLICT,
            ObeliskdError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError {
            status,
            message: error.to_string(),
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

/// What every handler can reach.
#[derive(Clone)]
pub struct AppState {
    pub daemon: DaemonInfo,
    pub store: Arc<Store>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/supernets", get(list_supernets).post(alloc_supernet))
        .route("/supernets/{supernet}", delete(dealloc_supernet))
        .route("/subnets", get(list_subnets).post(alloc_subnet))
        .route("/subnets/{supernet}/{subnet}", delete(dealloc_subnet))
        .route("/ips", get(list_ips).post(alloc_ip))
        .route("/ips/{supernet}/{subnet}/{ip}", delete(dealloc_ip))
        .route("/hosts", get(list_hosts).post(alloc_host))
        .route("/hosts/{host}", delete(dealloc_host))
        .with_state(state)
}

async fn health(
    ConnectInfo(conn): ConnectInfo<ConnInfo>,
    State(state): State<AppState>,
) -> Result<Json<Health>, ApiError> {
    let caller = conn
        .peer
        .ok_or_else(|| ApiError::internal("the caller's credentials are unavailable"))?;
    Ok(Json(Health {
        caller,
        daemon: state.daemon,
    }))
}

/// Reads are open to anyone who can connect; changes need [`Admin`].
async fn list_supernets(State(state): State<AppState>) -> Json<Vec<Supernet>> {
    Json(state.store.supernets().await)
}

async fn alloc_supernet(
    _: Admin,
    State(state): State<AppState>,
    request: Result<Json<SupernetAlloc>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Supernet>), ApiError> {
    let Json(request) = request.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let supernet = state
        .store
        .alloc_supernet(request.name, request.cidr)
        .await?;
    Ok((StatusCode::CREATED, Json(supernet)))
}

async fn list_subnets(
    State(state): State<AppState>,
    query: Result<axum::extract::Query<SubnetQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Vec<SubnetEntry>>, ApiError> {
    let axum::extract::Query(query) = query.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let subnets = state.store.subnets(query.supernet.as_ref()).await?;
    Ok(Json(
        subnets
            .into_iter()
            .map(|(supernet, subnet)| SubnetEntry { supernet, subnet })
            .collect(),
    ))
}

async fn alloc_subnet(
    _: Admin,
    State(state): State<AppState>,
    request: Result<Json<SubnetAlloc>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<SubnetEntry>), ApiError> {
    let Json(request) = request.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let subnet = state
        .store
        .alloc_subnet(
            &request.supernet,
            request.name,
            request.kind,
            request.size,
            request.reserve_size,
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(SubnetEntry {
            supernet: request.supernet,
            subnet,
        }),
    ))
}

async fn dealloc_supernet(
    _: Admin,
    State(state): State<AppState>,
    path: Result<axum::extract::Path<Name>, axum::extract::rejection::PathRejection>,
    query: Result<axum::extract::Query<SupernetDealloc>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Supernet>, ApiError> {
    let axum::extract::Path(name) = path.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let axum::extract::Query(query) = query.map_err(|e| ApiError::bad_request(e.body_text()))?;
    Ok(Json(
        state.store.dealloc_supernet(&name, query.recursive).await?,
    ))
}

async fn dealloc_subnet(
    _: Admin,
    State(state): State<AppState>,
    path: Result<axum::extract::Path<(Name, Name)>, axum::extract::rejection::PathRejection>,
    query: Result<axum::extract::Query<SubnetDealloc>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<SubnetEntry>, ApiError> {
    let axum::extract::Path((supernet, name)) =
        path.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let axum::extract::Query(query) = query.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let subnet = state
        .store
        .dealloc_subnet(&supernet, &name, query.recursive)
        .await?;
    Ok(Json(SubnetEntry { supernet, subnet }))
}

impl From<IpIn> for IpEntry {
    fn from(ip: IpIn) -> Self {
        IpEntry {
            supernet: ip.supernet,
            subnet: ip.subnet,
            kind: ip.kind,
            name: ip.ip.name,
            address: ip.ip.address,
        }
    }
}

async fn list_ips(
    State(state): State<AppState>,
    query: Result<axum::extract::Query<IpQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Vec<IpEntry>>, ApiError> {
    let axum::extract::Query(query) = query.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let ips = state
        .store
        .ips(query.supernet.as_ref(), query.subnet.as_ref())
        .await?;
    Ok(Json(ips.into_iter().map(IpEntry::from).collect()))
}

async fn alloc_ip(
    _: Admin,
    State(state): State<AppState>,
    request: Result<Json<IpAlloc>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<IpEntry>), ApiError> {
    let Json(request) = request.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let ip = state
        .store
        .alloc_ip(&request.supernet, &request.subnet, request.name)
        .await?;
    Ok((StatusCode::CREATED, Json(ip.into())))
}

async fn dealloc_ip(
    _: Admin,
    State(state): State<AppState>,
    path: Result<axum::extract::Path<(Name, Name, Name)>, axum::extract::rejection::PathRejection>,
) -> Result<Json<IpEntry>, ApiError> {
    let axum::extract::Path((supernet, subnet, name)) =
        path.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let ip = state.store.dealloc_ip(&supernet, &subnet, &name).await?;
    Ok(Json(ip.into()))
}

impl From<HostIn> for HostEntry {
    fn from(h: HostIn) -> Self {
        HostEntry {
            name: h.host.name,
            supernet: h.host.supernet,
            subnet: h.host.subnet,
            ip: h.host.ip,
            address: h.address,
        }
    }
}

async fn list_hosts(State(state): State<AppState>) -> Json<Vec<HostEntry>> {
    Json(
        state
            .store
            .hosts()
            .await
            .into_iter()
            .map(HostEntry::from)
            .collect(),
    )
}

async fn alloc_host(
    _: Admin,
    State(state): State<AppState>,
    request: Result<Json<HostAlloc>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<HostEntry>), ApiError> {
    let Json(request) = request.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let host = Host {
        name: request.name,
        supernet: request.supernet,
        subnet: request.subnet,
        ip: request.ip,
    };
    let host = state.store.alloc_host(host).await?;
    Ok((StatusCode::CREATED, Json(host.into())))
}

async fn dealloc_host(
    _: Admin,
    State(state): State<AppState>,
    path: Result<axum::extract::Path<Hostname>, axum::extract::rejection::PathRejection>,
) -> Result<Json<HostEntry>, ApiError> {
    let axum::extract::Path(name) = path.map_err(|e| ApiError::bad_request(e.body_text()))?;
    Ok(Json(state.store.dealloc_host(&name).await?.into()))
}

//

/// Serves the control API on `listener` until `shutdown` resolves.
///
/// # Errors
///
/// [`ObeliskdError::ControlServe`] if the server fails.
pub async fn serve<L>(
    listener: L,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ObeliskdError>
where
    L: Listener,
    L::Addr: std::fmt::Debug,
    ConnInfo: for<'a> Connected<IncomingStream<'a, L>>,
{
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<ConnInfo>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .map_err(ObeliskdError::ControlServe)
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

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
        send(app, Request::get(uri).body(Body::empty()).unwrap()).await
    }

    async fn post_json(
        app: Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::post(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        send(app, request).await
    }

    async fn send(app: Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn started_at() -> DaemonInfo {
        DaemonInfo {
            started_at: "2026-10-25T12:00:00Z".parse().unwrap(),
            dns_address: "127.0.0.79:53".parse().unwrap(),
        }
    }

    fn app(state_dir: &std::path::Path, peer: Peer) -> Router {
        let state = AppState {
            daemon: started_at(),
            store: Arc::new(
                Store::load(state_dir, Arc::new(crate::os::fake::RecordingOs::default())).unwrap(),
            ),
        };
        router(state).layer(MockConnectInfo(ConnInfo { peer: Some(peer) }))
    }

    #[tokio::test]
    async fn health_reports_the_caller_and_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let (status, body) = get_json(app(dir.path(), USER), "/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "caller": { "uid": 1000, "gid": 1000, "pid": null },
                "daemon": { "startedAt": "2026-10-25T12:00:00Z", "dnsAddress": "127.0.0.79:53" },
            })
        );
    }

    #[tokio::test]
    async fn supernets_alloc_as_root_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let obd = serde_json::json!({ "name": "obd", "cidr": "10.57.0.0/16" });

        let (status, body) = post_json(app(dir.path(), ROOT), "/supernets", obd.clone()).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body, obd);

        let (status, body) = get_json(app(dir.path(), ROOT), "/supernets").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!([obd]));
    }

    #[tokio::test]
    async fn supernet_errors_map_to_statuses() {
        let dir = tempfile::tempdir().unwrap();
        let post = |body| post_json(app(dir.path(), ROOT), "/supernets", body);
        post(serde_json::json!({ "name": "obd", "cidr": "10.57.0.0/16" })).await;

        let (status, _) = post(serde_json::json!({ "name": "x", "cidr": "10.57.9.0/24" })).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = post(serde_json::json!({ "name": "x", "cidr": "10.58.1.0/16" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) =
            post(serde_json::json!({ "name": "Bad_Name", "cidr": "10.59.0.0/16" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("lowercase"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn anyone_can_list_supernets() {
        let dir = tempfile::tempdir().unwrap();
        let stranger = Peer {
            uid: 4242,
            gid: 4242,
            pid: None,
        };
        let (status, body) = get_json(app(dir.path(), stranger), "/supernets").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn subnets_alloc_and_list_over_the_api() {
        let dir = tempfile::tempdir().unwrap();
        post_json(
            app(dir.path(), ROOT),
            "/supernets",
            serde_json::json!({ "name": "obd", "cidr": "10.57.0.0/16" }),
        )
        .await;

        let request = serde_json::json!({ "supernet": "obd", "name": "k3d-dev", "kind": "docker", "size": 24,
            "reserveSize": 26 });
        let (status, body) = post_json(app(dir.path(), ROOT), "/subnets", request).await;
        assert_eq!(status, StatusCode::CREATED);
        let dev = serde_json::json!({
            "supernet": "obd", "name": "k3d-dev", "kind": "docker",
            "cidr": "10.57.0.0/24", "dockerRange": "10.57.0.0/26",
            "dockerGateway": "10.57.0.1",
        });
        assert_eq!(body, dev);

        let stranger = Peer {
            uid: 4242,
            gid: 4242,
            pid: None,
        };
        let (status, body) = get_json(app(dir.path(), stranger), "/subnets?supernet=obd").await;
        assert_eq!(status, StatusCode::OK, "reads are open");
        assert_eq!(body, serde_json::json!([dev]));

        let (status, _) = get_json(app(dir.path(), ROOT), "/subnets?supernet=nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let no_reserve =
            serde_json::json!({ "supernet": "obd", "name": "x", "kind": "docker", "size": 24 });
        let (status, _) = post_json(app(dir.path(), ROOT), "/subnets", no_reserve).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let request =
            serde_json::json!({ "supernet": "obd", "name": "x", "kind": "host", "size": 24 });
        let (status, _) = post_json(app(dir.path(), stranger), "/subnets", request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    async fn delete(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        send(app, Request::delete(uri).body(Body::empty()).unwrap()).await
    }

    #[tokio::test]
    async fn dealloc_over_the_api() {
        let dir = tempfile::tempdir().unwrap();
        let root = || app(dir.path(), ROOT);
        post_json(
            root(),
            "/supernets",
            serde_json::json!({ "name": "obd", "cidr": "10.57.0.0/16" }),
        )
        .await;
        post_json(
            root(),
            "/subnets",
            serde_json::json!({ "supernet": "obd", "name": "a", "kind": "host", "size": 24 }),
        )
        .await;

        let stranger = Peer {
            uid: 4242,
            gid: 4242,
            pid: None,
        };
        let (status, _) = delete(app(dir.path(), stranger), "/subnets/obd/a").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = delete(root(), "/supernets/obd").await;
        assert_eq!(status, StatusCode::CONFLICT, "still has a subnet");
        assert!(
            body["error"].as_str().unwrap().contains("--recursive"),
            "{body}"
        );

        let (status, body) = delete(root(), "/subnets/obd/a").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["supernet"], "obd");
        assert_eq!(body["name"], "a");
        let (status, _) = delete(root(), "/subnets/obd/a").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = delete(root(), "/supernets/obd").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({ "name": "obd", "cidr": "10.57.0.0/16" })
        );
        let (status, _) = delete(root(), "/supernets/Bad_Name").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        post_json(
            root(),
            "/supernets",
            serde_json::json!({ "name": "lab", "cidr": "10.58.0.0/16" }),
        )
        .await;
        post_json(
            root(),
            "/subnets",
            serde_json::json!({ "supernet": "lab", "name": "x", "kind": "host", "size": 24 }),
        )
        .await;
        let (status, body) = delete(root(), "/supernets/lab?recursive=true").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["subnets"][0]["name"], "x", "the subnets it took along");
    }

    #[tokio::test]
    async fn ips_over_the_api() {
        let dir = tempfile::tempdir().unwrap();
        let root = || app(dir.path(), ROOT);
        post_json(
            root(),
            "/supernets",
            serde_json::json!({ "name": "obd", "cidr": "10.57.0.0/16" }),
        )
        .await;
        post_json(
            root(),
            "/subnets",
            serde_json::json!({ "supernet": "obd", "name": "host", "kind": "host", "size": 24 }),
        )
        .await;

        let request = serde_json::json!({ "supernet": "obd", "subnet": "host", "name": "db" });
        let (status, body) = post_json(root(), "/ips", request.clone()).await;
        assert_eq!(status, StatusCode::CREATED);
        let db = serde_json::json!({ "supernet": "obd", "subnet": "host", "kind": "host", "name": "db", "address": "10.57.0.1" });
        assert_eq!(body, db);

        let stranger = Peer {
            uid: 4242,
            gid: 4242,
            pid: None,
        };
        let (status, body) =
            get_json(app(dir.path(), stranger), "/ips?supernet=obd&subnet=host").await;
        assert_eq!((status, body), (StatusCode::OK, serde_json::json!([db])));
        let (status, _) = post_json(app(dir.path(), stranger), "/ips", request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = get_json(root(), "/ips?subnet=host").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = delete(root(), "/subnets/obd/host").await;
        assert_eq!(status, StatusCode::CONFLICT, "still has an ip");
        let (status, body) = delete(root(), "/ips/obd/host/db").await;
        assert_eq!((status, body), (StatusCode::OK, db));
        let (status, _) = delete(root(), "/ips/obd/host/db").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn hosts_over_the_api() {
        let dir = tempfile::tempdir().unwrap();
        let root = || app(dir.path(), ROOT);
        post_json(
            root(),
            "/supernets",
            serde_json::json!({ "name": "obd", "cidr": "10.57.0.0/16" }),
        )
        .await;
        post_json(
            root(),
            "/subnets",
            serde_json::json!({ "supernet": "obd", "name": "host", "kind": "host", "size": 24 }),
        )
        .await;
        post_json(
            root(),
            "/ips",
            serde_json::json!({ "supernet": "obd", "subnet": "host", "name": "web" }),
        )
        .await;

        let request = serde_json::json!({ "name": "app.obd.internal", "supernet": "obd", "subnet": "host", "ip": "web" });
        let (status, body) = post_json(root(), "/hosts", request.clone()).await;
        assert_eq!(status, StatusCode::CREATED);
        let entry = serde_json::json!({ "name": "app.obd.internal", "supernet": "obd", "subnet": "host", "ip": "web", "address": "10.57.0.1" });
        assert_eq!(body, entry);

        let stranger = Peer {
            uid: 4242,
            gid: 4242,
            pid: None,
        };
        let (status, body) = get_json(app(dir.path(), stranger), "/hosts").await;
        assert_eq!((status, body), (StatusCode::OK, serde_json::json!([entry])));
        let (status, _) = post_json(app(dir.path(), stranger), "/hosts", request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let relative =
            serde_json::json!({ "name": "app", "supernet": "obd", "subnet": "host", "ip": "web" });
        let (status, _) = post_json(root(), "/hosts", relative).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "full names only");
        let (status, _) = delete(root(), "/ips/obd/host/web").await;
        assert_eq!(status, StatusCode::CONFLICT, "the ip has a host");

        let (status, body) = delete(root(), "/hosts/app.obd.internal").await;
        assert_eq!((status, body), (StatusCode::OK, entry));
        let (status, _) = delete(root(), "/hosts/app.obd.internal").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn supernet_alloc_needs_admin() {
        let dir = tempfile::tempdir().unwrap();
        // Not root, and (unless the tests run as uid 4242) not the daemon's own user.
        let stranger = Peer {
            uid: 4242,
            gid: 4242,
            pid: None,
        };
        let body = serde_json::json!({ "name": "obd", "cidr": "10.57.0.0/16" });
        let (status, _) = post_json(app(dir.path(), stranger), "/supernets", body).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn admin_routes_reject_non_root() {
        use crate::control::auth::Admin;
        let app = |peer| {
            Router::new()
                .route("/admin", get(|_: Admin| async { Json("ok") }))
                .layer(MockConnectInfo(ConnInfo { peer: Some(peer) }))
        };
        let stranger = Peer {
            uid: 4242,
            gid: 4242,
            pid: None,
        };
        let (status, body) = get_json(app(stranger), "/admin").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "uid 4242 isn't root; try again with sudo");

        let (status, body) = get_json(app(ROOT), "/admin").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }
}
