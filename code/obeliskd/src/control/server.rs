//! The daemon's side of the control API.

use axum::extract::connect_info::Connected;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::serve::{IncomingStream, Listener};
use axum::{Json, Router};
use std::sync::Arc;

use super::Peer;
use super::api::{
    DaemonInfo, Depth, EffectiveNode, ErrorBody, GrantAdd, GrantCheck, GrantCheckQuery, GrantNode,
    GrantQuery, Health, HostAlloc, IpAlloc, IpDealloc, IpDeallocTagged, IpNode, IpQuery,
    IpResolved, SubnetAlloc, SubnetDealloc, SubnetNode, SubnetQuery, SupernetAlloc,
    SupernetDealloc, SupernetNode, Who, WhoArg,
};
use crate::model::{Actor, Grant, Host, Hostname, Principal, Scope};
use crate::model::{Name, Supernet};
use crate::state::{HostIn, Store};
use crate::users::Users;
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
            .inspect_err(|e| tracing::warn!(error = %e, "couldn't read a caller's credentials"))
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
            ObeliskdError::Forbidden(_) => StatusCode::FORBIDDEN,
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
    /// Names ↔ ids, and the groups a caller is in.
    pub users: Arc<dyn Users>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/supernets", get(list_supernets).post(alloc_supernet))
        .route("/supernets/{supernet}", delete(dealloc_supernet))
        .route("/subnets", get(list_subnets).post(alloc_subnet))
        .route("/subnets/{supernet}/{subnet}", delete(dealloc_subnet))
        .route(
            "/ips",
            get(list_ips).post(alloc_ip).delete(dealloc_ips_tagged),
        )
        .route(
            "/ips/{supernet}/{subnet}/{ip}",
            get(get_ip).delete(dealloc_ip),
        )
        .route("/hosts", get(list_hosts).post(alloc_host))
        .route("/hosts/{host}", delete(dealloc_host))
        .route(
            "/grants",
            get(list_grants).post(add_grant).delete(remove_grant),
        )
        .route("/grants/check", get(check_grants))
        .layer(axum::middleware::from_fn(log_request))
        .with_state(state)
}

/// Logs every request once it's answered: method, path, status, who asked, and how long it took.
async fn log_request(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let (method, uri) = (request.method().clone(), request.uri().clone());
    let uid = request
        .extensions()
        .get::<ConnectInfo<ConnInfo>>()
        .and_then(|ConnectInfo(conn)| conn.peer)
        .map_or_else(|| "?".to_string(), |peer| peer.uid.to_string());
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    let status = response.status();
    let ms = started.elapsed().as_millis();
    if status.is_server_error() {
        tracing::warn!(%uid, "{method} {uri} → {status} ({ms}ms)");
    } else {
        tracing::info!(%uid, "{method} {uri} → {status} ({ms}ms)");
    }
    response
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

fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(b)| b)
        .map_err(|e| ApiError::bad_request(e.body_text()))
}

fn query<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query
        .map(|Query(q)| q)
        .map_err(|e| ApiError::bad_request(e.body_text()))
}

fn path<T>(path: Result<axum::extract::Path<T>, PathRejection>) -> Result<T, ApiError> {
    path.map(|axum::extract::Path(p)| p)
        .map_err(|e| ApiError::bad_request(e.body_text()))
}

/// A tree, down to `depth`.
fn nodes(tree: &[Supernet], depth: Depth) -> Vec<SupernetNode> {
    tree.iter().map(|s| SupernetNode::of(s, depth)).collect()
}

/// The path to a subnet (as `node`) in `supernet`, read back from the store.
async fn subnet_path(
    state: &AppState,
    supernet: &Name,
    node: SubnetNode,
) -> Result<SupernetNode, ApiError> {
    let parent = state
        .store
        .supernet(supernet)
        .await
        .ok_or_else(|| ApiError::internal(format!("supernet {supernet} vanished")))?;
    Ok(SupernetNode::path(&parent, node))
}

