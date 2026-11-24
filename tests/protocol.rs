use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use note_taker::{
    cli::{Cli, Command, ListenArgs},
    stt::{
        elevenlabs::{Connection, connection_url, retry_delay},
        protocol::{Event, InputAudioChunk, parse},
    },
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{net::TcpListener, time::timeout};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
    },
};

fn args() -> ListenArgs {
    match Cli::try_parse_from(["stt", "listen"]).unwrap().command {
        Command::Listen(args) => args,
        _ => unreachable!(),
    }
}

#[test]
fn chunk_protocol_and_query_are_documented_shapes() {
    let value = serde_json::to_value(InputAudioChunk::new(&[0, 64, 0, 128], false)).unwrap();
    assert_eq!(
        value,
        json!({"message_type":"input_audio_chunk","audio_base_64":"AEAAgA==","sample_rate":16000,"commit":false})
    );
    let value = serde_json::to_value(InputAudioChunk::new(&[], true)).unwrap();
    assert_eq!(value["audio_base_64"], "");
    assert_eq!(value["commit"], true);
    let url = connection_url(
        "wss://api.elevenlabs.io/v1/speech-to-text/realtime",
        &args(),
    )
    .unwrap();
    let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(query["model_id"], "scribe_v3_live");
    assert_eq!(query["commit_strategy"], "vad");
    assert!(!query.contains_key("xi-api-key"));
}

#[test]
fn language_lists_configure_primary_secondary_and_multilingual_mode() {
    for (options, expected) in [
        (vec![], vec![]),
        (vec!["--language", "EN"], vec!["en"]),
        (vec!["--language", "EN, ms,zh"], vec!["en", "ms", "zh"]),
        (
            vec!["--language", "en", "--language", "ms,zh"],
            vec!["en", "ms", "zh"],
        ),
    ] {
        let Command::Listen(args) = Cli::try_parse_from([vec!["stt", "listen"], options].concat())
            .unwrap()
            .command
        else {
            unreachable!()
        };
        assert_eq!(args.language, expected);
        let url = connection_url("wss://example.com/realtime", &args).unwrap();
        let values = |name: &str| -> Vec<String> {
            url.query_pairs()
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .collect()
        };
        assert_eq!(values("language_code"), expected[..expected.len().min(1)]);
        assert_eq!(
            values("secondary_languages"),
            expected.get(1..).unwrap_or(&[])
        );
        assert_eq!(
            values("multilang_mode"),
            if expected.len() > 1 {
                vec!["true"]
            } else {
                vec![]
            }
        );
    }
    for languages in ["en,invalid", "en,", ",en", "en,,zh", "en,12"] {
        assert!(Cli::try_parse_from(["stt", "listen", "--language", languages]).is_err());
    }
}

