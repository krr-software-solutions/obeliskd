use super::PlatformImpl;

pub struct MacOs;

impl PlatformImpl for MacOs {
    fn name() -> &'static str {
        "macos"
    }
}
