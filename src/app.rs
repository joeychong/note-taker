use crate::{
    audio::pipeline::Pipeline,
    config::Config,
    output::Output,
    stt::{
        elevenlabs::{Connection, ENDPOINT, retry_delay},
        protocol::{Event, Failure},
        transcripts::TranscriptState,
    },
};
use anyhow::{Context, Result, anyhow};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[cfg(test)]
mod tests;

pub async fn listen(config: Config, stop: CancellationToken) -> Result<()> {
    let filter =
        EnvFilter::try_new(std::env::var("STT_LOG_LEVEL").unwrap_or_else(|_| "info".into()))
            .context("Invalid STT_LOG_LEVEL")?;
    let mut output = Output::start(&config.args).await?;
    let logging = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(output.log_writer())
        .try_init()
        .map_err(|e| anyhow!("Cannot initialize logging: {e}"));
    if let Err(error) = logging {
        output.shutdown().await?;
        return Err(error);
    }
    tracing::info!(
        "Platform: {}; source: {}",
        std::env::consts::OS,
        config.args.device
    );
    tracing::info!("Audio is transmitted to ElevenLabs. Press Ctrl+C to stop.");
    tracing::info!(audio_source = ?config.args.audio_source, diarize = config.args.diarize, "Capture options");
    if let Some(path) = &config.args.audio_output {
        tracing::info!(path = %path.display(), "Recording playback audio as 16 kHz mono WAV; capture pauses during reconnects");
    }
    if config.args.audio_source == crate::cli::AudioSourceMode::Both {
        tracing::info!(
            "Use headphones when capturing system audio and microphone together to avoid acoustic echo."
        );
    }
    let result = run(&config, &mut output, &stop).await;
    if let Err(error) = &result {
        tracing::error!("{error:#}");
    }
    let saved = output.flushed_sequence.clone();
    let flush_result = output.shutdown().await;
    if config.args.audio_output.is_some() && flush_result.is_ok() {
        eprintln!("Audio recording flushed and WAV header finalized.");
    }
    if config.args.output.is_some() {
        eprintln!(
            "Last transcript file sequence flushed: {}",
            saved.load(std::sync::atomic::Ordering::Acquire)
        );
    }
    // Preserve both errors if transcription and flushing failed.
    match (result, flush_result) {
        (Err(a), Err(b)) => Err(anyhow!("{a:#}; additionally output cleanup failed: {b:#}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

async fn run(config: &Config, output: &mut Output, stop: &CancellationToken) -> Result<()> {
    run_at(config, output, stop, ENDPOINT, retry_delay).await
}

async fn run_at(
    config: &Config,
    output: &mut Output,
    stop: &CancellationToken,
    endpoint: &str,
    delay_for: fn(u32) -> Duration,
) -> Result<()> {
    let mut attempts = 0;
    loop {
        output.check()?;
        if stop.is_cancelled() {
            return Ok(());
        }
        tracing::info!("Connecting to ElevenLabs...");
        let connected = tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = output.errors.changed() => { output.check()?; continue; }
            result = Connection::connect(endpoint, &config.api_key, &config.args) => result,
        };
        let live_started = Instant::now();
        let failure = match connected {
            Ok(mut connection) => match session(config, output, stop, &mut connection).await? {
                Some(failure) => failure,
                None => return Ok(()),
            },
            Err(failure) => failure,
        };
        output.check()?;
        if !failure.retryable {
            return Err(failure.into());
        }
        if live_started.elapsed() >= Duration::from_secs(60) {
            attempts = 0;
        }
        if attempts >= 5 {
            anyhow::bail!("Reconnect attempts exhausted: {failure}");
        }
        let delay = delay_for(attempts);
        attempts += 1;
        tracing::warn!(
            attempt = attempts,
            ?delay,
            "{failure}. Capture paused; outage audio and uncommitted speech may be lost. Reconnecting without replay."
        );
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = output.errors.changed() => { output.check()?; }
            _ = tokio::time::sleep(delay) => (),
        }
    }
}

async fn await_started(
    connection: &mut Connection,
    output: &mut Output,
    stop: &CancellationToken,
    state: &mut TranscriptState,
) -> Result<Option<Failure>> {
    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => return Ok(None),
            _ = output.errors.changed() => { output.check()?; }
            event = connection.events.recv() => match event {
                Some(Event::Started { session_id, config }) => {
                    if config.get("model_id").and_then(|v| v.as_str()) != Some("scribe_v3_live") {
                        anyhow::bail!("Server did not confirm scribe_v3_live; refusing to stream audio");
                    }
                    tracing::info!(server_session = session_id, "Connected. Starting audio capture...");
                    state.start_session(session_id);
                    return Ok(None);
                }
                Some(Event::Warning { warning }) => tracing::warn!("ElevenLabs: {warning}"),
                Some(Event::Other) => tracing::debug!("Ignoring unrequested server metadata"),
                Some(_) => anyhow::bail!("Transcript arrived before session_started"),
                None => return Ok(Some(connection.failures.borrow().clone().unwrap_or_else(|| Failure::transient("Disconnected before session_started")))),
            },
            _ = connection.failures.changed() => {
                if let Some(failure) = connection.failures.borrow().clone() { return Ok(Some(failure)); }
            }
            _ = &mut deadline => return Ok(Some(Failure::transient("Timed out waiting for session_started"))),
        }
    }
}

