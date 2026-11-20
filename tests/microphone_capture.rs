//! Opt-in native input smoke test; never uploads or saves captured audio.
use anyhow::{Context, Result, ensure};
use note_taker::audio::microphone::PreparedMicrophone;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires a connected microphone and microphone permission"]
async fn microphone_delivers_native_pcm_and_stops() -> Result<()> {
    let mut session =
        tokio::task::spawn_blocking(|| PreparedMicrophone::prepare("default")?.start()).await??;
    let result = async {
        let packet = tokio::select! {
            packet = session.frames.recv() => packet.context("Microphone ended without a packet")?,
            error = session.errors.recv() => anyhow::bail!("Microphone error: {error:?}"),
            _ = tokio::time::sleep(Duration::from_secs(5)) => anyhow::bail!("No microphone PCM arrived; check input and permission"),
        };
        let frame = packet.packet.decode()?;
        ensure!((8000..=192000).contains(&frame.sample_rate) && (1..=32).contains(&frame.channels), "Unsupported native microphone format");
        ensure!(!frame.samples.is_empty() && frame.samples.len().is_multiple_of(frame.channels), "Empty or misaligned microphone PCM");
        Ok(())
    }.await;
    tokio::task::spawn_blocking(move || session.capture.stop()).await??;
    result
}