/// The path to an ip (as `node`) in `supernet`/`subnet`, read back from the store.
async fn ip_path(
    state: &AppState,
    supernet: &Name,
    subnet: &Name,
    node: IpNode,
) -> Result<SupernetNode, ApiError> {
    let parent = state
        .store
        .supernet(supernet)
        .await
        .ok_or_else(|| ApiError::internal(format!("supernet {supernet} vanished")))?;
    let sub = parent
        .subnets
        .iter()
        .find(|s| &s.name == subnet)
        .ok_or_else(|| ApiError::internal(format!("subnet {supernet}/{subnet} vanished")))?;
    // A host's node is built without the ip's tag: take it from the ip, if it's still there.
    let mut node = node;
    if let Some(ip) = sub.ips.iter().find(|ip| ip.name == node.name) {
        node.tag.clone_from(&ip.tag);
    }
    Ok(SupernetNode::path(&parent, SubnetNode::path(sub, node)))
}

//

async fn list_supernets(
    State(state): State<AppState>,
) -> Result<Json<Vec<SupernetNode>>, ApiError> {
    let tree = state.store.tree(None, None).await?;
    Ok(Json(nodes(&tree, Depth::Supernets)))
}

async fn alloc_supernet(
    actor: Actor,
    State(state): State<AppState>,
    request: Result<Json<SupernetAlloc>, JsonRejection>,
) -> Result<(StatusCode, Json<SupernetNode>), ApiError> {
    let request = json_body(request)?;
    let supernet = state
        .store
        .alloc_supernet(&actor, request.name, request.cidr)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(SupernetNode::of(&supernet, Depth::Supernets)),
    ))
}

async fn dealloc_supernet(
    actor: Actor,
    State(state): State<AppState>,
    name: Result<axum::extract::Path<Name>, PathRejection>,
    request: Result<Query<SupernetDealloc>, QueryRejection>,
) -> Result<Json<SupernetNode>, ApiError> {
    let (name, request) = (path(name)?, query(request)?);
    let removed = state
        .store
        .dealloc_supernet(&actor, &name, request.recursive)
        .await?;
    // Everything that went with it.
    Ok(Json(SupernetNode::of(&removed, Depth::Hosts)))
}

async fn list_subnets(
    State(state): State<AppState>,
    request: Result<Query<SubnetQuery>, QueryRejection>,
) -> Result<Json<Vec<SupernetNode>>, ApiError> {
    let request = query(request)?;
    let tree = state.store.tree(request.supernet.as_ref(), None).await?;
    Ok(Json(nodes(&tree, Depth::Subnets)))
}

async fn alloc_subnet(
    actor: Actor,
    State(state): State<AppState>,
    request: Result<Json<SubnetAlloc>, JsonRejection>,
) -> Result<(StatusCode, Json<SupernetNode>), ApiError> {
    let request = json_body(request)?;
    let subnet = state
        .store
        .alloc_subnet(
            &actor,
            &request.supernet,
            request.name,
            request.kind,
            request.size,
            request.reserve_size,
        )
        .await?;
    let node = SubnetNode::of(&subnet, Depth::Subnets);
    Ok((
        StatusCode::CREATED,
        Json(subnet_path(&state, &request.supernet, node).await?),
    ))
}

async fn dealloc_subnet(
    actor: Actor,
    State(state): State<AppState>,
    names: Result<axum::extract::Path<(Name, Name)>, PathRejection>,
    request: Result<Query<SubnetDealloc>, QueryRejection>,
) -> Result<Json<SupernetNode>, ApiError> {
    let ((supernet, name), request) = (path(names)?, query(request)?);
    let removed = state
        .store
        .dealloc_subnet(&actor, &supernet, &name, request.recursive)
        .await?;
    let node = SubnetNode::of(&removed, Depth::Hosts);
    Ok(Json(subnet_path(&state, &supernet, node).await?))
}

async fn list_ips(
    State(state): State<AppState>,
    request: Result<Query<IpQuery>, QueryRejection>,
) -> Result<Json<Vec<SupernetNode>>, ApiError> {
    let request = query(request)?;
    let mut tree = state
        .store
        .tree(request.supernet.as_ref(), request.subnet.as_ref())
        .await?;
    if let Some(tag) = &request.tag {
        // A filter: keep only that tag's ips, and only the branches that have any.
        for supernet in &mut tree {
            for subnet in &mut supernet.subnets {
                subnet.ips.retain(|ip| ip.tag.as_ref() == Some(tag));
            }
            supernet.subnets.retain(|subnet| !subnet.ips.is_empty());
        }
        tree.retain(|supernet| !supernet.subnets.is_empty());
    }
    Ok(Json(nodes(&tree, Depth::Ips)))
}

