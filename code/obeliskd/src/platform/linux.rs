use super::PlatformImpl;

pub struct Linux;

impl PlatformImpl for Linux {
    fn name() -> &'static str {
        "linux"
    }
}
