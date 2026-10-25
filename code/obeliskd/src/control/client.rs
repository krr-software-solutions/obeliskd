//! The CLI's side of the control API.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use super::api::{ErrorBody, Health};
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

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ObeliskdError> {
        // The host is ignored: every request goes to the socket.
        let response = self
            .http
            .get(format!("http://obeliskd{path}"))
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
