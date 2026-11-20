use super::protocol::{Event, Failure, InputAudioChunk, parse};
use crate::{cli::ListenArgs, error::PipelineError};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{self, Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;

pub const ENDPOINT: &str = "wss://api.elevenlabs.io/v1/speech-to-text/realtime";

pub fn connection_url(endpoint: &str, args: &ListenArgs) -> Result<url::Url> {
    let mut url = url::Url::parse(endpoint)?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("model_id", "scribe_v3_live")
            .append_pair("audio_format", "pcm_16000")
            .append_pair("commit_strategy", "vad")
            .append_pair("vad_silence_threshold_secs", &args.vad_silence.to_string())
            .append_pair("keepalive_interval_ms", "5000");
        if let Some(language) = &args.language {
            query.append_pair("language_code", language);
        }
        if args.diarize {
            query.append_pair("diarize", "true");
        }
    }
    Ok(url)
}

pub enum Outbound {
    Audio(Vec<u8>),
    Commit(oneshot::Sender<()>),
    Pong(Vec<u8>),
    Close(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct AudioSender(mpsc::Sender<Outbound>);

impl AudioSender {
    pub fn send(&self, pcm: Vec<u8>) -> Result<()> {
        self.0.try_send(Outbound::Audio(pcm)).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                PipelineError::Backpressure("network audio (two seconds)")
            }
            mpsc::error::TrySendError::Closed(_) => PipelineError::OutputClosed("network audio"),
        })?;
        Ok(())
    }
}

