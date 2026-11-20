//! Run manually on a logged-in macOS desktop. Never sends audio over the network.
#![cfg(target_os = "macos")]

use anyhow::{Context, Result, ensure};
use note_taker::audio::macos;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires macOS screen recording permission, a display, and system audio playback"]
async fn native_capture_delivers_decodable_audio_and_stops() -> Result<()> {
    let mut session = tokio::task::spawn_blocking(|| macos::start("default")).await??;
    let result = async {
        let packet = tokio::select! {
            packet = session.frames.recv() => packet.context("Capture ended without a frame")?,
            error = session.errors.recv() => anyhow::bail!("Capture error: {:?}", error),
            _ = tokio::time::sleep(Duration::from_secs(5)) => anyhow::bail!("No native audio arrived; play system audio and retry"),
        };
        let frame = packet.packet.decode()?;
        ensure!(frame.sample_rate == 48000 && frame.channels == 2, "Unexpected native format");
        ensure!(!frame.samples.is_empty(), "Empty native PCM");
        ensure!(frame.samples.iter().all(|v| v.is_finite()), "Non-finite native PCM");
        Ok(())
    }.await;
    tokio::task::spawn_blocking(move || session.capture.stop()).await??;
    result
}
