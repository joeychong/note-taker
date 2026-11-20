use note_taker::{
    cli::OutputFormat,
    output::{OUTPUT_CAPACITY, Output, Transcript, file},
};
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::io::AsyncWrite;
use uuid::Uuid;

#[derive(Clone, Default)]
struct Memory(Arc<Mutex<Vec<u8>>>);
impl Memory {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}
impl AsyncWrite for Memory {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.lock().unwrap().extend(bytes);
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn simultaneous_output_preserves_repeated_commits_and_omits_partials() {
    let stdout = Memory::default();
    let stderr = Memory::default();
    let disk = Memory::default();
    let mut output = Output::with_writers(
        stdout.clone(),
        stderr.clone(),
        Some((disk.clone(), OutputFormat::Jsonl)),
        false,
    );
    output.partial("unfinished draft".into()).unwrap();
    for text in ["你好 café مرحبا", "Yes.", "Yes."] {
        output.committed(text.into()).unwrap();
    }
    output.shutdown().await.unwrap();
    assert_eq!(stdout.text(), "你好 café مرحبا\nYes.\nYes.\n");
    assert!(!stdout.text().contains('\x1b'));
    assert!(stderr.text().is_empty());
    let records: Vec<serde_json::Value> = disk
        .text()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(records.len(), 3);
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record["sequence"], index + 1);
        assert_eq!(record["session_id"], records[0]["session_id"]);
        assert!(record["timestamp"].as_str().unwrap().ends_with('Z'));
        assert!(record.get("elapsed_ms").is_none());
    }
}

#[tokio::test(start_paused = true)]
async fn elapsed_timestamps_match_terminal_and_files_and_survive_restart() {
    use note_taker::output::SpeakerSpan;
    use std::time::Duration;

    for format in [OutputFormat::Txt, OutputFormat::Jsonl] {
        let stdout = Memory::default();
        let disk = Memory::default();
        let mut output = Output::with_writers(
            stdout.clone(),
            Memory::default(),
            Some((disk.clone(), format)),
            false,
        );
        output.start_timestamps();
        output.committed("Starting".into()).unwrap();
        tokio::time::advance(Duration::from_secs(83)).await;
        // A reconnect starts capture again without resetting the process clock.
        output.start_timestamps();
        output
            .committed_with_speakers(
                "你好. Yes.".into(),
                Some("session".into()),
                Some(0),
                Some(vec![
                    SpeakerSpan {
                        speaker_id: Some("speaker_0".into()),
                        speaker_label: "Speaker 1".into(),
                        text: "你好.".into(),
                    },
                    SpeakerSpan {
                        speaker_id: Some("speaker_1".into()),
                        speaker_label: "Speaker 2".into(),
                        text: " Yes.".into(),
                    },
                ]),
            )
            .unwrap();
        tokio::time::advance(Duration::from_secs(3640)).await;
        output.committed("After an hour".into()).unwrap();
        output.shutdown().await.unwrap();
        assert_eq!(
            stdout.text(),
            "[00:00:00] Starting\n[00:01:23] Speaker 1: 你好.\n[00:01:23] Speaker 2: Yes.\n[01:02:03] After an hour\n"
        );
        match format {
            OutputFormat::Txt => assert_eq!(disk.text(), stdout.text()),
            OutputFormat::Jsonl => {
                let records: Vec<serde_json::Value> = disk
                    .text()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(records.len(), 3);
                for (record, elapsed) in records.iter().zip([0, 83_000, 3_723_000]) {
                    assert_eq!(record["elapsed_ms"], elapsed);
                }
                assert_eq!(records[1]["text"], "你好. Yes.");
                assert_eq!(records[1]["speakers"][0]["speaker_label"], "Speaker 1");
            }
        }
    }
}

#[tokio::test]
async fn appends_and_overwrites_with_record_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.txt");
    tokio::fs::write(&path, "existing").await.unwrap();
    for (overwrite, text, expected) in [
        (false, "new", "existing\nnew\n"),
        (true, "reset", "reset\n"),
    ] {
        let writer = file::open(&path, overwrite).await.unwrap();
        let mut output = Output::with_writers(
            Memory::default(),
            Memory::default(),
            Some((writer, OutputFormat::Txt)),
            false,
        );
        output.committed(text.into()).unwrap();
        output.shutdown().await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), expected);
    }
    assert!(
        file::open(&dir.path().join("missing/child.txt"), false)
            .await
            .is_err()
    );
}

struct Broken;
impl AsyncWrite for Broken {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other("disk full")))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn file_failure_does_not_block_terminal_and_is_reported() {
    let stdout = Memory::default();
    let mut output = Output::with_writers(
        stdout.clone(),
        Memory::default(),
        Some((Broken, OutputFormat::Txt)),
        false,
    );
    output.committed("still visible".into()).unwrap();
    output.errors.changed().await.unwrap();
    assert!(
        output
            .check()
            .unwrap_err()
            .to_string()
            .contains("disk full")
    );
    assert!(output.shutdown().await.is_err());
    assert_eq!(stdout.text(), "still visible\n");
}