pub struct Connection {
    pub events: mpsc::Receiver<Event>,
    pub failures: watch::Receiver<Option<Failure>>,
    sender: mpsc::Sender<Outbound>,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

fn fail(sender: &watch::Sender<Option<Failure>>, failure: Failure) {
    sender.send_if_modified(|current| {
        if current.is_none() {
            *current = Some(failure);
            true
        } else {
            false
        }
    });
}

impl Connection {
    pub async fn connect(
        endpoint: &str,
        key: &str,
        args: &ListenArgs,
    ) -> std::result::Result<Self, Failure> {
        // tokio-tungstenite enables Rustls without choosing a crypto backend.
        // Install ring before its TLS connector builds a ClientConfig. If another
        // caller installed a process provider first, keep that provider; a racing
        // install is harmless because install_default only succeeds once.
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let url = connection_url(endpoint, args)
            .map_err(|_| Failure::fatal("Invalid WebSocket endpoint"))?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| Failure::fatal("Invalid WebSocket request"))?;
        let mut header = key
            .parse::<tungstenite::http::HeaderValue>()
            .map_err(|_| Failure::fatal("ELEVENLABS_API_KEY is not a valid HTTP header"))?;
        header.set_sensitive(true);
        request.headers_mut().insert("xi-api-key", header);
        let config = WebSocketConfig::default()
            .max_message_size(Some(1024 * 1024))
            .max_frame_size(Some(1024 * 1024));
        let (socket, _) = timeout(Duration::from_secs(10), connect_async_with_config(request, Some(config), true)).await
            .map_err(|_| Failure::transient("WebSocket connection timed out"))?
            .map_err(|e| match e {
                tungstenite::Error::Http(response) => {
                    let status = response.status();
                    Failure { retryable: status.is_server_error() || status.as_u16() == 429,
                        message: format!("WebSocket handshake rejected: HTTP {status}; check API key, account quota and service availability") }
                }
                // Avoid Debug-formatting requests or responses, which can contain authentication.
                _ => Failure::transient("WebSocket connection failed (network, TLS, or protocol error)"),
            })?;
        let (mut sink, mut stream) = socket.split();
        let (sender, mut outgoing) = mpsc::channel((2000 / args.chunk_ms) as usize);
        let (event_tx, events) = mpsc::channel(128);
        let (failure_tx, failures) = watch::channel(None);
        let (activity_tx, activity) = watch::channel(Instant::now());
        let cancel = CancellationToken::new();
        let reader_cancel = cancel.clone();
        let writer_cancel = cancel.clone();
        let reader_failures = failure_tx.clone();
        let pong_tx = sender.clone();
        let reader = tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    _ = reader_cancel.cancelled() => break,
                    message = stream.next() => message,
                };
                activity_tx.send_replace(Instant::now());
                let event = match message {
                    Some(Ok(Message::Text(text))) => match parse(&text) {
                        Ok(Ok(event)) => Some(event),
                        Ok(Err(error)) => {
                            fail(&reader_failures, error);
                            break;
                        }
                        Err(error) => {
                            fail(&reader_failures, Failure::fatal(error.to_string()));
                            break;
                        }
                    },
                    Some(Ok(Message::Ping(bytes))) => {
                        if pong_tx.try_send(Outbound::Pong(bytes.to_vec())).is_err() {
                            fail(
                                &reader_failures,
                                Failure::fatal("Network control queue is full"),
                            );
                            break;
                        }
                        None
                    }
                    Some(Ok(Message::Pong(_))) => None,
                    Some(Ok(Message::Close(_))) | None => {
                        fail(
                            &reader_failures,
                            Failure::transient(
                                "WebSocket disconnected; uncommitted speech may be lost",
                            ),
                        );
                        break;
                    }
                    Some(Ok(_)) => {
                        fail(
                            &reader_failures,
                            Failure::fatal("Unexpected binary WebSocket message"),
                        );
                        break;
                    }
                    Some(Err(_)) => {
                        fail(
                            &reader_failures,
                            Failure::transient("WebSocket receive failed; delivery is uncertain"),
                        );
                        break;
                    }
                };
                if let Some(event) = event
                    && event_tx.try_send(event).is_err()
                {
                    fail(
                        &reader_failures,
                        Failure::fatal(
                            "Transcript event queue stalled; transcription may be incomplete",
                        ),
                    );
                    break;
                }
            }
            reader_cancel.cancel();
        });
        let writer = tokio::spawn(async move {
            let mut heartbeat = tokio::time::interval_at(
                Instant::now() + Duration::from_secs(15),
                Duration::from_secs(15),
            );
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let command = tokio::select! {
                    _ = writer_cancel.cancelled() => break,
                    command = outgoing.recv() => command,
                    _ = heartbeat.tick() => {
                        if activity.borrow().elapsed() > Duration::from_secs(45) {
                            fail(&failure_tx, Failure::transient("WebSocket health timeout")); break;
                        }
                        if !matches!(timeout(Duration::from_secs(5), sink.send(Message::Ping(Vec::new().into()))).await, Ok(Ok(()))) {
                            fail(&failure_tx, Failure::transient("WebSocket ping failed")); break;
                        }
                        continue;
                    }
                };
                let (message, ack, closing) = match command {
                    Some(Outbound::Audio(bytes)) => (
                        Message::Text(
                            serde_json::to_string(&InputAudioChunk::new(&bytes, false))
                                .expect("serializable chunk")
                                .into(),
                        ),
                        None,
                        false,
                    ),
                    Some(Outbound::Commit(ack)) => (
                        Message::Text(
                            serde_json::to_string(&InputAudioChunk::new(&[], true))
                                .expect("serializable chunk")
                                .into(),
                        ),
                        Some(ack),
                        false,
                    ),
                    Some(Outbound::Pong(bytes)) => (Message::Pong(bytes.into()), None, false),
                    Some(Outbound::Close(ack)) => (Message::Close(None), Some(ack), true),
                    None => break,
                };
                if !matches!(
                    timeout(Duration::from_secs(5), sink.send(message)).await,
                    Ok(Ok(()))
                ) {
                    fail(
                        &failure_tx,
                        Failure::transient(
                            "WebSocket send failed or timed out; delivery is uncertain",
                        ),
                    );
                    break;
                }
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
                if closing {
                    break;
                }
            }
            writer_cancel.cancel();
        });
        Ok(Self {
            events,
            failures,
            sender,
            cancel,
            tasks: vec![reader, writer],
        })
    }

    pub fn audio_sender(&self) -> AudioSender {
        AudioSender(self.sender.clone())
    }

    pub fn commit(&self) -> impl std::future::Future<Output = Result<()>> + Send + 'static {
        let sender = self.sender.clone();
        async move {
            let (tx, rx) = oneshot::channel();
            timeout(Duration::from_secs(5), async {
                sender
                    .send(Outbound::Commit(tx))
                    .await
                    .context("Cannot queue final commit")?;
                rx.await.context("Final commit could not be transmitted")
            })
            .await
            .context("Final commit timed out")?
        }
    }

    pub async fn close(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        timeout(Duration::from_secs(3), async {
            self.sender
                .send(Outbound::Close(tx))
                .await
                .context("Cannot queue WebSocket closure")?;
            rx.await.context("WebSocket close failed")
        })
        .await
        .context("WebSocket close timed out")?
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub fn retry_delay(attempt: u32) -> Duration {
    let seconds = (1_u64 << attempt.min(4)).min(16);
    let jitter = uuid::Uuid::new_v4().as_u128() as u64 % 251;
    Duration::from_millis(seconds * 1000 + jitter)
}
