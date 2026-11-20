use super::{FileRecord, Transcript, transcript_text};
use crate::cli::OutputFormat;
use anyhow::{Context, Result};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufWriter},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

pub async fn open(path: &Path, overwrite: bool) -> Result<BufWriter<File>> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .append(!overwrite)
        .truncate(overwrite)
        .open(path)
        .await
        .with_context(|| format!("Cannot open transcript file {}", path.display()))?;
    // Preserve record boundaries when appending to an existing file lacking a trailing newline.
    let length = file.metadata().await?.len();
    if !overwrite && length > 0 {
        file.seek(std::io::SeekFrom::End(-1)).await?;
        let mut last = [0];
        file.read_exact(&mut last).await?;
        if last[0] != b'\n' {
            file.write_all(b"\n").await?;
        }
    }
    Ok(BufWriter::new(file))
}

pub fn encode(record: &Transcript, format: OutputFormat) -> Result<Vec<u8>> {
    let mut bytes = match format {
        OutputFormat::Txt => transcript_text(record).into_bytes(),
        OutputFormat::Jsonl => serde_json::to_vec(record)?,
    };
    bytes.push(b'\n');
    Ok(bytes)
}

pub async fn run<W: AsyncWrite + Unpin>(
    mut writer: W,
    format: OutputFormat,
    mut rx: mpsc::Receiver<FileRecord>,
    stop: CancellationToken,
    flushed: Arc<AtomicU64>,
) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = 0;
    let mut stopping = false;
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled(), if !stopping => { stopping = true; rx.close(); }
            record = rx.recv() => match record {
                Some(record) => {
                    match &record {
                        FileRecord::Transcript(record) => writer.write_all(&encode(record, format)?).await?,
                        FileRecord::SpeakerMerge(record) if format == OutputFormat::Jsonl => {
                            let mut bytes = serde_json::to_vec(record.as_ref())?; bytes.push(b'\n'); writer.write_all(&bytes).await?;
                        }
                        FileRecord::SpeakerMerge(_) => (),
                    }
                    last = record.sequence();
                }
                None => break,
            },
            _ = tick.tick() => { writer.flush().await?; flushed.store(last, Ordering::Release); }
        }
    }
    writer.flush().await?;
    flushed.store(last, Ordering::Release);
    Ok(())
}
