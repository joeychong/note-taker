use super::{
    AudioCapture, AudioFrame, CaptureSession, backend, converter::Converter,
    microphone::PreparedMicrophone, mixer::Mixer,
};
use crate::cli::ListenArgs;
use crate::stt::elevenlabs::AudioSender;
use anyhow::{Context, Result, ensure};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

/// Places captured packets and synthesized silence on the same source-clock timeline.
pub struct Timeline {
    cursor: Option<f64>,
    origin: f64,
    converter: Converter,
    rate: u32,
}

impl Timeline {
    pub fn new(chunk_ms: u32, origin: f64) -> Result<Self> {
        Self::at_rate(48000, chunk_ms, origin, false)
    }

    pub fn at_rate(rate: u32, chunk_ms: u32, origin: f64, align_start: bool) -> Result<Self> {
        Ok(Self {
            cursor: align_start.then_some(origin),
            origin,
            converter: Converter::new(rate, chunk_ms)?,
            rate,
        })
    }

    fn silence_until(&mut self, timestamp: f64) -> Result<Vec<Vec<u8>>> {
        let cursor = self.cursor.unwrap_or(self.origin);
        let difference = timestamp - cursor;
        if difference <= 0.0 {
            return Ok(Vec::new());
        }
        ensure!(
            difference <= 1.0,
            "Audio clock jumped or processing stalled for {difference:.3}s; audio may have been lost"
        );
        let count = (difference * f64::from(self.rate)).round() as usize;
        if count == 0 {
            return Ok(Vec::new());
        }
        let chunks = self.converter.push(&vec![0.0; count], 1)?;
        self.cursor = Some(cursor + count as f64 / f64::from(self.rate));
        Ok(chunks)
    }

    pub fn push(&mut self, mut frame: AudioFrame) -> Result<Vec<Vec<u8>>> {
        ensure!(
            frame.sample_rate == self.rate,
            "Capture sample rate changed; restart capture"
        );
        ensure!(
            (1..=32).contains(&frame.channels)
                && frame.samples.len().is_multiple_of(frame.channels),
            "Invalid source channel layout"
        );
        ensure!(
            frame.timestamp.is_finite(),
            "Invalid native audio timestamp"
        );
        if frame.timestamp < self.origin && self.cursor == Some(self.origin) {
            let skip = (((self.origin - frame.timestamp) * f64::from(self.rate)).ceil() as usize)
                .min(frame.samples.len() / frame.channels);
            if skip > 0 {
                tracing::warn!(
                    frames = skip,
                    "Excluding capture startup audio before both inputs were ready"
                );
                frame.samples.drain(..skip * frame.channels);
                frame.timestamp += skip as f64 / f64::from(self.rate);
                if frame.samples.is_empty() {
                    return Ok(Vec::new());
                }
            }
        }
        let mut chunks = Vec::new();
        if let Some(cursor) = self.cursor {
            let gap = frame.timestamp - cursor;
            ensure!(
                gap >= -0.005,
                "Native audio arrived {:.3}s after its timeline was already sent; stopping to avoid losing or replaying speech",
                -gap
            );
            if gap > 0.005 {
                tracing::warn!(
                    seconds = gap,
                    "Native audio timestamp gap filled with silence; audio may be missing"
                );
                chunks.extend(self.silence_until(frame.timestamp)?);
            }
        }
        chunks.extend(self.converter.push(&frame.samples, frame.channels)?);
        self.cursor = Some(
            frame.timestamp + (frame.samples.len() / frame.channels) as f64 / f64::from(self.rate),
        );
        Ok(chunks)
    }

    pub fn idle(&mut self, now: f64) -> Result<Vec<Vec<u8>>> {
        // Leave 300 ms for native delivery before synthesizing omitted silence.
        self.silence_until(now - 0.3)
    }

    pub fn finish(self) -> Result<Vec<Vec<u8>>> {
        self.converter.finish()
    }
}

pub struct Pipeline {
    captures: Vec<Box<dyn AudioCapture>>,
    workers: Vec<JoinHandle<Result<()>>>,
    stopping: Arc<AtomicBool>,
    pub errors: watch::Receiver<Option<String>>,
}

