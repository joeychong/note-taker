//! Manual hardware test only; it never uploads or writes audio.
#![cfg(target_os = "windows")]

use anyhow::{Context, Result, ensure};
use note_taker::audio::windows;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires Windows 10 version 1703+ or Windows 11, an output endpoint, and active system audio"]
async fn wasapi_loopback_delivers_pcm_and_stops() -> Result<()> {
    let sources = tokio::task::spawn_blocking(windows::devices).await??;
    ensure!(
        sources.iter().any(|source| source.id == "default"),
        "Configure a default Windows playback device"
    );
    let mut session = tokio::task::spawn_blocking(|| windows::start("default")).await??;
    let result = async {
        let packet = tokio::select! {
            packet = session.frames.recv() => packet.context("WASAPI ended without delivering PCM")?,
            error = session.errors.recv() => anyhow::bail!("WASAPI error: {error:?}"),
            _ = tokio::time::sleep(Duration::from_secs(5)) => anyhow::bail!("No loopback audio arrived; play audio through the default output and retry"),
        };
        let frame = packet.packet.decode()?;
        ensure!(frame.sample_rate == 48000 && frame.channels == 2, "Unexpected loopback format");
        ensure!(!frame.samples.is_empty() && frame.samples.iter().all(|v| v.is_finite()), "Invalid WASAPI PCM");
        ensure!(frame.timestamp > 0.0 && frame.timestamp <= windows::host_seconds(), "Invalid WASAPI source timestamp");
        Ok(())
    }.await;
    tokio::task::spawn_blocking(move || session.capture.stop()).await??;
    result
}