async fn alloc_ip(
    actor: Actor,
    State(state): State<AppState>,
    request: Result<Json<IpAlloc>, JsonRejection>,
) -> Result<(StatusCode, Json<SupernetNode>), ApiError> {
    let request = json_body(request)?;
    let ip = state
        .store
        .alloc_ip(
            &actor,
            &request.supernet,
            &request.subnet,
            request.name,
            request.tag,
        )
        .await?;
    let node = IpNode::of(&ip.ip, Depth::Ips);
    Ok((
        StatusCode::CREATED,
        Json(ip_path(&state, &request.supernet, &request.subnet, node).await?),
    ))
}

async fn get_ip(
    State(state): State<AppState>,
    names: Result<axum::extract::Path<(Name, Name, Name)>, PathRejection>,
) -> Result<Json<IpResolved>, ApiError> {
    let (supernet, subnet, name) = path(names)?;
    let tree = state.store.tree(Some(&supernet), Some(&subnet)).await?;
    let parent = &tree[0];
    let sub = &parent.subnets[0];
    let ip = sub
        .ips
        .iter()
        .find(|ip| ip.name == name)
        .ok_or_else(|| ObeliskdError::NotFound(format!("no ip {supernet}/{subnet}/{name}")))?;
    Ok(Json(IpResolved {
        supernet: SupernetNode::of(parent, Depth::Supernets),
        subnet: SubnetNode::of(sub, Depth::Subnets),
        ip: IpNode::of(ip, Depth::Hosts),
    }))
}

async fn dealloc_ip(
    actor: Actor,
    State(state): State<AppState>,
    names: Result<axum::extract::Path<(Name, Name, Name)>, PathRejection>,
    request: Result<Query<IpDealloc>, QueryRejection>,
) -> Result<Json<SupernetNode>, ApiError> {
    let ((supernet, subnet, name), request) = (path(names)?, query(request)?);
    let removed = state
        .store
        .dealloc_ip(&actor, &supernet, &subnet, &name, request.recursive)
        .await?;
    // With any hosts it took along.
    let node = IpNode::of(&removed.ip, Depth::Hosts);
    Ok(Json(ip_path(&state, &supernet, &subnet, node).await?))
}

async fn dealloc_ips_tagged(
    actor: Actor,
    State(state): State<AppState>,
    request: Result<Query<IpDeallocTagged>, QueryRejection>,
) -> Result<Json<Vec<SupernetNode>>, ApiError> {
    let request = query(request)?;
    let removed = state
        .store
        .dealloc_ips_tagged(
            &actor,
            request.supernet.as_ref(),
            request.subnet.as_ref(),
            &request.tag,
            request.recursive,
        )
        .await?;
    // Just the ips that went, with any hosts they took along.
    Ok(Json(nodes(&removed, Depth::Hosts)))
}

async fn list_hosts(State(state): State<AppState>) -> Result<Json<Vec<SupernetNode>>, ApiError> {
    let tree = state.store.tree(None, None).await?;
    Ok(Json(nodes(&tree, Depth::Hosts)))
}

async fn alloc_host(
    actor: Actor,
    State(state): State<AppState>,
    request: Result<Json<HostAlloc>, JsonRejection>,
) -> Result<(StatusCode, Json<SupernetNode>), ApiError> {
    let request = json_body(request)?;
    let host = Host {
        name: request.name,
        supernet: request.supernet,
        subnet: request.subnet,
        ip: request.ip,
    };
    let added = state.store.alloc_host(&actor, host).await?;
    let node = host_node(&added);
    Ok((
        StatusCode::CREATED,
        Json(ip_path(&state, &added.host.supernet, &added.host.subnet, node).await?),
    ))
}

async fn dealloc_host(
    actor: Actor,
    State(state): State<AppState>,
    name: Result<axum::extract::Path<Hostname>, PathRejection>,
) -> Result<Json<SupernetNode>, ApiError> {
    let name = path(name)?;
    let removed = state.store.dealloc_host(&actor, &name).await?;
    let node = host_node(&removed);
    Ok(Json(
        ip_path(&state, &removed.host.supernet, &removed.host.subnet, node).await?,
    ))
}

