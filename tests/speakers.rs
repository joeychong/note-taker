use note_taker::{
    cli::OutputFormat,
    output::Output,
    stt::{protocol::parse, transcripts::TranscriptState},
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, DuplexStream};

fn setup(diarize: bool) -> (Output, TranscriptState, DuplexStream, DuplexStream) {
    let (stdout, read_stdout) = tokio::io::duplex(65536);
    let (file, read_file) = tokio::io::duplex(65536);
    let output = Output::with_writers(
        stdout,
        tokio::io::sink(),
        Some((file, OutputFormat::Jsonl)),
        false,
    );
    let mut state = TranscriptState::new(diarize);
    state.start_session("server-session-a".into());
    (output, state, read_stdout, read_file)
}
fn apply(state: &mut TranscriptState, output: &mut Output, value: Value) {
    state
        .handle(parse(&value.to_string()).unwrap().unwrap(), output)
        .unwrap();
}
fn segment(
    state: &mut TranscriptState,
    output: &mut Output,
    index: u64,
    text: &str,
    speaker: Option<&str>,
) {
    apply(
        state,
        output,
        json!({"message_type":"committed_transcript","segment_index":index,"text":text}),
    );
    apply(
        state,
        output,
        json!({"message_type":"diarized_transcript","segment_index":index,"text":text,"words":[{"text":text,"speaker_id":speaker}]}),
    );
}
async fn finish(
    output: Output,
    mut stdout: DuplexStream,
    mut file: DuplexStream,
) -> (String, Vec<Value>) {
    output.shutdown().await.unwrap();
    let mut text = String::new();
    stdout.read_to_string(&mut text).await.unwrap();
    let mut jsonl = String::new();
    file.read_to_string(&mut jsonl).await.unwrap();
    (
        text,
        jsonl
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect(),
    )
}

#[tokio::test]
async fn speaker_mode_is_optional_and_plain_output_stays_compatible() {
    let (mut output, mut state, stdout, file) = setup(false);
    apply(
        &mut state,
        &mut output,
        json!({"message_type":"partial_transcript","text":"draft"}),
    );
    segment(&mut state, &mut output, 0, "Hello", Some("speaker_0"));
    let (text, rows) = finish(output, stdout, file).await;
    assert_eq!(text, "Hello\n");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get("speakers").is_none());
}

#[tokio::test]
async fn correlates_by_index_deduplicates_metadata_and_preserves_repeated_speech() {
    let (mut output, mut state, stdout, file) = setup(true);
    for index in [0, 1] {
        apply(
            &mut state,
            &mut output,
            json!({"message_type":"committed_transcript","segment_index":index,"text":"Yes."}),
        );
    }
    assert!(state.has_pending_labels());
    for index in [0, 0, 1] {
        apply(
            &mut state,
            &mut output,
            json!({"message_type":"diarized_transcript","segment_index":index,"text":"Yes.","words":[{"text":"Yes.","speaker_id":"speaker_0"}]}),
        );
    }
    assert!(!state.has_pending_labels());
    let (text, rows) = finish(output, stdout, file).await;
    assert_eq!(text, "Speaker 1: Yes.\nSpeaker 1: Yes.\n");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["segment_index"], 0);
    assert_eq!(rows[1]["segment_index"], 1);
    assert_eq!(rows[0]["speaker_session_id"], "server-session-a");
}

#[tokio::test]
async fn multiple_speakers_unicode_and_spacing_are_preserved() {
    let (mut output, mut state, stdout, file) = setup(true);
    apply(
        &mut state,
        &mut output,
        json!({"message_type":"committed_transcript","segment_index":0,"text":"Hello, 世界!"}),
    );
    apply(
        &mut state,
        &mut output,
        json!({"message_type":"diarized_transcript","segment_index":0,"text":"Hello, 世界!","words":[
        {"text":"Hello,","speaker_id":"speaker_0"},{"text":" ","type":"spacing","speaker_id":null},{"text":"世界!","speaker_id":"speaker_1"}]}),
    );
    let (text, rows) = finish(output, stdout, file).await;
    assert_eq!(text, "Speaker 1: Hello,\nSpeaker 2: 世界!\n");
    assert_eq!(rows[0]["text"], "Hello, 世界!");
    let reconstructed: String = rows[0]["speakers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["text"].as_str().unwrap())
        .collect();
    assert_eq!(reconstructed, "Hello, 世界!");
}

#[tokio::test]
async fn chained_merges_update_future_labels_and_append_jsonl_corrections() {
    let (mut output, mut state, stdout, file) = setup(true);
    for (index, speaker) in ["speaker_0", "speaker_1", "speaker_2"].iter().enumerate() {
        segment(&mut state, &mut output, index as u64, "Hi", Some(speaker));
    }
    for (from, to) in [("speaker_2", "speaker_1"), ("speaker_1", "speaker_0")] {
        apply(
            &mut state,
            &mut output,
            json!({"message_type":"speakers_merged","from_speaker_id":from,"to_speaker_id":to}),
        );
    }
    segment(&mut state, &mut output, 3, "Again", Some("speaker_2"));
    let (text, rows) = finish(output, stdout, file).await;
    assert_eq!(
        text,
        "Speaker 1: Hi\nSpeaker 2: Hi\nSpeaker 3: Hi\nSpeaker 1: Again\n"
    );
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[3]["event"], "speaker_merge");
    assert_eq!(rows[4]["to_speaker_id"], "speaker_0");
    assert_eq!(rows[5]["speakers"][0]["speaker_id"], "speaker_0");
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row["sequence"], index + 1);
    }
}

