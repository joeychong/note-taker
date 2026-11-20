//! Bounded, asynchronous WAV recording of the normalized PCM sent to transcription.
use anyhow::{Context, Result, ensure};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{
    fs::OpenOptions,
    io::{AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufWriter},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch},
    task::JoinHandle,
};

const BYTE_BUDGET: usize = 64_000; // Two seconds at 16 kHz, mono PCM16.
const MAX_DATA_BYTES: u64 = (u32::MAX as u64 - 36) & !1;

struct Packet {
    pcm: Vec<u8>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub struct RecordingSender {
    tx: mpsc::Sender<Packet>,
    budget: Arc<Semaphore>,
}

impl RecordingSender {
    /// Called outside native callbacks. Never waits for filesystem I/O.
    pub fn send(&self, pcm: &[u8]) -> Result<()> {
        ensure!(pcm.len().is_multiple_of(2), "Misaligned recording PCM");
        if pcm.is_empty() {
            return Ok(());
        }
        let size = u32::try_from(pcm.len()).context("Recording chunk is too large")?;
        let permit = self.budget.clone().try_acquire_many_owned(size).context(
            "Audio recording exceeded its two-second buffer; stopping to avoid silent loss",
        )?;
        self.tx
            .try_send(Packet {
                pcm: pcm.to_vec(),
                _permit: permit,
            })
            .context("Audio recording queue is full or closed; stopping to avoid silent loss")?;
        Ok(())
    }
}

/// Check before either output is opened, including aliases to an existing file.
pub fn validate_paths(args: &crate::cli::ListenArgs) -> Result<()> {
    if let Some(audio) = &args.audio_output {
        ensure!(
            args.overwrite_audio
                || !audio
                    .try_exists()
                    .context("Cannot inspect audio output path")?,
            "Audio recording {} already exists; use a new filename or --overwrite-audio",
            audio.display()
        );
    }
    if let (Some(audio), Some(transcript)) = (&args.audio_output, &args.output) {
        let resolve = |path: &Path| -> Result<std::path::PathBuf> {
            if path.exists() {
                return path.canonicalize().context("Cannot resolve output path");
            }
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok(parent
                .canonicalize()
                .context("Cannot resolve output directory")?
                .join(path.file_name().context("Output path needs a filename")?))
        };
        ensure!(
            resolve(audio)? != resolve(transcript)?,
            "Audio and transcript output must be different files"
        );
        #[cfg(unix)]
        if let (Ok(a), Ok(b)) = (std::fs::metadata(audio), std::fs::metadata(transcript)) {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                a.dev() != b.dev() || a.ino() != b.ino(),
                "Audio and transcript output must be different files (hard links)"
            );
        }
    }
    Ok(())
}

pub async fn start(
    path: &Path,
    overwrite: bool,
    errors: watch::Sender<Option<String>>,
) -> Result<(RecordingSender, JoinHandle<Result<()>>)> {
    let mut writer = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create(overwrite)
            .create_new(!overwrite)
            .truncate(overwrite)
            .open(path)
            .await
            .with_context(|| {
                format!(
                    "Cannot create audio recording {} (existing files require --overwrite-audio)",
                    path.display()
                )
            })?,
    );
    writer
        .write_all(&header(0))
        .await
        .context("Cannot write WAV header")?;
    writer.flush().await?;
    let (tx, rx) = mpsc::channel(32);
    let path = path.to_owned();
    let task = tokio::spawn(async move {
        let result = write_audio(writer, rx).await;
        if let Err(error) = &result {
            crate::output::report(
                &errors,
                format!("Audio recording {} failed: {error:#}", path.display()),
            );
        }
        result
    });
    Ok((
        RecordingSender {
            tx,
            budget: Arc::new(Semaphore::new(BYTE_BUDGET)),
        },
        task,
    ))
}

fn header(bytes: u32) -> [u8; 44] {
    let mut header = [0; 44];
    header[..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(36 + bytes).to_le_bytes());
    header[8..16].copy_from_slice(b"WAVEfmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes()); // Integer PCM
    header[22..24].copy_from_slice(&1u16.to_le_bytes()); // Mono
    header[24..28].copy_from_slice(&16_000u32.to_le_bytes());
    header[28..32].copy_from_slice(&32_000u32.to_le_bytes());
    header[32..34].copy_from_slice(&2u16.to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&bytes.to_le_bytes());
    header
}

async fn checkpoint<W: AsyncWrite + AsyncSeek + Unpin>(writer: &mut W, bytes: u64) -> Result<()> {
    writer.seek(std::io::SeekFrom::Start(0)).await?;
    writer.write_all(&header(bytes as u32)).await?;
    writer.flush().await?;
    writer.seek(std::io::SeekFrom::Start(44 + bytes)).await?;
    Ok(())
}

async fn write_audio<W: AsyncWrite + AsyncSeek + Unpin>(
    mut writer: W,
    mut rx: mpsc::Receiver<Packet>,
) -> Result<()> {
    let mut bytes = 0u64;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Prioritize the timer so a continuous stream cannot starve header checkpoints.
    let result: Result<()> = async {
        loop {
            tokio::select! {
                biased;
                _ = tick.tick() => checkpoint(&mut writer, bytes).await.context("Cannot checkpoint WAV recording")?,
                packet = rx.recv() => match packet {
                    Some(packet) => {
                        ensure!(bytes + packet.pcm.len() as u64 <= MAX_DATA_BYTES,
                            "WAV reached its 4 GiB limit (about 37 hours); start a new recording");
                        writer.write_all(&packet.pcm).await.context("Cannot write recorded audio")?;
                        bytes += packet.pcm.len() as u64;
                    }
                    None => break,
                },
            }
        }
        Ok(())
    }.await;
    // Even after a source/size error, preserve a playable header for completed chunks.
    let flushed = checkpoint(&mut writer, bytes)
        .await
        .context("Cannot finalize WAV recording");
    match (result, flushed) {
        (Err(a), Err(b)) => Err(anyhow::anyhow!("{a:#}; additionally {b:#}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
    };

    struct BrokenDisk;
    impl AsyncWrite for BrokenDisk {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::other("disk full")))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncSeek for BrokenDisk {
        fn start_seek(self: Pin<&mut Self>, _: io::SeekFrom) -> io::Result<()> {
            Ok(())
        }
        fn poll_complete(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<u64>> {
            Poll::Ready(Ok(0))
        }
    }

    #[tokio::test]
    async fn recording_disk_errors_are_returned_instead_of_success() {
        let (tx, rx) = mpsc::channel(1);
        drop(tx);
        let error = write_audio(BrokenDisk, rx).await.unwrap_err();
        assert!(format!("{error:#}").contains("disk full"));
    }

    #[test]
    fn riff_limit_does_not_overflow_the_header() {
        let bytes = header(MAX_DATA_BYTES as u32);
        let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(u64::from(riff_size), MAX_DATA_BYTES + 36);
        assert_eq!(MAX_DATA_BYTES % 2, 0);
    }
}