/// A `--user`/`--group` (name or number), or `--everyone`, as stored.
fn principal(users: &dyn Users, who: &WhoArg) -> Result<Principal, ApiError> {
    let id = |value: &str, kind: &str, lookup: &dyn Fn(&str) -> Option<u32>| {
        value
            .parse::<u32>()
            .ok()
            .or_else(|| lookup(value))
            .ok_or_else(|| ApiError::bad_request(format!("no {kind} named {value}")))
    };
    match (&who.user, &who.group, who.everyone) {
        (Some(user), None, false) => Ok(Principal::User(id(user, "user", &|n| users.user_id(n))?)),
        (None, Some(group), false) => Ok(Principal::Group(id(group, "group", &|n| {
            users.group_id(n)
        })?)),
        (None, None, true) => Ok(Principal::Everyone),
        _ => Err(ApiError::bad_request(
            "name exactly one of user, group or everyone",
        )),
    }
}

/// A stored principal, with its name if the OS knows it.
fn who(users: &dyn Users, principal: Principal) -> Who {
    match principal {
        Principal::User(id) => Who::User {
            id,
            name: users.user_name(id),
        },
        Principal::Group(id) => Who::Group {
            id,
            name: users.group_name(id),
        },
        Principal::Everyone => Who::Everyone,
    }
}

fn grant_node(users: &dyn Users, scope: &Scope, grant: Grant) -> GrantNode {
    GrantNode {
        supernet: scope.supernet().cloned(),
        subnet: scope.subnet().cloned(),
        who: who(users, grant.who),
        role: grant.role,
    }
}

fn scope(supernet: Option<Name>, subnet: Option<Name>) -> Result<Scope, ApiError> {
    Scope::of(supernet, subnet).map_err(ApiError::bad_request)
}

async fn list_grants(
    State(state): State<AppState>,
    request: Result<Query<GrantQuery>, QueryRejection>,
) -> Result<Json<Vec<GrantNode>>, ApiError> {
    let request = query(request)?;
    let who = request.who();
    let principal = if who == WhoArg::default() {
        None
    } else {
        Some(principal(state.users.as_ref(), &who)?)
    };
    let at = scope(request.supernet, request.subnet)?;
    let grants = state.store.grants(&at, principal).await?;
    Ok(Json(
        grants
            .iter()
            .map(|(at, grant)| grant_node(state.users.as_ref(), at, *grant))
            .collect(),
    ))
}

async fn add_grant(
    actor: Actor,
    State(state): State<AppState>,
    request: Result<Json<GrantAdd>, JsonRejection>,
) -> Result<(StatusCode, Json<GrantNode>), ApiError> {
    let request = json_body(request)?;
    let grant = Grant {
        who: principal(state.users.as_ref(), &request.who)?,
        role: request.role,
    };
    let at = scope(request.supernet, request.subnet)?;
    let added = state.store.grant(&actor, at.clone(), grant).await?;
    Ok((
        StatusCode::CREATED,
        Json(grant_node(state.users.as_ref(), &at, added)),
    ))
}

async fn remove_grant(
    actor: Actor,
    State(state): State<AppState>,
    request: Result<Query<GrantQuery>, QueryRejection>,
) -> Result<Json<GrantNode>, ApiError> {
    let request = query(request)?;
    let principal = principal(state.users.as_ref(), &request.who())?;
    let at = scope(request.supernet, request.subnet)?;
    let removed = state.store.revoke(&actor, at.clone(), principal).await?;
    Ok(Json(grant_node(state.users.as_ref(), &at, removed)))
}

async fn check_grants(
    caller: Actor,
    State(state): State<AppState>,
    request: Result<Query<GrantCheckQuery>, QueryRejection>,
) -> Result<Json<GrantCheck>, ApiError> {
    let request = query(request)?;
    let users = state.users.as_ref();
    let actor = match &request.user {
        None => caller,
        Some(user) => {
            let Principal::User(uid) = principal(
                users,
                &WhoArg {
                    user: Some(user.clone()),
                    ..WhoArg::default()
                },
            )?
            else {
                unreachable!("a user was named")
            };
            Actor {
                uid,
                gids: users.groups_of(uid),
            }
        }
    };
    let at = match (request.supernet, request.subnet) {
        (None, None) => None,
        (supernet, subnet) => Some(scope(supernet, subnet)?),
    };
    let scopes = state.store.effective(&actor, at.as_ref()).await?;
    Ok(Json(GrantCheck {
        user: who(users, Principal::User(actor.uid)),
        root: actor.is_root(),
        scopes: scopes
            .into_iter()
            .map(|e| EffectiveNode {
                supernet: e.scope.supernet().cloned(),
                subnet: e.scope.subnet().cloned(),
                role: e.role,
                from: e
                    .from
                    .iter()
                    .map(|(at, grant)| grant_node(users, at, *grant))
                    .collect(),
            })
            .collect(),
    }))
}

