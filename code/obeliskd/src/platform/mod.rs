//! Everything that differs per OS.
//!
//! [`PlatformImpl`] is the contract; each OS implements it on a unit struct, and exactly one of
//! those is exported as [`Platform`], chosen at compile time. Call through the type
//! (`Platform::name()`), never `&dyn`: the calls are static and other OSes' code isn't compiled.

use std::path::PathBuf;

/// What each OS has to provide.
pub trait PlatformImpl {
    /// The OS's name, e.g. `"linux"`.
    fn name() -> &'static str;

    /// Where the daemon keeps everything that persists: settings, rules, the CA, certs.
    ///
    /// The daemon owns it; nothing in it is edited by hand.
    fn state_dir() -> PathBuf;
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Linux as Platform;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacOs as Platform;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::Windows as Platform;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("obeliskd supports Linux, macOS, and Windows only");
