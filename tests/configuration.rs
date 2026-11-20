use std::process::Command;

fn invoke(directory: &std::path::Path, env_chunk: Option<&str>, cli_chunk: Option<&str>) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_stt"));
    command
        .current_dir(directory)
        .arg("listen")
        .env("ELEVENLABS_API_KEY", "")
        .env_remove("STT_AUDIO_DEVICE")
        .env_remove("STT_OUTPUT_FORMAT")
        .env_remove("STT_VAD_SILENCE")
        .env_remove("STT_LANGUAGE")
        .env_remove("STT_CHUNK_MS");
    if let Some(value) = env_chunk {
        command.env("STT_CHUNK_MS", value);
    }
    if let Some(value) = cli_chunk {
        command.args(["--chunk-ms", value]);
    }
    let output = command.output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn cli_overrides_environment_which_overrides_dotenv() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join(".env"), "STT_CHUNK_MS=99\n").unwrap();
    assert!(invoke(directory.path(), None, None).contains("99"));
    // Empty credentials stop before any network or capture begins.
    assert!(invoke(directory.path(), Some("200"), None).contains("ELEVENLABS_API_KEY is empty"));
    assert!(
        invoke(directory.path(), Some("99"), Some("200")).contains("ELEVENLABS_API_KEY is empty")
    );
}

#[test]
fn jsonl_without_destination_is_rejected_before_connecting() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_stt"))
        .current_dir(directory.path())
        .env("ELEVENLABS_API_KEY", "test-key")
        .env("STT_CHUNK_MS", "200")
        .env("STT_VAD_SILENCE", "1")
        .args(["listen", "--format", "jsonl"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--format jsonl requires --output"));
}

#[test]
fn timestamp_flag_overrides_environment_and_dotenv() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join(".env"), "STT_TIMESTAMPS=invalid\n").unwrap();
    let run = |environment: Option<&str>, arguments: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_stt"));
        command
            .current_dir(directory.path())
            .env("ELEVENLABS_API_KEY", "")
            .env_remove("STT_TIMESTAMPS")
            .arg("listen")
            .args(arguments);
        if let Some(value) = environment {
            command.env("STT_TIMESTAMPS", value);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        String::from_utf8(output.stderr).unwrap()
    };
    assert!(run(None, &[]).contains("invalid"));
    assert!(run(Some("true"), &[]).contains("ELEVENLABS_API_KEY is empty"));
    assert!(run(Some("invalid"), &["--timestamps=false"]).contains("ELEVENLABS_API_KEY is empty"));
    assert!(run(None, &["--timestamps"]).contains("ELEVENLABS_API_KEY is empty"));
}
