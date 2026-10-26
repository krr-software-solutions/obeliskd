//! The CLI's side of the control API.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::api::{
    ErrorBody, GrantAdd, GrantCheck, GrantCheckQuery, GrantNode, GrantQuery, Health, HostAlloc,
    HostResolved, IpAlloc, IpDealloc, IpDeallocTagged, IpQuery, IpResolved, SubnetAlloc,
    SubnetDealloc, SubnetQuery, SupernetAlloc, SupernetDealloc, SupernetNode,
};
use crate::model::{Hostname, Name};
use crate::{ObeliskdError, Platform, PlatformImpl};

/// Talks to the daemon over its control socket.
pub struct ControlClient {
    http: reqwest::Client,
    socket: PathBuf,
}

impl ControlClient {
    /// # Errors
    ///
    /// [`ObeliskdError::ControlClient`] if the HTTP client can't be built.
    pub fn new(socket: &Path) -> Result<Self, ObeliskdError> {
        Ok(ControlClient {
            http: Platform::control_client(socket)?,
            socket: socket.to_path_buf(),
        })
    }

    /// `GET /health`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or answers with an error.
    pub async fn health(&self) -> Result<Health, ObeliskdError> {
        self.get("/health").await
    }

    /// `GET /supernets`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or answers with an error.
    pub async fn supernets(&self) -> Result<Vec<SupernetNode>, ObeliskdError> {
        self.get("/supernets").await
    }

    /// `POST /supernets`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (e.g. a name taken or an overlap).
    pub async fn alloc_supernet(
        &self,
        request: &SupernetAlloc,
    ) -> Result<SupernetNode, ObeliskdError> {
        self.post("/supernets", request).await
    }

    /// `GET /subnets`, optionally only one supernet's.
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or answers with an error (e.g. an unknown supernet).
    pub async fn subnets(&self, query: &SubnetQuery) -> Result<Vec<SupernetNode>, ObeliskdError> {
        self.send(self.http.get(url("/subnets")).query(query)).await
    }

    /// `POST /subnets`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (e.g. an overlap, or no free range).
    pub async fn alloc_subnet(&self, request: &SubnetAlloc) -> Result<SupernetNode, ObeliskdError> {
        self.post("/subnets", request).await
    }

    /// `DELETE /supernets/{supernet}`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (unknown, or still has subnets without
    /// `recursive`).
    pub async fn dealloc_supernet(
        &self,
        supernet: &Name,
        request: &SupernetDealloc,
    ) -> Result<SupernetNode, ObeliskdError> {
        let path = format!("/supernets/{supernet}");
        self.send(self.http.delete(url(&path)).query(request)).await
    }

    /// `DELETE /subnets/{supernet}/{subnet}`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (unknown supernet or subnet).
    pub async fn dealloc_subnet(
        &self,
        supernet: &Name,
        subnet: &Name,
        request: &SubnetDealloc,
    ) -> Result<SupernetNode, ObeliskdError> {
        let path = format!("/subnets/{supernet}/{subnet}");
        self.send(self.http.delete(url(&path)).query(request)).await
    }

    /// `GET /ips`, optionally filtered by supernet (and subnet).
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or answers with an error (e.g. an unknown subnet).
    pub async fn ips(&self, query: &IpQuery) -> Result<Vec<SupernetNode>, ObeliskdError> {
        self.send(self.http.get(url("/ips")).query(query)).await
    }

