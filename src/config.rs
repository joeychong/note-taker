use crate::cli::ListenArgs;
use anyhow::{Context, Result, ensure};

pub struct Config {
    pub args: ListenArgs,
    // Deliberately no Debug implementation: authentication must not appear in logs.
    pub api_key: String,
}

impl Config {
    pub fn from_args(args: ListenArgs) -> Result<Self> {
        let api_key = std::env::var("ELEVENLABS_API_KEY")
            .context("Set ELEVENLABS_API_KEY in the environment or a local .env file")?;
        ensure!(!api_key.trim().is_empty(), "ELEVENLABS_API_KEY is empty");
        ensure!(
            !args.device.trim().is_empty(),
            "Audio device must not be empty; use stt devices"
        );
        ensure!(
            !args.microphone_device.trim().is_empty(),
            "Microphone device must not be empty"
        );
        ensure!(
            args.audio_source.microphone() || args.microphone_device == "default",
            "--microphone-device requires --audio-source microphone or both"
        );
        ensure!(
            args.audio_source.system() || args.device == "default",
            "--device selects system audio; use --microphone-device for microphone-only capture"
        );
        ensure!(
            args.output.is_some() || args.format == crate::cli::OutputFormat::Txt,
            "--format jsonl requires --output; stdout always contains finalized plain text"
        );
        Ok(Self { args, api_key })
    }
}
