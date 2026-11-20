pub mod converter;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod microphone;
pub mod mixer;
pub mod pipeline;
pub mod recording;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub use macos as backend;
#[cfg(target_os = "windows")]
pub use windows as backend;

use anyhow::Result;
use tokio::sync::{OwnedSemaphorePermit, mpsc};

#[derive(Debug, Clone)]
pub struct AudioSource {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

pub struct AudioFrame {
    pub sample_rate: u32,
    pub channels: usize,
    pub samples: Vec<f32>,
    /// Source clock timestamp, not wall-clock time.
    pub timestamp: f64,
}

/// Decoding runs on the processing worker, never in the native callback.
pub trait AudioPacket: Send {
    fn decode(self: Box<Self>) -> Result<AudioFrame>;
}

pub struct CapturedPacket {
    pub packet: Box<dyn AudioPacket>,
    pub permit: OwnedSemaphorePermit,
}

pub trait AudioCapture: Send {
    fn stop(&mut self) -> Result<()>;
}

pub struct CaptureSession {
    pub sample_rate: u32,
    pub capture: Box<dyn AudioCapture>,
    pub frames: mpsc::Receiver<CapturedPacket>,
    pub errors: mpsc::Receiver<anyhow::Error>,
}

pub fn devices(mode: crate::cli::AudioSourceMode) -> Result<Vec<AudioSource>> {
    let mut sources = Vec::new();
    if mode.system() {
        sources.extend(backend::devices()?);
    }
    if mode.microphone() {
        sources.extend(microphone::devices()?);
    }
    Ok(sources)
}
