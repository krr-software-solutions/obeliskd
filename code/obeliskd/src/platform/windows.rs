// Windows isn't supported yet. This file is only compiled when targeting Windows (see `mod.rs`),
// so this fails exactly those builds. Delete the line below to start supporting it.
compile_error!("obeliskd doesn't support Windows yet");

use std::path::PathBuf;

use super::PlatformImpl;

pub struct Windows;

impl PlatformImpl for Windows {
    fn name() -> &'static str {
        "windows"
    }

    fn state_dir() -> PathBuf {
        let program_data =
            std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
        PathBuf::from(program_data).join("obeliskd")
    }
}
