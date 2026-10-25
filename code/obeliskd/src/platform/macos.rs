use std::path::PathBuf;

use super::PlatformImpl;

pub struct MacOs;

impl PlatformImpl for MacOs {
    fn name() -> &'static str {
        "macos"
    }

    fn state_dir() -> PathBuf {
        PathBuf::from("/Library/Application Support/obeliskd")
    }
}
