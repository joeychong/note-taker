use super::*;
use crate::cli::{Cli, Command, OutputFormat};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::{io::AsyncReadExt, net::TcpListener};
use tokio_tungstenite::{accept_async, tungstenite::Message};

fn args() -> crate::cli::ListenArgs {
    match Cli::try_parse_from(["stt", "listen"]).unwrap().command {
        Command::Listen(args) => args,
        _ => unreachable!(),
    }
}

#[tokio::test]
#[allow(clippy::result_large_err)] // Fixed by tungstenite's callback signature.
async fn supervisor_exhausts_transient_retries_but_does_not_retry_authentication() {
    for (status, expected_attempts) in [(503, 6), (401, 1)] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..expected_attempts {
                let (tcp, _) = listener.accept().await.unwrap();
                let result = tokio_tungstenite::accept_hdr_async(tcp,
                    |_: &tokio_tungstenite::tungstenite::handshake::server::Request,
                     _: tokio_tungstenite::tungstenite::handshake::server::Response| {
                        Err(tokio_tungstenite::tungstenite::http::Response::builder()
                            .status(status).body(Some("mock failure".into())).unwrap())
                    }).await;
                assert!(result.is_err());
            }
        });
        let mut output = Output::with_writers(
            tokio::io::sink(),
            tokio::io::sink(),
            None::<(tokio::io::Sink, OutputFormat)>,
            false,
        );
        let config = Config {
            args: args(),
            api_key: "test-key".into(),
        };
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            run_at(
                &config,
                &mut output,
                &CancellationToken::new(),
                &format!("ws://{address}"),
                |_| Duration::ZERO,
            ),
        )
        .await
        .unwrap();
        let error = result.unwrap_err().to_string();
        if status == 503 {
            assert!(error.contains("attempts exhausted"));
        } else {
            assert!(error.contains("401"));
        }
        output.shutdown().await.unwrap();
        server.await.unwrap();
    }
}

async fn fake_finalizer(deliver_final: bool) -> (Connection, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        let commit = ws.next().await.unwrap().unwrap();
        let commit: serde_json::Value = serde_json::from_str(commit.to_text().unwrap()).unwrap();
        assert_eq!(commit["commit"], true);
        assert_eq!(commit["audio_base_64"], "");
        ws.send(Message::Text(
            json!({"message_type":"partial_transcript", "text":"draft"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        if deliver_final {
            for kind in [
                "committed_transcript",
                "committed_transcript_with_timestamps",
            ] {
                ws.send(Message::Text(
                    json!({"message_type":kind, "text":"Final speech"})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
        }
        while let Some(Ok(message)) = ws.next().await {
            if message.is_close() {
                break;
            }
        }
    });
    let connection = Connection::connect(&format!("ws://{address}"), "test-key", &args())
        .await
        .unwrap();
    (connection, server)
}

#[tokio::test]
async fn finalization_waits_for_final_and_ignores_delayed_metadata() {
    let (mut connection, server) = fake_finalizer(true).await;
    let (writer, mut reader) = tokio::io::duplex(4096);
    let mut output = Output::with_writers(
        writer,
        tokio::io::sink(),
        None::<(tokio::io::Sink, OutputFormat)>,
        false,
    );
    let mut pending = TranscriptState::new(false);
    pending.pending_speech = true;
    finalize_with_wait(
        &mut connection,
        &mut output,
        &mut pending,
        Duration::from_millis(100),
    )
    .await
    .unwrap();
    assert!(!pending.pending_speech);
    connection.close().await.unwrap();
    output.shutdown().await.unwrap();
    let mut text = String::new();
    reader.read_to_string(&mut text).await.unwrap();
    assert_eq!(text, "Final speech\n");
    server.await.unwrap();
}

#[tokio::test]
async fn missing_final_is_an_error_and_does_not_promote_partial() {
    let (mut connection, server) = fake_finalizer(false).await;
    let (writer, mut reader) = tokio::io::duplex(4096);
    let mut output = Output::with_writers(
        writer,
        tokio::io::sink(),
        None::<(tokio::io::Sink, OutputFormat)>,
        false,
    );
    let mut pending = TranscriptState::new(false);
    pending.pending_speech = true;
    let result = finalize_with_wait(
        &mut connection,
        &mut output,
        &mut pending,
        Duration::from_millis(100),
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("unconfirmed speech")
    );
    connection.close().await.unwrap();
    output.shutdown().await.unwrap();
    let mut text = String::new();
    reader.read_to_string(&mut text).await.unwrap();
    assert!(text.is_empty());
    server.await.unwrap();
}

#[tokio::test]
async fn cancelled_startup_does_not_open_a_connection_or_start_capture() {
    let stop = CancellationToken::new();
    stop.cancel();
    let mut output = Output::with_writers(
        tokio::io::sink(),
        tokio::io::sink(),
        None::<(tokio::io::Sink, OutputFormat)>,
        false,
    );
    run(
        &Config {
            args: args(),
            api_key: "not-a-real-key".into(),
        },
        &mut output,
        &stop,
    )
    .await
    .unwrap();
    output.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_speaker_annotations_and_preserves_text_when_they_timeout() {
    for deliver_labels in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(tcp).await.unwrap();
            let commit = ws.next().await.unwrap().unwrap();
            assert!(commit.to_text().unwrap().contains("\"commit\":true"));
            ws.send(Message::Text(json!({"message_type":"committed_transcript","segment_index":0,"text":"Final words"}).to_string().into())).await.unwrap();
            if deliver_labels {
                tokio::time::sleep(Duration::from_millis(15)).await;
                ws.send(Message::Text(json!({"message_type":"diarized_transcript","segment_index":0,"text":"Final words","words":[{"text":"Final words","speaker_id":"speaker_0"}]}).to_string().into())).await.unwrap();
            }
            while let Some(Ok(message)) = ws.next().await {
                if message.is_close() {
                    break;
                }
            }
        });
        let mut settings = args();
        settings.diarize = true;
        let mut connection = Connection::connect(&format!("ws://{address}"), "test-key", &settings)
            .await
            .unwrap();
        let (writer, mut reader) = tokio::io::duplex(4096);
        let mut output = Output::with_writers(
            writer,
            tokio::io::sink(),
            None::<(tokio::io::Sink, OutputFormat)>,
            false,
        );
        let mut state = TranscriptState::new(true);
        state.start_session("mock".into());
        let result = finalize_with_wait(
            &mut connection,
            &mut output,
            &mut state,
            Duration::from_millis(100),
        )
        .await;
        if deliver_labels {
            result.unwrap();
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Speaker finalization timed out")
            );
        }
        connection.close().await.unwrap();
        output.shutdown().await.unwrap();
        let mut text = String::new();
        reader.read_to_string(&mut text).await.unwrap();
        assert_eq!(
            text,
            if deliver_labels {
                "Speaker 1: Final words\n"
            } else {
                "Unknown speaker: Final words\n"
            }
        );
        server.await.unwrap();
    }
}