#[tokio::test]
async fn saturation_is_an_error_instead_of_silent_loss() {
    // No await means workers cannot drain on this current-thread runtime.
    let mut output = Output::with_writers(
        Memory::default(),
        Memory::default(),
        Some((Memory::default(), OutputFormat::Txt)),
        false,
    );
    for _ in 0..OUTPUT_CAPACITY {
        output.committed("hello".into()).unwrap();
    }
    assert!(
        output
            .committed("overflow".into())
            .unwrap_err()
            .to_string()
            .contains("full")
    );
    output.shutdown().await.unwrap();
}

#[test]
fn jsonl_escapes_newlines_and_txt_keeps_one_line_per_commit() {
    let record = Transcript {
        speaker_session_id: None,
        segment_index: None,
        speakers: None,
        elapsed_ms: None,
        timestamp: "2026-11-19T09:00:01Z".into(),
        session_id: Uuid::new_v4(),
        sequence: 1,
        text: "a\nb\"c".into(),
    };
    let json = file::encode(&record, OutputFormat::Jsonl).unwrap();
    assert_eq!(json.iter().filter(|&&b| b == b'\n').count(), 1);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&json).unwrap()["text"],
        record.text
    );
    assert_eq!(
        file::encode(&record, OutputFormat::Txt).unwrap(),
        b"a b\"c\n"
    );
}

#[tokio::test]
async fn broken_terminal_still_drains_and_flushes_the_healthy_file() {
    let disk = Memory::default();
    let mut output = Output::with_writers(
        Broken,
        Memory::default(),
        Some((disk.clone(), OutputFormat::Txt)),
        false,
    );
    for text in ["one", "two", "three"] {
        output.committed(text.into()).unwrap();
    }
    let flushed = output.flushed_sequence.clone();
    assert!(output.shutdown().await.is_err());
    assert_eq!(disk.text(), "one\ntwo\nthree\n");
    assert_eq!(flushed.load(std::sync::atomic::Ordering::Acquire), 3);
}

struct Stalled;
impl AsyncWrite for Stalled {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Pending
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_times_out_stalled_io_without_blocking_the_other_sink() {
    let disk = Memory::default();
    let mut output = Output::with_writers(
        Stalled,
        Memory::default(),
        Some((disk.clone(), OutputFormat::Txt)),
        false,
    );
    output
        .committed("saved despite blocked pipe".into())
        .unwrap();
    let start = tokio::time::Instant::now();
    assert!(
        output
            .shutdown()
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
    assert_eq!(start.elapsed(), std::time::Duration::from_secs(5));
    assert_eq!(disk.text(), "saved despite blocked pipe\n");
}

#[tokio::test]
async fn txt_and_terminal_share_speaker_turns_and_do_not_repeat_merge_metadata() {
    use note_taker::output::SpeakerSpan;
    let stdout = Memory::default();
    let disk = Memory::default();
    let mut output = Output::with_writers(
        stdout.clone(),
        Memory::default(),
        Some((disk.clone(), OutputFormat::Txt)),
        false,
    );
    output
        .committed_with_speakers(
            "Hello world".into(),
            Some("server".into()),
            Some(0),
            Some(vec![
                SpeakerSpan {
                    speaker_id: Some("speaker_0".into()),
                    speaker_label: "Speaker 1".into(),
                    text: "Hello".into(),
                },
                SpeakerSpan {
                    speaker_id: Some("speaker_1".into()),
                    speaker_label: "Speaker 2".into(),
                    text: " world".into(),
                },
            ]),
        )
        .unwrap();
    output
        .speaker_merge("server", "speaker_1", "speaker_0", "Speaker 2", "Speaker 1")
        .unwrap();
    output.shutdown().await.unwrap();
    assert_eq!(stdout.text(), "Speaker 1: Hello\nSpeaker 2: world\n");
    assert_eq!(disk.text(), stdout.text());
}

#[tokio::test]
async fn shutdown_drains_both_audio_recording_and_transcript_file() {
    use clap::Parser;
    use note_taker::cli::{Cli, Command};
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("meeting.jsonl");
    let audio = dir.path().join("meeting.wav");
    let Command::Listen(mut args) = Cli::try_parse_from(["stt", "listen"]).unwrap().command else {
        unreachable!()
    };
    args.output = Some(transcript.clone());
    args.format = OutputFormat::Jsonl;
    args.audio_output = Some(audio.clone());
    args.no_partial = true;
    let mut output = Output::start(&args).await.unwrap();
    let sender = output.audio_recorder().unwrap();
    sender.send(&[1, 0, 2, 0]).unwrap();
    output.committed("Saved alongside audio".into()).unwrap();
    drop(sender);
    output.shutdown().await.unwrap();
    let line = tokio::fs::read_to_string(transcript).await.unwrap();
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["text"], "Saved alongside audio");
    let wav = tokio::fs::read(audio).await.unwrap();
    assert_eq!(&wav[44..], &[1, 0, 2, 0]);
    assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 4);
}
