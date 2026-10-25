use super::PlatformImpl;

pub struct Windows;

impl PlatformImpl for Windows {
    fn name() -> &'static str {
        "windows"
    }
}
