//! The allocation model: supernets (and, later, subnets and ips), with their validation rules.

pub mod name;
pub mod subnet;
pub mod supernet;

pub use name::Name;
pub use subnet::{Subnet, SubnetKind};
pub use supernet::Supernet;
