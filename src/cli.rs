use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

const USAGE_GUIDE: &str = "Quick start:
  Set ELEVENLABS_API_KEY in your environment or a .env file.
  Allow Screen & System Audio Recording for system audio on macOS.
  Allow Microphone access when capturing a microphone.

Examples:
  stt listen                                      Transcribe system audio
  stt listen --output transcript.txt              Display and append to TXT
  stt listen --output meeting.txt --audio-output meeting.wav
                                                  Save transcripts and playback audio
  stt listen --diarize --timestamps --output meeting.txt
                                                  Add speaker labels and elapsed time
  stt listen --audio-source microphone            Capture your microphone
  stt listen --audio-source both --diarize         Capture a meeting and your microphone
  stt listen --output meeting.jsonl --format jsonl Save structured JSON Lines
  stt listen --output transcript.txt --overwrite  Replace an existing file
  stt listen --language en --no-partial            English, finalized lines only
  stt devices                                    List system audio sources
  stt devices --audio-source microphone           List microphones

Transcript files append by default. Only finalized transcripts are saved.
Audio is WAV (16 kHz mono); existing files require --overwrite-audio.
--timestamps shows elapsed recognition time, not exact speech start time.
Use headphones with --audio-source both to reduce duplicate audio.
Press Ctrl+C once to stop; wait for final transcripts and file flushing.
Audio is sent to ElevenLabs for transcription.

More help:
  stt listen --help      All capture and output options
  stt devices --help     Source listing options";

#[derive(Parser, Debug)]
#[command(
    name = "stt",
    version,
    about = "Transcribe system audio or a microphone with ElevenLabs Scribe Live",
    after_help = USAGE_GUIDE
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Capture audio and transcribe it live
    #[command(after_help = USAGE_GUIDE)]
    Listen(ListenArgs),
    /// List system audio capture sources (macOS may require recording permission)
    Devices(DeviceArgs),
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum AudioSourceMode {
    System,
    Microphone,
    Both,
}

impl AudioSourceMode {
    pub fn system(self) -> bool {
        matches!(self, Self::System | Self::Both)
    }
    pub fn microphone(self) -> bool {
        matches!(self, Self::Microphone | Self::Both)
    }
}

#[derive(clap::Args, Debug)]
pub struct DeviceArgs {
    /// List system audio sources, microphones, or both
    #[arg(long, value_enum, default_value = "system")]
    pub audio_source: AudioSourceMode,
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum OutputFormat {
    Txt,
    Jsonl,
}

#[derive(clap::Args, Debug)]
pub struct ListenArgs {
    /// Choose remote/system audio, local microphone, or a mix of both
    #[arg(long, env = "STT_AUDIO_SOURCE", default_value = "system")]
    pub audio_source: AudioSourceMode,
    /// Add anonymous speaker labels (not meeting participant names)
    #[arg(long, alias = "speaker-detection", env = "STT_DIARIZE", action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true", default_value_t = false)]
    pub diarize: bool,
    /// Prefix transcript lines with elapsed recognition time [HH:MM:SS]
    #[arg(long, env = "STT_TIMESTAMPS", action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true", default_value_t = false)]
    pub timestamps: bool,
    /// Microphone ID from devices --audio-source microphone
    #[arg(long, env = "STT_MICROPHONE_DEVICE", default_value = "default")]
    pub microphone_device: String,
    /// System audio source ID from stt devices
    #[arg(long, env = "STT_AUDIO_DEVICE", default_value = "default")]
    pub device: String,
    /// Audio chunk duration in milliseconds (100–1000)
    #[arg(long, env = "STT_CHUNK_MS", default_value_t = 200, value_parser = clap::value_parser!(u32).range(100..=1000))]
    pub chunk_ms: u32,
    /// Silence duration in seconds before committing speech (0.192–3.0)
    #[arg(long, env = "STT_VAD_SILENCE", default_value_t = 1.0, value_parser = parse_vad)]
    pub vad_silence: f64,
    /// Comma-separated language codes (e.g. en,ms,zh); first is primary; omit for auto detection
    #[arg(long, env = "STT_LANGUAGE", value_delimiter = ',', value_parser = parse_language)]
    pub language: Vec<String>,
    /// Append finalized transcripts while continuing terminal output
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Save captured audio as a 16 kHz mono PCM WAV file for playback
    #[arg(long, value_name = "PATH")]
    pub audio_output: Option<PathBuf>,
    /// Replace an existing audio file (WAV recordings never append)
    #[arg(long, requires = "audio_output")]
    pub overwrite_audio: bool,
    /// Saved file format; jsonl requires --output
    #[arg(long, env = "STT_OUTPUT_FORMAT", default_value = "txt")]
    pub format: OutputFormat,
    /// Truncate the output file instead of appending
    #[arg(long, requires = "output")]
    pub overwrite: bool,
    /// Display finalized text only, without live partial updates
    #[arg(long)]
    pub no_partial: bool,
}

fn parse_vad(s: &str) -> Result<f64, String> {
    let value = s.parse::<f64>().map_err(|_| "expected a number")?;
    if value.is_finite() && (0.192..=3.0).contains(&value) {
        Ok(value)
    } else {
        Err("VAD silence must be between 0.192 and 3.0 seconds".into())
    }
}

fn parse_language(s: &str) -> Result<String, String> {
    let s = s.trim();
    if (2..=3).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphabetic()) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err("expected a two- or three-letter language code, such as en or eng".into())
    }
}