#[tokio::test]
async fn missing_annotations_preserve_only_committed_text_as_unknown() {
    let (mut output, mut state, stdout, file) = setup(true);
    apply(
        &mut state,
        &mut output,
        json!({"message_type":"committed_transcript","segment_index":0,"text":"Final"}),
    );
    apply(
        &mut state,
        &mut output,
        json!({"message_type":"partial_transcript","text":"Uncommitted"}),
    );
    state.flush_unlabeled(&mut output).unwrap();
    let (text, rows) = finish(output, stdout, file).await;
    assert_eq!(text, "Unknown speaker: Final\n");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["speakers"][0]["speaker_id"], Value::Null);
}

#[tokio::test]
async fn reconnects_never_assume_that_reused_speaker_ids_are_the_same_person() {
    let (mut output, mut state, stdout, file) = setup(true);
    segment(&mut state, &mut output, 0, "First", Some("speaker_0"));
    let mut state = TranscriptState::new(true);
    state.start_session("server-session-b".into());
    segment(&mut state, &mut output, 0, "Second", Some("speaker_0"));
    let (text, rows) = finish(output, stdout, file).await;
    assert_eq!(text, "Speaker 1: First\nSpeaker 2: Second\n");
    assert_ne!(rows[0]["speaker_session_id"], rows[1]["speaker_session_id"]);
}

#[tokio::test]
async fn duplicated_old_commits_do_not_clear_a_newer_uncommitted_partial() {
    let (mut output, mut state, stdout, file) = setup(true);
    segment(&mut state, &mut output, 0, "Final", Some("speaker_0"));
    apply(
        &mut state,
        &mut output,
        json!({"message_type":"partial_transcript","text":"New speech"}),
    );
    apply(
        &mut state,
        &mut output,
        json!({"message_type":"committed_transcript","segment_index":0,"text":"Final"}),
    );
    assert!(state.pending_speech);
    let (text, rows) = finish(output, stdout, file).await;
    assert_eq!(text, "Speaker 1: Final\n");
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn pending_speaker_metadata_is_bounded_and_bad_word_alignment_does_not_lose_text() {
    let (mut output, mut state, stdout, file) = setup(true);
    apply(
        &mut state,
        &mut output,
        json!({"message_type":"committed_transcript","segment_index":0,"text":"Preserve this"}),
    );
    apply(
        &mut state,
        &mut output,
        json!({"message_type":"diarized_transcript","segment_index":0,"text":"Preserve this","words":[{"text":"not matching","speaker_id":"speaker_0"}]}),
    );
    let (text, _) = finish(output, stdout, file).await;
    assert_eq!(text, "Unknown speaker: Preserve this\n");
    let (mut output, mut state, stdout, file) = setup(true);
    for index in 0..64 {
        apply(
            &mut state,
            &mut output,
            json!({"message_type":"committed_transcript","segment_index":index,"text":"Hi"}),
        );
    }
    let event = parse(
        &json!({"message_type":"committed_transcript","segment_index":64,"text":"overflow"})
            .to_string(),
    )
    .unwrap()
    .unwrap();
    assert!(
        state
            .handle(event, &mut output)
            .unwrap_err()
            .to_string()
            .contains("buffer is full")
    );
    let (text, rows) = finish(output, stdout, file).await;
    assert_eq!(rows.len(), 65);
    assert!(text.ends_with("Unknown speaker: overflow\n"));
}