impl Pipeline {
    pub async fn start(
        args: &ListenArgs,
        sender: AudioSender,
        recorder: Option<super::recording::RecordingSender>,
    ) -> Result<Self> {
        let chunk_ms = args.chunk_ms;
        let mode = args.audio_source;
        let microphone_device = args.microphone_device.clone();
        let prepared = if mode.microphone() {
            Some(
                tokio::task::spawn_blocking(move || {
                    PreparedMicrophone::prepare(&microphone_device)
                })
                .await
                .context("Microphone preparation worker panicked")??,
            )
        } else {
            None
        };
        let mut sessions = Vec::new();
        if mode.system() {
            let device = args.device.clone();
            sessions.push(
                tokio::task::spawn_blocking(move || backend::start(&device))
                    .await
                    .context("System capture startup worker panicked")??,
            );
        }
        if let Some(prepared) = prepared {
            sessions.push(
                tokio::task::spawn_blocking(move || prepared.start())
                    .await
                    .context("Microphone startup worker panicked")??,
            );
        }
        let (error_tx, error_rx) = watch::channel(None);
        let stopping = Arc::new(AtomicBool::new(false));
        let origin = backend::host_seconds();
        let align_start = sessions.len() == 2;
        let (mixed_tx, mut mixed_rx) = mpsc::channel((4000 / chunk_ms + 4) as usize);
        let mut mixer = Mixer::new(sessions.len(), chunk_ms)?;
        let mut workers = Vec::new();
        let mixer_errors = error_tx.clone();
        workers.push(tokio::spawn(async move {
            let result = async {
                while let Some(message) = mixed_rx.recv().await {
                    match message {
                        MixedInput::Pcm(track, pcm) => mixer.push(track, &pcm)?,
                        MixedInput::Finished(track) => mixer.finish(track),
                    }
                    for chunk in mixer.chunks() {
                        if let Some(recorder) = &recorder {
                            recorder.send(&chunk)?;
                        }
                        sender.send(chunk)?;
                    }
                }
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                crate::output::report(&mixer_errors, format!("Audio mixing failed: {error:#}"));
            }
            result
        }));
        let mut captures = Vec::new();
        for (
            track,
            CaptureSession {
                capture,
                mut frames,
                mut errors,
                sample_rate,
            },
        ) in sessions.into_iter().enumerate()
        {
            captures.push(capture);
            let mixed_tx = mixed_tx.clone();
            let error_tx = error_tx.clone();
            let stopping = stopping.clone();
            workers.push(tokio::spawn(async move {
            let result = async {
                let mut timeline = Timeline::at_rate(sample_rate, chunk_ms, origin, align_start)?;
                let mut tick = tokio::time::interval(Duration::from_millis(100));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut errors_open = true;
                loop {
                    let chunks = tokio::select! {
                        biased;
                        error = errors.recv(), if errors_open => match error {
                            Some(error) => return Err(error),
                            None => { errors_open = false; continue; }
                        },
                        packet = frames.recv() => match packet {
                            Some(packet) => {
                                let decoded = packet.packet.decode()?;
                                drop(packet.permit);
                                timeline.push(decoded)?
                            }
                            None => {
                                ensure!(stopping.load(Ordering::Acquire), "Audio input {track} ended unexpectedly; restart capture");
                                break;
                            },
                        },
                        _ = tick.tick() => timeline.idle(backend::host_seconds())?,
                    };
                    for chunk in chunks {
                        mixed_tx.try_send(MixedInput::Pcm(track, chunk)).context("Audio mixing queue is full or closed; stopping to avoid silent loss")?;
                    }
                }
                for chunk in timeline.finish()? {
                    mixed_tx.try_send(MixedInput::Pcm(track, chunk)).context("Cannot drain final audio into mixer")?;
                }
                mixed_tx.try_send(MixedInput::Finished(track)).context("Cannot finalize audio mixer track")?;
                Ok(())
            }
            .await;
            if let Err(e) = &result {
                crate::output::report(&error_tx, format!("Audio input {track} failed: {e:#}"));
            }
            result
            }));
        }
        drop(mixed_tx);
        Ok(Self {
            captures,
            workers,
            stopping,
            errors: error_rx,
        })
    }

    pub async fn stop(mut self, drain: bool) -> Result<()> {
        self.stopping.store(true, Ordering::Release);
        if !drain {
            for worker in &self.workers {
                worker.abort();
            }
        }
        let stops = self
            .captures
            .drain(..)
            .map(|mut capture| tokio::task::spawn_blocking(move || capture.stop()));
        let results = futures_util::future::join_all(stops).await;
        let mut failures = Vec::new();
        for result in results {
            if let Err(error) = result
                .context("Capture stop worker panicked")
                .and_then(|result| result)
            {
                failures.push(format!("{error:#}"));
            }
        }
        if drain {
            match tokio::time::timeout(
                Duration::from_secs(5),
                futures_util::future::join_all(self.workers.iter_mut()),
            )
            .await
            {
                Ok(results) => {
                    for result in results {
                        if let Err(error) = result
                            .context("Audio worker panicked")
                            .and_then(|result| result)
                        {
                            failures.push(format!("{error:#}"));
                        }
                    }
                }
                Err(_) => {
                    for worker in &self.workers {
                        worker.abort();
                    }
                    anyhow::bail!(
                        "Audio pipeline did not drain within five seconds; audio may be lost"
                    );
                }
            }
        }
        ensure!(
            failures.is_empty(),
            "Audio shutdown failed: {}",
            failures.join("; ")
        );
        Ok(())
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        for worker in &self.workers {
            worker.abort();
        }
    }
}

enum MixedInput {
    Pcm(usize, Vec<u8>),
    Finished(usize),
}