#[test]
fn event_schemas_errors_and_metadata() {
    assert_eq!(
        parse(r#"{"message_type":"partial_transcript","text":"hi"}"#)
            .unwrap()
            .unwrap(),
        Event::Partial {
            text: "hi".into(),
            words: vec![]
        }
    );
    assert_eq!(
        parse(r#"{"message_type":"committed_transcript","text":"hi"}"#)
            .unwrap()
            .unwrap(),
        Event::Committed {
            text: "hi".into(),
            segment_index: None
        }
    );
    assert_eq!(
        parse(r#"{"message_type":"committed_transcript_with_timestamps","text":"hi","words":[]}"#)
            .unwrap()
            .unwrap(),
        Event::Other
    );
    assert!(
        !parse(r#"{"message_type":"auth_error","error":"bad key"}"#)
            .unwrap()
            .unwrap_err()
            .retryable
    );
    assert!(
        parse(r#"{"message_type":"transcriber_error","error":"temporary"}"#)
            .unwrap()
            .unwrap_err()
            .retryable
    );
    assert!(parse("not json").is_err());
    assert!(parse(r#"{"message_type":"committed_transcript"}"#).is_err());
    assert!(parse(r#"{"message_type":"auth_error"}"#).is_err());
}

#[test]
fn cli_validation_and_bounded_backoff() {
    let cli = Cli::try_parse_from([
        "stt",
        "listen",
        "--diarize",
        "--audio-source",
        "both",
        "--microphone-device",
        "mic:test",
    ])
    .unwrap();
    let Command::Listen(speaker_args) = cli.command else {
        unreachable!()
    };
    assert!(speaker_args.diarize);
    assert_eq!(
        speaker_args.audio_source,
        note_taker::cli::AudioSourceMode::Both
    );
    let url = connection_url(
        "wss://api.elevenlabs.io/v1/speech-to-text/realtime",
        &speaker_args,
    )
    .unwrap();
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "diarize" && value == "true")
    );
    assert!(Cli::try_parse_from(["stt", "listen", "--audio-source", "invalid"]).is_err());
    assert!(Cli::try_parse_from(["stt", "devices", "--audio-source", "microphone"]).is_ok());
    for options in [
        vec!["--chunk-ms", "99"],
        vec!["--vad-silence", "NaN"],
        vec!["--vad-silence", "4"],
        vec!["--overwrite"],
        vec!["--language", "invalid"],
    ] {
        assert!(Cli::try_parse_from([vec!["stt", "listen"], options].concat()).is_err());
    }
    for attempt in 0..10 {
        let delay = retry_delay(attempt);
        assert!(delay >= Duration::from_secs(1_u64 << attempt.min(4)));
        assert!(delay <= Duration::from_millis(16250));
    }
}

#[tokio::test]
#[allow(clippy::result_large_err)] // tungstenite's handshake callback fixes this error type.
async fn mock_stream_authentication_chunks_commit_and_final() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(tcp, |request: &Request, response: Response| {
            assert_eq!(request.headers()["xi-api-key"], "test-key");
            assert!(
                request
                    .uri()
                    .query()
                    .unwrap()
                    .contains("model_id=scribe_v3_live")
            );
            Ok(response)
        })
        .await
        .unwrap();
        ws.send(Message::Text(json!({"message_type":"session_started","session_id":"mock", "config":{"model_id":"scribe_v3_live"}}).to_string().into())).await.unwrap();
        let chunk: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(chunk["audio_base_64"], "AAAAAA==");
        ws.send(Message::Text(
            json!({"message_type":"partial_transcript","text":"hello"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let commit: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(commit["commit"], true);
        assert_eq!(commit["audio_base_64"], "");
        ws.send(Message::Text(
            json!({"message_type":"committed_transcript","text":"hello world"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        while let Some(Ok(message)) = ws.next().await {
            if message.is_close() {
                break;
            }
        }
    });
    let mut connection =
        Connection::connect(&format!("ws://{address}/realtime"), "test-key", &args())
            .await
            .unwrap();
    assert!(matches!(
        connection.events.recv().await.unwrap(),
        Event::Started { .. }
    ));
    connection.audio_sender().send(vec![0; 4]).unwrap();
    assert!(matches!(
        connection.events.recv().await.unwrap(),
        Event::Partial { .. }
    ));
    connection.commit().await.unwrap();
    assert_eq!(
        connection.events.recv().await.unwrap(),
        Event::Committed {
            text: "hello world".into(),
            segment_index: None
        }
    );
    connection.close().await.unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn disconnect_is_reported_and_a_new_connection_never_replays_audio() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let message = ws.next().await.unwrap().unwrap();
            let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            // A fresh connection only receives newly supplied samples.
            assert_eq!(
                value["audio_base_64"],
                if index == 0 { "AQI=" } else { "AwQ=" }
            );
            ws.close(None).await.unwrap();
        }
    });
    for bytes in [vec![1, 2], vec![3, 4]] {
        let mut connection = Connection::connect(&format!("ws://{address}"), "test-key", &args())
            .await
            .unwrap();
        connection.audio_sender().send(bytes).unwrap();
        timeout(Duration::from_secs(2), connection.failures.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(connection.failures.borrow().as_ref().unwrap().retryable);
    }
    server.await.unwrap();
}
