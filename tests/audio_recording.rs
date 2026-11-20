use clap::Parser;
use note_taker::{
    audio::{mixer::Mixer, recording},
    cli::{Cli, Command},
};
use tokio::sync::watch;

fn verify_wav(bytes: &[u8], pcm: &[u8]) {
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..16], b"WAVEfmt ");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
        bytes.len() - 8
    );
    assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 16);
    assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 1);
    assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 16000);
    assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 32000);
    assert_eq!(u16::from_le_bytes(bytes[32..34].try_into().unwrap()), 2);
    assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16);
    assert_eq!(&bytes[36..40], b"data");
    assert_eq!(
        u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize,
        pcm.len()
    );
    assert_eq!(&bytes[44..], pcm);
}

#[tokio::test]
async fn records_mixed_audio_in_order_and_finalizes_partial_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meeting.wav");
    let (errors, state) = watch::channel(None);
    let (sender, task) = recording::start(&path, false, errors).await.unwrap();
    let mut mixer = Mixer::new(2, 200).unwrap();
    let a = [1000i16.to_le_bytes(); 3217].concat();
    let b = [3000i16.to_le_bytes(); 3217].concat();
    mixer.push(0, &a).unwrap();
    mixer.push(1, &b).unwrap();
    let mut expected = Vec::new();
    for chunk in mixer.chunks() {
        expected.extend_from_slice(&chunk);
        sender.send(&chunk).unwrap();
    }
    mixer.finish(0);
    mixer.finish(1);
    for chunk in mixer.chunks() {
        expected.extend_from_slice(&chunk);
        sender.send(&chunk).unwrap();
    }
    assert_eq!(expected, [2000i16.to_le_bytes(); 3217].concat());
    drop(sender);
    task.await.unwrap().unwrap();
    verify_wav(&tokio::fs::read(path).await.unwrap(), &expected);
    assert!(state.borrow().is_none());
}

#[tokio::test]
async fn refuses_existing_audio_without_explicit_overwrite_and_handles_bad_paths() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("existing.wav");
    tokio::fs::write(&path, b"keep me").await.unwrap();
    let (errors, _) = watch::channel(None);
    assert!(
        recording::start(&path, false, errors.clone())
            .await
            .is_err()
    );
    assert_eq!(tokio::fs::read(&path).await.unwrap(), b"keep me");
    let (sender, task) = recording::start(&path, true, errors.clone()).await.unwrap();
    drop(sender);
    task.await.unwrap().unwrap();
    verify_wav(&tokio::fs::read(path).await.unwrap(), &[]);
    assert!(
        recording::start(&dir.path().join("missing/audio.wav"), false, errors)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn refusing_an_existing_recording_does_not_truncate_the_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("meeting.txt");
    let audio = dir.path().join("meeting.wav");
    tokio::fs::write(&transcript, b"existing transcript")
        .await
        .unwrap();
    tokio::fs::write(&audio, b"existing audio").await.unwrap();
    let Command::Listen(mut args) = Cli::try_parse_from(["stt", "listen"]).unwrap().command else {
        unreachable!()
    };
    args.output = Some(transcript.clone());
    args.overwrite = true;
    args.audio_output = Some(audio.clone());
    assert!(note_taker::output::Output::start(&args).await.is_err());
    assert_eq!(
        tokio::fs::read(transcript).await.unwrap(),
        b"existing transcript"
    );
    assert_eq!(tokio::fs::read(audio).await.unwrap(), b"existing audio");
}

#[tokio::test]
async fn bounded_recording_reports_backpressure_and_drains_already_accepted_audio() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bounded.wav");
    let (errors, _) = watch::channel(None);
    let (sender, task) = recording::start(&path, false, errors).await.unwrap();
    // Without yielding, the writer cannot drain this current-thread runtime's queue.
    let pcm = vec![42; 32000];
    sender.send(&pcm).unwrap();
    sender.send(&pcm).unwrap();
    assert!(
        sender
            .send(&[0, 0])
            .unwrap_err()
            .to_string()
            .contains("two-second buffer")
    );
    assert!(
        sender
            .send(&[1])
            .unwrap_err()
            .to_string()
            .contains("Misaligned")
    );
    drop(sender);
    task.await.unwrap().unwrap();
    verify_wav(
        &tokio::fs::read(path).await.unwrap(),
        &[pcm.clone(), pcm].concat(),
    );
}

#[tokio::test]
async fn wav_header_is_checkpointed_while_recording_is_still_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.wav");
    let (errors, _) = watch::channel(None);
    let (sender, task) = recording::start(&path, false, errors).await.unwrap();
    sender.send(&[1, 0, 2, 0]).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1150)).await;
    verify_wav(&tokio::fs::read(&path).await.unwrap(), &[1, 0, 2, 0]);
    sender.send(&[3, 0]).unwrap();
    drop(sender);
    task.await.unwrap().unwrap();
    verify_wav(&tokio::fs::read(&path).await.unwrap(), &[1, 0, 2, 0, 3, 0]);
}

#[test]
fn transcript_and_audio_cannot_share_a_path_and_audio_overwrite_requires_a_path() {
    assert!(Cli::try_parse_from(["stt", "listen", "--overwrite-audio"]).is_err());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meeting.wav");
    let Command::Listen(mut args) = Cli::try_parse_from(["stt", "listen"]).unwrap().command else {
        unreachable!()
    };
    args.output = Some(path.clone());
    args.audio_output = Some(dir.path().join("./meeting.wav"));
    args.overwrite = true;
    args.overwrite_audio = true;
    assert!(recording::validate_paths(&args).is_err());
    std::fs::write(&path, b"keep me").unwrap();
    assert!(recording::validate_paths(&args).is_err());
    #[cfg(unix)]
    {
        let linked = dir.path().join("linked.wav");
        std::fs::hard_link(&path, &linked).unwrap();
        args.audio_output = Some(linked);
        assert!(recording::validate_paths(&args).is_err());
    }
    assert_eq!(std::fs::read(path).unwrap(), b"keep me");
}
