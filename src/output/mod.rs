pub mod file;
pub mod terminal;

use crate::{
    cli::{ListenArgs, OutputFormat},
    error::PipelineError,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    io::AsyncWrite,
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const MAX_TEXT_BYTES: usize = 64 * 1024;
pub const OUTPUT_CAPACITY: usize = 128;

#[derive(Debug, Clone, Serialize)]
pub struct Transcript {
    /// UTC time the committed result was received, not source audio time.
    pub timestamp: String,
    pub session_id: Uuid,
    pub sequence: u64,
    pub text: String,
    /// Recognition emission time since capture first started, including reconnect gaps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speakers: Option<Vec<SpeakerSpan>>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SpeakerSpan {
    pub speaker_id: Option<String>,
    pub speaker_label: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct SpeakerMerge {
    pub event: &'static str,
    pub timestamp: String,
    pub session_id: Uuid,
    pub sequence: u64,
    pub speaker_session_id: String,
    pub from_speaker_id: String,
    pub to_speaker_id: String,
    pub from_label: String,
    pub to_label: String,
}

pub enum FileRecord {
    Transcript(Arc<Transcript>),
    SpeakerMerge(Arc<SpeakerMerge>),
}
impl FileRecord {
    pub fn sequence(&self) -> u64 {
        match self {
            Self::Transcript(r) => r.sequence,
            Self::SpeakerMerge(r) => r.sequence,
        }
    }
}

/// Speaker turns become separate lines; original text remains intact in JSONL.
pub fn transcript_text(record: &Transcript) -> String {
    let prefix = record.elapsed_ms.map(time_prefix).unwrap_or_default();
    match &record.speakers {
        Some(spans) => spans
            .iter()
            .filter(|span| !span.text.trim().is_empty())
            .map(|span| {
                format!(
                    "{prefix}{}: {}",
                    span.speaker_label,
                    span.text.trim().replace(['\r', '\n'], " ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => format!("{prefix}{}", record.text.replace(['\r', '\n'], " ")),
    }
}

fn time_prefix(elapsed_ms: u64) -> String {
    let seconds = elapsed_ms / 1000;
    format!(
        "[{:02}:{:02}:{:02}] ",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[derive(Debug)]
pub enum TerminalEvent {
    Final(Arc<Transcript>),
    Log(String),
}

pub(crate) fn report(errors: &watch::Sender<Option<String>>, message: String) {
    errors.send_if_modified(|current| {
        if current.is_none() {
            *current = Some(message);
            true
        } else {
            false
        }
    });
}

pub struct Output {
    recording: Option<crate::audio::recording::RecordingSender>,
    terminal: mpsc::Sender<TerminalEvent>,
    file: Option<mpsc::Sender<FileRecord>>,
    partial: watch::Sender<String>,
    pub errors: watch::Receiver<Option<String>>,
    error_tx: watch::Sender<Option<String>>,
    stop: CancellationToken,
    tasks: Vec<JoinHandle<Result<()>>>,
    pub flushed_sequence: Arc<AtomicU64>,
    session: Uuid,
    sequence: u64,
    next_speaker: u64,
    capture_started: Option<tokio::time::Instant>,
}

impl Output {
    pub async fn start(args: &ListenArgs) -> Result<Self> {
        crate::audio::recording::validate_paths(args)?;
        let writer = match &args.output {
            Some(path) => Some(file::open(path, args.overwrite).await?),
            None => None,
        };
        let interactive = !args.no_partial
            && io::stdout().is_terminal()
            && io::stderr().is_terminal()
            && std::env::var("TERM").as_deref() != Ok("dumb");
        let mut output = Self::with_writers(
            tokio::io::stdout(),
            tokio::io::stderr(),
            writer.map(|w| (w, args.format)),
            interactive,
        );
        if let Some(path) = &args.audio_output {
            let started =
                crate::audio::recording::start(path, args.overwrite_audio, output.error_tx.clone())
                    .await;
            match started {
                Ok((sender, task)) => {
                    output.recording = Some(sender);
                    output.tasks.push(task);
                }
                Err(error) => {
                    let _ = output.shutdown().await;
                    return Err(error);
                }
            }
        }
        Ok(output)
    }

    /// Injectable writers allow testing real fan-out without global stdout or an API key.
    pub fn with_writers<O, E, F>(
        stdout: O,
        stderr: E,
        file: Option<(F, OutputFormat)>,
        interactive: bool,
    ) -> Self
    where
        O: AsyncWrite + Unpin + Send + 'static,
        E: AsyncWrite + Unpin + Send + 'static,
        F: AsyncWrite + Unpin + Send + 'static,
    {
        let (terminal, terminal_rx) = mpsc::channel(OUTPUT_CAPACITY);
        let (partial, partial_rx) = watch::channel(String::new());
        let (error_tx, errors) = watch::channel(None);
        let stop = CancellationToken::new();
        let flushed_sequence = Arc::new(AtomicU64::new(0));
        let task_errors = error_tx.clone();
        let terminal_stop = stop.clone();
        let mut tasks = vec![tokio::spawn(async move {
            let result = terminal::run(
                stdout,
                stderr,
                terminal_rx,
                partial_rx,
                terminal_stop,
                interactive,
            )
            .await;
            if let Err(e) = &result {
                report(&task_errors, format!("Terminal output failed: {e}"));
            }
            result
        })];
        let file = file.map(|(writer, format)| {
            let (tx, rx) = mpsc::channel(OUTPUT_CAPACITY);
            let task_errors = error_tx.clone();
            let file_stop = stop.clone();
            let flushed = flushed_sequence.clone();
            tasks.push(tokio::spawn(async move {
                let result = file::run(writer, format, rx, file_stop, flushed).await;
                if let Err(e) = &result {
                    report(&task_errors, format!("Transcript file failed: {e}"));
                }
                result
            }));
            tx
        });
        Self {
            recording: None,
            terminal,
            file,
            partial,
            errors,
            error_tx,
            stop,
            tasks,
            flushed_sequence,
            session: Uuid::new_v4(),
            sequence: 0,
            next_speaker: 1,
            capture_started: None,
        }
    }

    /// Start once, retaining elapsed time across network reconnects.
    pub fn start_timestamps(&mut self) {
        self.capture_started
            .get_or_insert_with(tokio::time::Instant::now);
    }

    fn elapsed_ms(&self) -> Option<u64> {
        self.capture_started
            .map(|start| u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    pub fn partial(&self, text: String) -> Result<()> {
        ensure!(
            text.len() <= MAX_TEXT_BYTES,
            "Partial transcript exceeds 64 KiB"
        );
        let text = match self.elapsed_ms() {
            Some(elapsed) if !text.is_empty() => format!("{}{text}", time_prefix(elapsed)),
            _ => text,
        };
        self.partial.send_replace(text);
        Ok(())
    }

    pub fn committed(&mut self, text: String) -> Result<()> {
        self.committed_with_speakers(text, None, None, None)
    }

    pub fn committed_with_speakers(
        &mut self,
        text: String,
        speaker_session_id: Option<String>,
        segment_index: Option<u64>,
        speakers: Option<Vec<SpeakerSpan>>,
    ) -> Result<()> {
        ensure!(
            text.len() <= MAX_TEXT_BYTES,
            "Committed transcript exceeds 64 KiB"
        );
        self.partial.send_replace(String::new());
        if text.trim().is_empty() {
            return Ok(());
        }
        self.sequence += 1;
        let record = Arc::new(Transcript {
            timestamp: OffsetDateTime::now_utc().format(&Rfc3339)?,
            session_id: self.session,
            sequence: self.sequence,
            text,
            elapsed_ms: self.elapsed_ms(),
            speaker_session_id,
            segment_index,
            speakers,
        });
        ensure!(
            serde_json::to_vec(record.as_ref())?.len() <= MAX_TEXT_BYTES,
            "Final transcript metadata exceeds 64 KiB"
        );
        // Independent nonblocking fan-out: a failed sink cannot block the other sink.
        let file_result = self
            .file
            .as_ref()
            .map(|file| file.try_send(FileRecord::Transcript(record.clone())));
        let terminal_result = self.terminal.try_send(TerminalEvent::Final(record));
        if let Some(Err(e)) = file_result {
            return Err(queue_error(e, "file").into());
        }
        terminal_result.map_err(|e| queue_error(e, "terminal"))?;
        Ok(())
    }

    pub fn allocate_speaker_label(&mut self) -> String {
        let label = format!("Speaker {}", self.next_speaker);
        self.next_speaker += 1;
        label
    }

    pub fn speaker_merge(
        &mut self,
        server_session: &str,
        from: &str,
        to: &str,
        from_label: &str,
        to_label: &str,
    ) -> Result<()> {
        self.sequence += 1;
        let record = SpeakerMerge {
            event: "speaker_merge",
            timestamp: OffsetDateTime::now_utc().format(&Rfc3339)?,
            session_id: self.session,
            sequence: self.sequence,
            speaker_session_id: server_session.into(),
            from_speaker_id: from.into(),
            to_speaker_id: to.into(),
            from_label: from_label.into(),
            to_label: to_label.into(),
        };
        if let Some(file) = &self.file {
            file.try_send(FileRecord::SpeakerMerge(Arc::new(record)))
                .map_err(|e| queue_error(e, "file"))?;
        }
        tracing::info!(
            "Speaker label correction: {from_label} is {to_label}. Earlier printed/TXT lines keep their original labels; JSONL records this correction."
        );
        Ok(())
    }

    pub fn log_writer(&self) -> LogWriter {
        LogWriter {
            terminal: self.terminal.clone(),
            errors: self.error_tx.clone(),
        }
    }

    pub fn check(&self) -> Result<()> {
        if let Some(error) = self.errors.borrow().as_ref() {
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    pub fn audio_recorder(&self) -> Option<crate::audio::recording::RecordingSender> {
        self.recording.clone()
    }

    pub async fn shutdown(mut self) -> Result<()> {
        // Pipeline workers have stopped; closing the last sender drains PCM and fixes the WAV header.
        self.recording.take();
        self.partial.send_replace(String::new());
        self.stop.cancel();
        let drain = async {
            let results = futures_util::future::join_all(self.tasks.iter_mut()).await;
            let mut failures = Vec::new();
            for result in results {
                if let Err(error) = result.context("Output worker panicked").and_then(|r| r) {
                    failures.push(format!("{error:#}"));
                }
            }
            ensure!(
                failures.is_empty(),
                "Output cleanup failed: {}",
                failures.join("; ")
            );
            self.check()
        };
        let result = tokio::time::timeout(Duration::from_secs(5), drain).await;
        for task in &self.tasks {
            task.abort();
        }
        match result {
            Ok(result) => result,
            Err(_) => anyhow::bail!(
                "Output drain timed out; last file sequence flushed: {}. Output may be incomplete",
                self.flushed_sequence.load(Ordering::Acquire)
            ),
        }
    }
}

fn queue_error<T>(error: mpsc::error::TrySendError<T>, name: &'static str) -> PipelineError {
    match error {
        mpsc::error::TrySendError::Full(_) => PipelineError::Backpressure(name),
        mpsc::error::TrySendError::Closed(_) => PipelineError::OutputClosed(name),
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct LogWriter {
    terminal: mpsc::Sender<TerminalEvent>,
    errors: watch::Sender<Option<String>>,
}

pub struct LogBuffer {
    owner: LogWriter,
    bytes: Vec<u8>,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = LogBuffer;
    fn make_writer(&'a self) -> Self::Writer {
        LogBuffer {
            owner: self.clone(),
            bytes: Vec::new(),
        }
    }
}

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len() + bytes.len() > MAX_TEXT_BYTES {
            return Err(io::Error::other("Log record exceeds 64 KiB"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogBuffer {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.bytes).into_owned();
        if self
            .owner
            .terminal
            .try_send(TerminalEvent::Log(text))
            .is_err()
        {
            report(
                &self.owner.errors,
                "Terminal diagnostic queue stalled; stopping capture".into(),
            );
        }
    }
}
