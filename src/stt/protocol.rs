use crate::output::MAX_TEXT_BYTES;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SpeakerWord {
    pub text: String,
    #[serde(default)]
    pub speaker_id: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InputAudioChunk {
    pub message_type: &'static str,
    pub audio_base_64: String,
    pub commit: bool,
    pub sample_rate: u32,
}

impl InputAudioChunk {
    pub fn new(pcm: &[u8], commit: bool) -> Self {
        Self {
            message_type: "input_audio_chunk",
            audio_base_64: STANDARD.encode(pcm),
            commit,
            sample_rate: 16000,
        }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "message_type")]
pub enum Event {
    #[serde(rename = "session_started")]
    Started {
        session_id: String,
        config: serde_json::Value,
    },
    #[serde(rename = "partial_transcript")]
    Partial {
        text: String,
        #[serde(default)]
        words: Vec<SpeakerWord>,
    },
    #[serde(rename = "committed_transcript")]
    Committed {
        text: String,
        #[serde(default)]
        segment_index: Option<u64>,
    },
    #[serde(rename = "diarized_transcript")]
    Diarized {
        text: String,
        segment_index: u64,
        words: Vec<SpeakerWord>,
    },
    #[serde(rename = "speakers_merged")]
    SpeakersMerged {
        from_speaker_id: String,
        to_speaker_id: String,
    },
    #[serde(rename = "warning")]
    Warning { warning: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct Failure {
    pub retryable: bool,
    pub message: String,
}

impl Failure {
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            message: message.into(),
        }
    }
    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            retryable: false,
            message: message.into(),
        }
    }
}

pub fn parse(text: &str) -> Result<std::result::Result<Event, Failure>> {
    let value: serde_json::Value =
        serde_json::from_str(text).context("Malformed WebSocket JSON")?;
    let kind = value
        .get("message_type")
        .and_then(|x| x.as_str())
        .context("Missing message_type")?;
    if matches!(
        kind,
        "error"
            | "auth_error"
            | "quota_exceeded"
            | "commit_throttled"
            | "unaccepted_terms"
            | "rate_limited"
            | "queue_overflow"
            | "resource_exhausted"
            | "session_time_limit_exceeded"
            | "input_error"
            | "invalid_request"
            | "chunk_size_exceeded"
            | "insufficient_audio_activity"
            | "transcriber_error"
    ) {
        let error = value
            .get("error")
            .and_then(|v| v.as_str())
            .context("Malformed API error: missing error string")?;
        let retryable = matches!(
            kind,
            "resource_exhausted"
                | "session_time_limit_exceeded"
                | "transcriber_error"
                | "rate_limited"
        );
        return Ok(Err(Failure {
            retryable,
            message: format!("ElevenLabs {kind}: {error}"),
        }));
    }
    let event: Event = serde_json::from_value(value).context("Malformed ElevenLabs event")?;
    match &event {
        Event::Partial { text, .. }
        | Event::Committed { text, .. }
        | Event::Diarized { text, .. } => {
            ensure!(text.len() <= MAX_TEXT_BYTES, "Transcript exceeds 64 KiB")
        }
        _ => (),
    }
    match &event {
        Event::Partial { words, .. } | Event::Diarized { words, .. } => {
            ensure!(words.len() <= 4096, "Too many words in a speaker event");
            let mut bytes = 0;
            for word in words {
                bytes += word.text.len();
                if let Some(id) = &word.speaker_id {
                    ensure!(id.len() <= 128, "Speaker ID is too long");
                }
            }
            ensure!(bytes <= MAX_TEXT_BYTES, "Speaker word text exceeds 64 KiB");
        }
        Event::SpeakersMerged {
            from_speaker_id,
            to_speaker_id,
        } => {
            ensure!(
                from_speaker_id.len() <= 128 && to_speaker_id.len() <= 128,
                "Speaker ID is too long"
            );
        }
        _ => (),
    }
    Ok(Ok(event))
}
