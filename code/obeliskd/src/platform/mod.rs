//! Everything that differs per OS.
//!
//! [`PlatformImpl`] is the contract; each OS implements it on a unit struct, and exactly one of
//! those is exported as [`Platform`], chosen at compile time. Call through the type
//! (`Platform::name()`), never `&dyn`: the calls are static and other OSes' code isn't compiled.

/// What each OS has to provide.
pub trait PlatformImpl {
    /// The OS's name, e.g. `"linux"`.
    fn name() -> &'static str;
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
