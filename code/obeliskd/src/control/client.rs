//! The CLI's side of the control API.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::api::{ErrorBody, Health, SubnetAlloc, SubnetEntry, SubnetQuery, SupernetAlloc};
use crate::model::Supernet;
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
    pub async fn supernets(&self) -> Result<Vec<Supernet>, ObeliskdError> {
        self.get("/supernets").await
    }

    /// `POST /supernets`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (e.g. a name taken or an overlap).
    pub async fn alloc_supernet(&self, request: &SupernetAlloc) -> Result<Supernet, ObeliskdError> {
        self.post("/supernets", request).await
    }

    /// `GET /subnets`, optionally only one supernet's.
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or answers with an error (e.g. an unknown supernet).
    pub async fn subnets(&self, query: &SubnetQuery) -> Result<Vec<SubnetEntry>, ObeliskdError> {
        self.send(self.http.get(url("/subnets")).query(query)).await
    }

    /// `POST /subnets`
    ///
    /// # Errors
    ///
    /// If the daemon can't be reached, or refuses (e.g. an overlap, or no free range).
    pub async fn alloc_subnet(&self, request: &SubnetAlloc) -> Result<SubnetEntry, ObeliskdError> {
        self.post("/subnets", request).await
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
