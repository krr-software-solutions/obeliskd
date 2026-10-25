//! The control API: HTTP over the daemon's control socket.
//!
//! The daemon serves it ([`server`]), the CLI calls it ([`client`]), and both share the wire
//! types ([`api`]). Who is calling comes from the kernel, per connection ([`Peer`]); handlers state
//! what they need with the [`auth`] extractors.

pub mod api;
pub mod auth;
pub mod client;
pub mod server;

use serde::{Deserialize, Serialize};

/// Who is on the other end of a control connection, as the kernel reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub uid: u32,
    pub gid: u32,
    /// Not reported on every OS.
    pub pid: Option<u32>,
}
