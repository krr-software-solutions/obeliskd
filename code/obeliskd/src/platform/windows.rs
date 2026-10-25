// Windows isn't supported yet. This file is only compiled when targeting Windows (see `mod.rs`),
// so this fails exactly those builds. Delete the line below to start supporting it.
compile_error!("obeliskd doesn't support Windows yet");

use super::PlatformImpl;

pub struct Windows;

impl PlatformImpl for Windows {
    fn name() -> &'static str {
        "windows"
    }
}