/// The ip a host is on, with just that host under it.
fn host_node(host: &HostIn) -> IpNode {
    IpNode {
        name: host.host.ip.clone(),
        address: host.address,
        tag: None,
        hosts: Some(vec![host.host.name.clone()]),
    }
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
    use crate::users::fake::FakeUsers;

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
            users: Arc::new(FakeUsers {
                users: vec![
                    (1000, "kalin", vec![1000, 984]),
                    (4242, "stranger", vec![4242]),
                ],
                groups: vec![(984, "devs"), (1000, "kalin"), (4242, "stranger")],
            }),
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
        // A tree from the supernet down: the path to the new subnet.
        let dev = serde_json::json!({
            "name": "obd", "cidr": "10.57.0.0/16",
            "subnets": [{
                "name": "k3d-dev", "kind": "docker", "cidr": "10.57.0.0/24",
                "dockerRange": "10.57.0.0/26", "dockerGateway": "10.57.0.1",
            }],
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
        assert_eq!(body["name"], "obd");
        assert_eq!(body["subnets"][0]["name"], "a");
        let (status, _) = delete(root(), "/subnets/obd/a").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = delete(root(), "/supernets/obd").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({ "name": "obd", "cidr": "10.57.0.0/16", "subnets": [] })
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
        let db = serde_json::json!({
            "name": "obd", "cidr": "10.57.0.0/16",
            "subnets": [{ "name": "host", "kind": "host", "cidr": "10.57.0.0/24",
                "ips": [{ "name": "db", "address": "10.57.0.1" }] }],
        });
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
        let mut removed = db.clone();
        removed["subnets"][0]["ips"][0]["hosts"] = serde_json::json!([]);
        assert_eq!(
            (status, body),
            (StatusCode::OK, removed),
            "with the hosts it took along"
        );
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
        let entry = serde_json::json!({
            "name": "obd", "cidr": "10.57.0.0/16",
            "subnets": [{ "name": "host", "kind": "host", "cidr": "10.57.0.0/24",
                "ips": [{ "name": "web", "address": "10.57.0.1", "hosts": ["app.obd.internal"] }] }],
        });
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
    async fn ips_carry_an_optional_tag_and_filter_by_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = || app(dir.path(), ROOT);
        post_json(
            root(),
            "/supernets",
            serde_json::json!({ "name": "obd", "cidr": "10.57.0.0/16" }),
        )
        .await;
        for subnet in ["a", "b"] {
            post_json(root(), "/subnets", serde_json::json!({ "supernet": "obd", "name": subnet, "kind": "host", "size": 24 })).await;
        }
        let ip = |subnet: &str, name: &str, tag: Option<&str>| {
            let mut body = serde_json::json!({ "supernet": "obd", "subnet": subnet, "name": name });
            if let Some(tag) = tag {
                body["tag"] = tag.into();
            }
            post_json(root(), "/ips", body)
        };
        let (status, body) = ip("a", "db", Some("pr-42")).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["subnets"][0]["ips"][0]["tag"], "pr-42");
        let (_, body) = ip("a", "web", None).await;
        assert!(
            body["subnets"][0]["ips"][0].get("tag").is_none(),
            "no tag, no field"
        );
        ip("b", "cache", Some("pr-42")).await;
        let (status, _) = ip("b", "x", Some("Not_A_Name")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (_, body) = get_json(root(), "/ips?tag=pr-42").await;
        let names: Vec<_> = body[0]["subnets"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| {
                s["ips"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["name"].as_str().unwrap().to_string())
            })
            .collect();
        assert_eq!(names, ["db", "cache"], "only the tag's ips, across subnets");
        let (_, body) = get_json(root(), "/ips?tag=nothing").await;
        assert_eq!(body, serde_json::json!([]), "no branches without matches");
    }

    #[tokio::test]
    async fn ips_dealloc_by_tag_returns_what_went() {
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
        for (name, tag) in [("db", Some("pr-42")), ("web", None)] {
            let mut body = serde_json::json!({ "supernet": "obd", "subnet": "a", "name": name });
            if let Some(tag) = tag {
                body["tag"] = tag.into();
            }
            post_json(root(), "/ips", body).await;
        }
        post_json(
            root(),
            "/hosts",
            serde_json::json!({ "name": "db.obd.internal", "supernet": "obd", "subnet": "a", "ip": "db" }),
        )
        .await;

        let (status, body) = delete(root(), "/ips?tag=pr-42").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, body) = delete(root(), "/ips?tag=pr-42&recursive=true").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let ip = &body[0]["subnets"][0]["ips"][0];
        assert_eq!(ip["name"], "db");
        assert_eq!(ip["hosts"], serde_json::json!(["db.obd.internal"]));
        assert_eq!(body[0]["subnets"][0]["ips"].as_array().unwrap().len(), 1);
        let (status, _) = delete(root(), "/ips?tag=pr-42").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = delete(root(), "/ips").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "the tag is required");
    }

    #[tokio::test]
    async fn grants_by_name_let_users_change_things() {
        let dir = tempfile::tempdir().unwrap();
        let root = || app(dir.path(), ROOT);
        let kalin = || app(dir.path(), USER);
        post_json(
            root(),
            "/supernets",
            serde_json::json!({ "name": "obd", "cidr": "10.57.0.0/16" }),
        )
        .await;
        let subnet =
            serde_json::json!({ "supernet": "obd", "name": "a", "kind": "host", "size": 24 });
        let (status, _) = post_json(kalin(), "/subnets", subnet.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // kalin is in devs (gid 984) only in the user database: the connection says gid 1000.
        let (status, body) = post_json(
            root(),
            "/grants",
            serde_json::json!({ "supernet": "obd", "group": "devs", "role": "manage" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(
            body,
            serde_json::json!({ "supernet": "obd", "who": { "kind": "group", "id": 984, "name": "devs" }, "role": "manage" })
        );
        let (status, _) = post_json(kalin(), "/subnets", subnet).await;
        assert_eq!(status, StatusCode::CREATED);

        let (_, body) = get_json(kalin(), "/grants/check?supernet=obd&subnet=a").await;
        assert_eq!(
            body["user"],
            serde_json::json!({ "kind": "user", "id": 1000, "name": "kalin" })
        );
        assert_eq!(body["root"], false);
        assert_eq!(body["scopes"][0]["role"], "manage");
        assert_eq!(body["scopes"][0]["from"][0]["who"]["name"], "devs");
        let (_, body) = get_json(kalin(), "/grants/check?user=stranger").await;
        assert_eq!(body["scopes"], serde_json::json!([]));

        let (_, body) = get_json(kalin(), "/grants?group=984").await;
        assert_eq!(body.as_array().unwrap().len(), 1, "by name or number");
        for bad in [
            "/grants?user=nobody",
            "/grants?user=kalin&group=devs",
            "/grants?subnet=a",
        ] {
            let (status, _) = get_json(kalin(), bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }

        let (status, _) = delete(kalin(), "/grants?supernet=obd&group=devs").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "manage can't remove grants");
        let (status, body) = delete(root(), "/grants?supernet=obd&group=devs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["role"], "manage");
        let (status, _) = delete(root(), "/grants?supernet=obd&group=devs").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn one_ip_is_its_path_or_404() {
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
        for name in ["db", "web"] {
            post_json(root(), "/ips", serde_json::json!({ "supernet": "obd", "subnet": "a", "name": name, "tag": "pr-42" })).await;
        }

        // Anyone may look.
        let (status, body) = get_json(app(dir.path(), USER), "/ips/obd/a/web").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "supernet": { "name": "obd", "cidr": "10.57.0.0/16" },
                "subnet": { "name": "a", "kind": "host", "cidr": "10.57.0.0/24" },
                "ip": { "name": "web", "address": "10.57.0.2", "tag": "pr-42", "hosts": [] },
            }),
            "side by side, not nested"
        );
        for missing in ["/ips/obd/a/nope", "/ips/obd/nope/web", "/ips/nope/a/web"] {
            let (status, _) = get_json(root(), missing).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
        }
    }

    #[tokio::test]
    async fn supernet_alloc_needs_root() {
        let dir = tempfile::tempdir().unwrap();
        let stranger = Peer {
            uid: 4242,
            gid: 4242,
            pid: None,
        };
        let body = serde_json::json!({ "name": "obd", "cidr": "10.57.0.0/16" });
        let (status, body) = post_json(app(dir.path(), stranger), "/supernets", body).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body["error"],
            "uid 4242 isn't root, and creating a supernet needs root; try again with sudo"
        );
    }
}