    /// `GET /ips/{supernet}/{subnet}/{ip}`: one ip and where it lives, or `None` if it (or its supernet
    /// or subnet) doesn't exist.
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or answers with any other error.
    pub async fn ip(
        &self,
        supernet: &Name,
        subnet: &Name,
        ip: &Name,
    ) -> Result<Option<IpResolved>, ObeliskdError> {
        let path = format!("/ips/{supernet}/{subnet}/{ip}");
        match self.get(&path).await {
            Ok(node) => Ok(Some(node)),
            Err(ObeliskdError::ControlStatus { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// `POST /ips`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (name taken, subnet full, …).
    pub async fn alloc_ip(&self, request: &IpAlloc) -> Result<SupernetNode, ObeliskdError> {
        self.post("/ips", request).await
    }

    /// `DELETE /ips/{supernet}/{subnet}/{ip}`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (unknown ip).
    pub async fn dealloc_ip(
        &self,
        supernet: &Name,
        subnet: &Name,
        ip: &Name,
        request: &IpDealloc,
    ) -> Result<SupernetNode, ObeliskdError> {
        let path = format!("/ips/{supernet}/{subnet}/{ip}");
        self.send(self.http.delete(url(&path)).query(request)).await
    }

    /// `DELETE /ips?tag=…`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (no ips with the tag, hosts still on them).
    pub async fn dealloc_ips_tagged(
        &self,
        request: &IpDeallocTagged,
    ) -> Result<Vec<SupernetNode>, ObeliskdError> {
        self.send(self.http.delete(url("/ips")).query(request))
            .await
    }

    /// `GET /grants`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or answers with an error (an unknown scope, user or group).
    pub async fn grants(&self, query: &GrantQuery) -> Result<Vec<GrantNode>, ObeliskdError> {
        self.send(self.http.get(url("/grants")).query(query)).await
    }

    /// `POST /grants`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (not an admin of the scope, unknown user, …).
    pub async fn add_grant(&self, request: &GrantAdd) -> Result<GrantNode, ObeliskdError> {
        self.post("/grants", request).await
    }

    /// `DELETE /grants`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (no such grant, not an admin of the scope, …).
    pub async fn remove_grant(&self, query: &GrantQuery) -> Result<GrantNode, ObeliskdError> {
        self.send(self.http.delete(url("/grants")).query(query))
            .await
    }

    /// `GET /grants/check`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or answers with an error (an unknown scope or user).
    pub async fn check_grants(&self, query: &GrantCheckQuery) -> Result<GrantCheck, ObeliskdError> {
        self.send(self.http.get(url("/grants/check")).query(query))
            .await
    }

    /// `GET /hosts`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or answers with an error.
    pub async fn hosts(&self) -> Result<Vec<SupernetNode>, ObeliskdError> {
        self.get("/hosts").await
    }

    /// `GET /resolve/{name}`: what DNS answers `name` with, or `None` if nothing matches it.
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or answers with any other error.
    pub async fn resolve_host(
        &self,
        name: &Hostname,
    ) -> Result<Option<HostResolved>, ObeliskdError> {
        match self.get(&format!("/resolve/{name}")).await {
            Ok(found) => Ok(Some(found)),
            Err(ObeliskdError::ControlStatus { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// `POST /hosts`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (hostname taken, no such ip, …).
    pub async fn alloc_host(&self, request: &HostAlloc) -> Result<SupernetNode, ObeliskdError> {
        self.post("/hosts", request).await
    }

    /// `DELETE /hosts/{host}`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (unknown host).
    pub async fn dealloc_host(&self, host: &Hostname) -> Result<SupernetNode, ObeliskdError> {
        let path = format!("/hosts/{host}");
        self.send(self.http.delete(url(&path))).await
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ObeliskdError> {
        self.send(self.http.get(url(path))).await
    }

    async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ObeliskdError> {
        self.send(self.http.post(url(path)).json(body)).await
    }

    async fn send<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, ObeliskdError> {
        let response = request
            .send()
            .await
            .map_err(|source| ObeliskdError::ControlConnect {
                socket: self.socket.clone(),
                source,
            })?;

        let status = response.status();
        if !status.is_success() {
            let message = match response.json::<ErrorBody>().await {
                Ok(body) => body.error,
                Err(_) => status.to_string(),
            };
            return Err(ObeliskdError::ControlStatus {
                status: status.as_u16(),
                message,
            });
        }
        response.json().await.map_err(ObeliskdError::ControlDecode)
    }
}

/// The host is ignored: every request goes to the socket.
fn url(path: &str) -> String {
    format!("http://obeliskd{path}")
}
