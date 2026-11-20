#[cfg(not(any(target_os = "macos", target_os = "windows")))]
compile_error!("stt currently implements macOS and Windows capture. Linux capture is planned.");

pub mod app;
pub mod audio;
pub mod cli;
pub mod config;
pub mod error;
pub mod output;
pub mod stt;
