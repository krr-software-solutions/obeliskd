//! The allocation model: supernets (and, later, subnets and ips), with their validation rules.

pub mod grant;
pub mod host;
pub mod ip;
pub mod name;
pub mod subnet;
pub mod supernet;

pub use grant::{Actor, Grant, Principal, Role, Scope};
pub use host::{Host, Hostname};
pub use ip::Ip;
pub use name::Name;
pub use subnet::{Subnet, SubnetKind};
pub use supernet::Supernet;
