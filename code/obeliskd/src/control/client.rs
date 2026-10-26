//! The CLI's side of the control API.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::api::{
    ErrorBody, Health, HostAlloc, IpAlloc, IpQuery, SubnetAlloc, SubnetDealloc, SubnetQuery,
    SupernetAlloc, SupernetDealloc, SupernetNode,
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
    ) -> Result<SupernetNode, ObeliskdError> {
        let path = format!("/ips/{supernet}/{subnet}/{ip}");
        self.send(self.http.delete(url(&path))).await
    }

    /// `GET /hosts`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or answers with an error.
    pub async fn hosts(&self) -> Result<Vec<SupernetNode>, ObeliskdError> {
        self.get("/hosts").await
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
