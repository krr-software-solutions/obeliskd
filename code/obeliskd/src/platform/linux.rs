use std::path::PathBuf;

use super::PlatformImpl;

pub struct Linux;

impl PlatformImpl for Linux {
    fn name() -> &'static str {
        "linux"
    }

    fn state_dir() -> PathBuf {
        PathBuf::from("/var/lib/obeliskd")
    }
}