fn event(event: Event, output: &mut Output, state: &mut TranscriptState) -> Result<()> {
    state.handle(event, output)
}

async fn session(
    config: &Config,
    output: &mut Output,
    stop: &CancellationToken,
    connection: &mut Connection,
) -> Result<Option<Failure>> {
    let mut pending = TranscriptState::new(config.args.diarize);
    if let Some(failure) = await_started(connection, output, stop, &mut pending).await? {
        return Ok(Some(failure));
    }
    if stop.is_cancelled() {
        let _ = connection.close().await;
        return Ok(None);
    }
    let recorder = output.audio_recorder();
    let mut pipeline = tokio::select! {
        _ = stop.cancelled() => { let _ = connection.close().await; return Ok(None); }
        _ = output.errors.changed() => { output.check()?; anyhow::bail!("Output worker stopped during capture startup"); }
        result = Pipeline::start(&config.args, connection.audio_sender(), recorder) => result?,
    };
    if config.args.timestamps {
        output.start_timestamps();
    }
    tracing::info!("Audio capture started. Listening...");
    let mut result: Result<Option<Failure>> = async {
        loop {
            tokio::select! {
                biased;
                _ = output.errors.changed() => { output.check()?; }
                _ = stop.cancelled() => return Ok(None),
                message = connection.events.recv() => match message {
                    Some(message) => event(message, output, &mut pending)?,
                    None => return Ok(Some(connection.failures.borrow().clone().unwrap_or_else(|| Failure::transient("WebSocket receiver stopped")))),
                },
                _ = connection.failures.changed() => {
                    if let Some(failure) = connection.failures.borrow().clone() { return Ok(Some(failure)); }
                }
                _ = pipeline.errors.changed() => {
                    if let Some(failure) = connection.failures.borrow().clone() { return Ok(Some(failure)); }
                    if let Some(error) = pipeline.errors.borrow().as_ref() { anyhow::bail!("Audio capture failed: {error}"); }
                    anyhow::bail!("Audio processing worker stopped unexpectedly");
                }
            }
        }
    }.await;
    let graceful = matches!(&result, Ok(None));
    let stop_result = pipeline.stop(graceful).await;
    // Receive and preserve finals already accepted before a transport failure.
    while let Ok(message) = connection.events.try_recv() {
        if let Err(error) = event(message, output, &mut pending) {
            result = Err(error);
            break;
        }
    }
    if let Err(error) = stop_result {
        result = Err(error);
    }
    match result {
        Ok(None) => {
            let finalized = finalize(connection, output, &mut pending).await;
            let annotations = pending.flush_unlabeled(output);
            let closed = connection.close().await;
            finalized?;
            annotations?;
            closed?;
            tracing::info!("Stopped; finalized transcripts drained.");
            Ok(None)
        }
        result => {
            pending.flush_unlabeled(output)?;
            if pending.pending_speech {
                tracing::warn!("Pending partial was not committed and will not be saved");
            }
            output.partial(String::new())?;
            result
        }
    }
}

async fn finalize(
    connection: &mut Connection,
    output: &mut Output,
    pending: &mut TranscriptState,
) -> Result<()> {
    finalize_with_wait(connection, output, pending, Duration::from_secs(10)).await
}

async fn finalize_with_wait(
    connection: &mut Connection,
    output: &mut Output,
    pending: &mut TranscriptState,
    wait: Duration,
) -> Result<()> {
    // Keep receiving while the writer drains queued audio and sends the commit.
    let sender_commit = connection.commit();
    tokio::pin!(sender_commit);
    let mut committed = false;
    let deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            _ = output.errors.changed() => { output.check()?; }
            message = connection.events.recv() => match message {
                Some(message) => event(message, output, pending)?,
                None => anyhow::bail!("Connection closed during finalization; pending speech may be missing"),
            },
            result = &mut sender_commit, if !committed => {
                result?;
                committed = true;
                deadline.as_mut().reset(Instant::now() + wait);
            }
            _ = connection.failures.changed() => {
                if let Some(failure) = connection.failures.borrow().as_ref() {
                    anyhow::bail!("Finalization failed: {failure}; pending speech may be missing");
                }
            }
            _ = &mut deadline => {
                if !committed || pending.pending_speech { anyhow::bail!("Finalization timed out with unconfirmed speech; partial text was not saved"); }
                if pending.has_pending_labels() {
                    pending.flush_unlabeled(output)?;
                    anyhow::bail!("Speaker finalization timed out; committed text was preserved with unknown speaker labels");
                }
                return Ok(());
            }
        }
    }
}
