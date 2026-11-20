# stt: native system audio transcription

A Rust CLI that streams system audio, microphone audio, or both to ElevenLabs **Scribe v3 Live**. macOS system capture uses ScreenCaptureKit; Windows WASAPI loopback is implemented at source level. Microphone capture uses native CoreAudio/WASAPI through CPAL. Final text appears on stdout while an independent task optionally saves it to TXT or JSONL. Optional speaker diarization adds anonymous speaker labels.

No Node.js, Python, FFmpeg, BlackHole, virtual audio driver, or external recording process is required.

## Platform status

The deployment target is macOS 13+, on Apple Silicon and Intel. Both ARM64 and x86_64 release executables compile locally with a verified macOS 13.0 minimum-version load command. Development and automated test execution use Apple Silicon macOS 27.2.1. Intel execution is unverified because Rosetta is unavailable on this machine. The included workflow is configured to build and test on native ARM64 and Intel runners, but has not been run here. macOS 13 runtime behavior and Intel hardware capture still require testing on those systems.

Local native capture smoke testing reached the macOS permission check and was denied. Actual audio capture and end-to-end transcription therefore remain unverified here; no ElevenLabs API key was available. The automated conversion, output, and mock-network tests do not substitute for that verification.

**Windows is implemented at source level only and is unverified.** No Windows build, automated test execution, or real-device capture test has been run. The intended baseline is Windows 10 version 1703+ and Windows 11, with MSVC x64 and ARM64 targets subject to later platform validation. No Windows executable or release archive is provided yet. A successful macOS test run does not compile or validate the Windows-specific module.

Linux PulseAudio/PipeWire capture remains deferred. Building on another OS deliberately fails with an explicit diagnostic.

## Build and install

### macOS

Install current stable Rust from [rustup.rs](https://rustup.rs/) and Apple's Xcode developer tools. A current full Xcode installation is recommended: the native binding compiles Swift bridges and needs a compatible macOS SDK and Swift toolchain. No audio driver or additional package manager library is needed.

```sh
xcode-select --install
rustup update stable
cargo build --locked --release
./target/release/stt --version
# Optional installation into your PATH:
cargo install --locked --path .
```

Full Xcode users can select its toolchain with `sudo xcode-select -s /Applications/Xcode.app/Contents/Developer`. Keep the install path stable after granting permissions. The binary embeds a ScreenCaptureKit purpose string and links the macOS Swift runtime through `/usr/lib/swift`.

Release archives are named `stt-macos-arm64.tar.gz` and `stt-macos-x86_64.tar.gz`, with SHA-256 checksums. CI publishes them for `v*` tags only after both native jobs succeed. Binaries are ad-hoc signed, not Developer ID signed or notarized; macOS download security may require explicitly allowing a trusted binary in System Settings. Cross-compiling Intel from ARM needs the Intel Rust target and a compatible Apple SDK/Swift bridge build; native runners are preferred.

### Windows: build and verify on your own machine

Install current stable Rust using [rustup](https://rustup.rs/) with its native MSVC toolchain. Install Visual Studio Build Tools with the **Desktop development with C++** workload and an appropriate Windows SDK. On ARM64, use an ARM64-capable MSVC toolchain and SDK. No virtual audio driver or separate audio-recording program is needed.

The following commands are provided for later validation; they have **not** been run on Windows:

```powershell
cargo build --locked --release
.\target\release\stt.exe --version
.\target\release\stt.exe devices
Copy-Item .env.example .env
# Edit .env to add your own ELEVENLABS_API_KEY.
.\target\release\stt.exe listen --output transcript.txt
.\target\release\stt.exe listen --output transcript.jsonl --format jsonl
```

The expected executable is `target\release\stt.exe`. For a future test distribution, include it, this README, and `.env.example`; never include your real `.env` or API key. A Windows executable may receive SmartScreen/Smart App Control warnings depending on signing, reputation, and machine policy. The existing GitHub workflow continues to build macOS only; Windows CI and release publication are not enabled by this change.

## API key and configuration

Create an ElevenLabs API key with speech-to-text access and available quota. Use an environment variable or an optional `.env` file in the working directory (dotenvy can also find one in a parent directory).

```sh
cp .env.example .env
# Edit .env locally to set ELEVENLABS_API_KEY; do not commit the key.
```

Configuration precedence is **CLI arguments → existing environment → .env → defaults**. `.env` never overrides an existing environment variable.

| Variable | Default | Purpose |
| --- | --- | --- |
| `ELEVENLABS_API_KEY` | required for `listen` | Authentication; never printed |
| `STT_AUDIO_DEVICE` | `default` | Source ID from `stt devices` |
| `STT_AUDIO_SOURCE` | `system` | `system`, `microphone`, or `both` |
| `STT_MICROPHONE_DEVICE` | `default` | Input ID from `stt devices --audio-source microphone` |
| `STT_DIARIZE` | `false` | Enable anonymous speaker labels; CLI `--diarize` enables, `--diarize=false` disables |
| `STT_TIMESTAMPS` | `false` | Prefix lines with elapsed recognition time; `--timestamps` enables, `--timestamps=false` disables |
| `STT_CHUNK_MS` | `200` | PCM chunk duration, 100–1,000 ms |
| `STT_VAD_SILENCE` | `1.0` | Server VAD silence, 0.192–3.0 seconds |
| `STT_OUTPUT_FORMAT` | `txt` | `txt` or `jsonl`; JSONL requires an output file |
| `STT_LOG_LEVEL` | `info` | tracing filter, e.g. `warn` or `debug` |
| `STT_LANGUAGE` | automatic detection | Optional two- or three-letter language code |

`devices`, help, and version do not require an API key. Output format defaults to TXT regardless of the filename extension.

## macOS permissions and sources

Run `stt devices` or `stt listen` from a normal logged-in desktop session with a connected display. macOS may request recording permission. In **System Settings → Privacy & Security → Screen & System Audio Recording** (called Screen Recording on older versions), enable the terminal/IDE or `stt`, then fully restart that application. The OS may request permission again after the executable changes. This is normal TCC behavior.

`default` uses the main display's capture scope at startup. Other IDs look like `display:1`. ScreenCaptureKit scopes audio by captured content, not by a selectable speaker output device. A display stream is configured at 2×2 pixels with a one-second minimum video frame interval; video callbacks are immediately discarded. No screen frames are stored. System audio capture does not request microphone access.

If a display disappears or macOS stops the stream, capture exits with an actionable error. The default display is resolved again on reconnect; this version does not automatically switch display scopes during an otherwise healthy session. DRM/protected content and some applications may exclude their audio from capture.

## Windows audio sources and behavior (pending platform validation)

`stt.exe devices` is intended to list `default` plus active playback endpoints with IDs prefixed by `wasapi:`. Copy the entire ID and quote it when selecting a fixed endpoint:

```powershell
.\target\release\stt.exe listen --device default --output transcript.txt
# Replace the quoted value with a complete ID printed by devices:
.\target\release\stt.exe listen --device 'wasapi:<endpoint-id>' --output transcript.txt
```

`default` follows the default **console-role playback endpoint**; this is output capture, not microphone capture. A dedicated COM-initialized worker opens event-driven shared-mode WASAPI loopback. It requests 48 kHz stereo float32 PCM with native audio-engine format conversion enabled, allowing the shared Rust processor to downmix and resample to 16 kHz PCM16. Devices which cannot accept this stream configuration return an error rather than producing placeholder audio. Event-driven loopback requires Windows 10 version 1703 or newer.

The worker copies and releases each native audio packet before queueing it. Conversion and encoding run outside WASAPI. Silent buffers are zero-filled; QPC timestamps align captured packets and synthesized silence. Buffer discontinuities after the first packet, unreliable timestamps, and queue overflow stop capture with an explicit error.

Endpoint and audio-session notifications trigger a bounded reopen when following `default`. The worker reports the gap, closes the previous stream, and retries opening the default endpoint for up to approximately two seconds. Audio during the switch may be lost and is never replayed. An explicitly selected endpoint stays fixed; its removal or session invalidation produces an actionable error instead of silently switching sources. Protected audio, exclusive-mode applications, device drivers, or audio-service configuration may prevent capture.

Ctrl+C uses Windows console signal handling. Partial rendering enables and later restores stderr's virtual-terminal mode when supported; otherwise it falls back to finalized lines. Redirected output remains free of ANSI escapes. These Windows paths require native validation; running from PowerShell or Command Prompt does not validate them merely by compiling the shared code on macOS.

## Usage

Run `stt` without arguments, `stt --help`, or `stt -h` to show the usage guide and common examples. Use `stt listen --help` for every capture and output option. Help does not require an API key or start audio capture.

```sh
stt devices
stt listen
stt listen --output transcript.txt
stt listen --output transcript.jsonl --format jsonl
stt listen --output transcript.txt --overwrite
stt listen --no-partial
stt listen --device default --chunk-ms 200 --vad-silence 1.0
stt listen --language en
stt listen --diarize --output meeting.txt
stt listen --diarize --timestamps --output meeting.txt
stt listen --output meeting.txt --audio-output meeting.wav
stt listen --audio-source microphone --output voice-notes.txt
stt listen --audio-source both --diarize --output meeting.jsonl --format jsonl
stt devices --audio-source microphone
stt listen --help
stt --version
```

On an interactive terminal, the current partial appears on stderr and is replaced as recognition progresses. Committed text becomes permanent stdout lines. Logs also go to stderr. Partial display is disabled when either stream is redirected, for `TERM=dumb`, or with `--no-partial`; redirected streams contain no ANSI escapes.

## Meeting and microphone use cases

The defaults remain **system audio only, no speaker labels**. Capture and diarization are independent options:

| Use case | Options |
| --- | --- |
| Remote participants or a video/podcast | `--audio-source system` |
| Dictation, voice notes, or an in-person meeting through your microphone | `--audio-source microphone` |
| Remote meeting including your own microphone | `--audio-source both` |
| Add anonymous speaker labels to any of these | `--diarize` (alias: `--speaker-detection`) |

Choose an input with `stt devices --audio-source microphone`, then pass its complete quoted `mic:...` ID to `--microphone-device`. `stt devices --audio-source both` lists both kinds; each kind has its own `default`. `--device` selects system output capture; it does not select a microphone. Microphone-only mode does not request macOS Screen Recording permission.

On macOS, microphone/both modes also require **System Settings → Privacy & Security → Microphone** permission for the launching app, then possibly a terminal restart. The executable includes the microphone purpose string. On Windows, check microphone privacy settings for desktop apps. Mic formats of 8–192 kHz, 1–32 channels, and common float/integer PCM are accepted; unsupported formats or native input errors are reported explicitly. The current default microphone is selected when capture starts; reconnect or restart if the microphone changes or disconnects.

In `both` mode the input streams are independently downmixed/resampled, initially aligned to a shared host clock, and averaged at equal gain into one mono stream. Startup audio before both streams are ready is excluded with a warning. Their final tails are drained, padding a finished track with silence. No echo cancellation or active cross-device clock-drift correction is performed. Use **headphones** to keep remote speech from being recorded a second time through your microphone. Independent clocks can drift during long sessions; a stalled track, large timestamp discontinuity, or exceeded buffer limit stops capture explicitly.

## Speaker labels

`--diarize` sends the documented `diarize=true` parameter. Draft labels can appear in partials. Basic commits are held in a bounded buffer until their matching `diarized_transcript` arrives, correlated by `segment_index`; each segment is printed/saved once. A commit containing multiple speakers becomes multiple labeled TXT/stdout lines:

```text
Speaker 1: Can we deploy this Friday?
Speaker 2: Yes, after testing is complete.
```

Add `--timestamps` for elapsed time alongside speaker labels, in both terminal output and TXT files:

```sh
stt listen --diarize --timestamps --output meeting.txt
```

```text
[00:01:23] Speaker 1: Can we deploy this Friday?
[00:01:28] Speaker 2: Yes, after testing is complete.
```

The clock starts when audio capture first starts, includes network reconnect gaps, and resets when you run a new process. It measures **recognition emission time, not the audio position where speech began**: network/VAD/speaker-label delays are included. All speaker turns in one committed segment share its timestamp. Without `--diarize`, timestamped lines contain only `[HH:MM:SS] message`. Partial timestamps reflect their latest update. JSONL keeps its original text and adds a numeric `elapsed_ms` field when timestamps are enabled; the existing UTC `timestamp` field is unchanged. Hours can exceed 23 for long sessions.

These labels distinguish voices; they do **not** identify participant names or automatically identify which speaker is you. Missing or unalignable labels become `Unknown speaker`, preserving the final text. Accuracy can vary with noise and overlapping speech. On disconnect or a finalization timeout, already committed text still waiting for labels is saved as unknown; uncommitted partials are never saved. A speaker-annotation timeout reports an error after preserving final text.

Speaker numbering is allocated across the process and is not reused after reconnecting: `speaker_0` in a new provider session is not assumed to be the same person. JSONL stores the provider's `speaker_session_id`, `segment_index`, and ordered `speakers` spans, including raw `speaker_id`, display `speaker_label`, and span `text`.

The provider can later merge two speaker IDs. Future labels follow chained merges; earlier printed and appended TXT lines remain unchanged, and stderr explains the correction. **With diarization enabled, JSONL additionally appends `event: "speaker_merge"` metadata records** containing the session, IDs, labels, timestamp, and sequence. Consumers should apply these corrections to earlier rows in the same `speaker_session_id` when rendering a corrected history. These rows do not repeat transcript text. Speaker merges can arrive after a transcript; append-only TXT cannot represent retroactive relabeling. [ElevenLabs speaker diarization](https://elevenlabs.io/docs/eleven-api/guides/how-to/speech-to-text/realtime/diarization)

```sh
# stdout contains only committed plain text, even when a structured file is also saved:
stt listen --output transcript.jsonl --format jsonl > finalized.txt
```

## Simultaneous saving

`--output` creates the destination before capture begins and appends by default. It saves finalized text while terminal output continues; with diarization, JSONL also saves final speaker-correction metadata. Partials are never saved. `--overwrite` explicitly truncates and requires `--output`. Missing parent directories are reported rather than created. Existing append files must be readable as well as writable so the CLI can preserve a final newline boundary. TXT keeps one line per commit or speaker turn; embedded line breaks become spaces. JSONL preserves original text using JSON escaping.

```json
{"timestamp":"2026-11-19T09:00:01Z","session_id":"aa5d9199-4539-4ef0-a84b-67f1cb2e5742","sequence":1,"text":"Good morning everyone."}
```

`timestamp` is UTC recognition receipt time, **not audio time**; in speaker mode it is recorded when the diarized result is emitted (or when a missing annotation is flushed as unknown). A new process gets a new UUID; sequence numbers start at 1 and increase across reconnects, including speaker-correction records when enabled. Speaker metadata is omitted by default and word timings are not requested. Appended sessions are distinguished by UUID.

Partial updates and delayed metadata do not append duplicate transcript lines. Indexed events are deduplicated by provider segment within a connection. Identical text in separate commits is preserved as potentially intentional repeated speech; text-only commits cannot reliably distinguish duplicate delivery from repeated speech. Audio is never replayed after reconnecting.

Files flush every second and at shutdown. Flush means userspace data was passed to the OS, not that it is durable against power loss. Do not share the same output file between concurrent CLI processes. Disk errors and stalled output are fatal; the CLI reports the last file sequence successfully flushed.

## Save audio for playback

Use `--audio-output PATH` to record while live transcription and transcript saving continue:

```sh
# Record system audio and save the transcript at the same time:
stt listen --output meeting.txt --audio-output meeting.wav

# Include your microphone, speaker labels, and elapsed transcript timestamps:
stt listen --audio-source both --diarize --timestamps \
  --output meeting.txt --audio-output meeting.wav

# Explicitly replace an existing audio recording:
stt listen --audio-output meeting.wav --overwrite-audio
```

Open the WAV in a normal audio player after stopping. The recording contains the selected source(s), normalized to **16 kHz mono signed 16-bit PCM WAV**, matching the audio fed to transcription. In `both` mode it contains the combined mix, without separate microphone or system tracks. It includes captured silence and untranscribed speech, even when no final transcript is produced. It is intended for speech playback, rather than preserving the original stereo/music quality. Audio uses about **1.92 MB per minute / 115.2 MB per hour**, plus a 44-byte header. No extra recording process or codec installation is needed.

Audio recording is optional and needs `--audio-output`; it still runs alongside online transcription and requires an ElevenLabs API key. It does not add an offline recording mode. Audio files **never append**: an existing destination is refused unless you specify `--overwrite-audio`. This flag affects only the audio; `--overwrite` affects only the transcript. The two destinations must be different files. Use one process per output file and a new recording filename for each session.

A dedicated asynchronous writer uses a bounded two-second PCM buffer, outside native capture callbacks. A slow/full disk or exceeded buffer stops the session with an error. The header and data are flushed at least once per second while disk I/O is responsive; Ctrl+C drains queued recording data and finalizes the header before exit. WAV is limited to 4 GiB (about 37 hours); reaching the limit stops explicitly. A crash, forced kill, disk failure, or shutdown timeout can leave an incomplete file; periodic header checkpoints preserve the last successfully flushed prefix when possible. Flushing does not guarantee durability against power loss.

Capture pauses during connection setup and reconnects. The WAV joins captured portions together and **omits outage gaps**; it is not a continuous wall-clock recording. Some recorded audio may not have reached ElevenLabs before a network failure. Transcript timestamps include reconnect gaps and recognition delay, so they are not precise playback seek positions. The app does not replay the recording automatically.

WAV files contain potentially sensitive meeting audio and are stored unencrypted on your disk. Get appropriate participant consent, protect saved files, and delete them when no longer needed. Generated `*.wav` files are ignored by Git.

## Reliability and shutdown

Native callbacks only retain or copy and enqueue audio buffers. Processing converts native PCM outside callbacks, mixes channels, applies anti-aliased FFT resampling, compensates filter delay, and produces ordered mono 16 kHz PCM16 little-endian chunks. Silence omitted by the backend is synthesized from source-clock timing with a 300 ms delivery allowance. Late frames, large clock jumps, and timestamp gaps are reported; the CLI favors an explicit stop over silently dropping or replaying audio.

Buffering is bounded: approximately one second of native frames per input, two seconds of normalized samples per mixer track, a fixed-size intermediate mixer queue, two seconds queued for transmission, 128 server events, and 128 records per output worker. Speaker tracking retains at most 64 pending commits (512 KiB of text) and 128 speaker/alias entries per provider session. Transcript records are limited to 64 KiB and WebSocket messages to 1 MiB. Partial display retains only the latest value. Capture, network, terminal, and file work run separately. These bounds limit queued data, not total process memory; native framework and TLS allocations are additional.

Network/TLS failures and selected transient API errors retry up to five times using roughly 1, 2, 4, 8, and 16-second delays plus jitter. The retry budget resets after a connection has lasted at least one minute. Capture pauses during recovery. Buffered and uncertain audio is abandoned with a warning, so an outage leaves a gap; previously finalized records remain saved. Authentication, quota, invalid requests, native capture failures, and local queue overflow do not trigger replay or aggressive retry.

Ctrl+C stops capture, drains accepted audio, sends an explicit commit, waits up to ten seconds for final results, closes the WebSocket, and drains output queues for up to five seconds. Native lifecycle operations use the binding's bounded completion wait (normally 30 seconds); an OS permission dialog or uninterruptible OS I/O may take longer to resolve internally. A finalization failure exits nonzero and never promotes a partial to saved text. The API has no explicit end-of-session completion acknowledgement; waiting cannot guarantee results the server did not deliver.

The connection sends WebSocket pings, answers server pings, requests documented silence keepalives, and detects lack of activity. Silence continues to be streamed for server VAD and may be billable.

TLS uses Rustls with the explicitly enabled `ring` crypto provider and the operating system's trust roots. Provider initialization happens before WebSocket connection setup; normal certificate verification remains enabled.

## Troubleshooting

| Symptom | Action |
| --- | --- |
| Permission denied / no display | Grant recording permission to the launching terminal, restart it, and use a logged-in desktop session. |
| No Windows output endpoint / WASAPI failure | Select an active playback device in Windows Sound settings, check the Windows Audio service, and retry `stt.exe devices`. |
| Fixed Windows endpoint disappears | Reconnect it, choose another listed endpoint, or restart with `--device default` to follow output changes. |
| Connected but no speech | Check `--audio-source`: the default captures system output only. For your voice use `microphone` or `both`, check input privacy permission and OS input levels. Initial recognition can take several seconds. |
| Microphone denied or unavailable | Grant microphone access to the launching app, reconnect the input, and use `devices --audio-source microphone` to select it. |
| Missing or changing speaker labels | Labels are anonymous/provisional and may merge. Missing annotations preserve final text as unknown; use JSONL to retain correction events. |
| HTTP 401/403 or `auth_error` | Check the key, its permissions, and account access. |
| Quota or unaccepted-terms error | Check ElevenLabs billing/quota or account terms. |
| Queue full / disk error | Use a responsive output destination; free disk space, check permissions, or remove a stalled pipe consumer, then restart. |
| Network loss | Expect a reported gap and bounded reconnects; uncertain audio is not replayed. |
| Swift/SDK build error | Select a compatible current Xcode installation and ensure compiler caches are writable. |
| Missing Swift runtime at launch | Rebuild with this project's build script; it adds `/usr/lib/swift` to the runtime search path. |

## Development and verification

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release

# Optional real capture check: needs desktop permission and active system audio.
# This test never connects to ElevenLabs or writes captured audio to disk.
cargo test --test macos_capture -- --ignored --nocapture
# Optional microphone-only packet test; requires microphone permission:
cargo test --test microphone_capture -- --ignored --nocapture
```

On Windows, after successfully compiling, run the shared test suite and then the opt-in WASAPI smoke test while playing system audio:

```powershell
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked --test windows_capture -- --ignored --nocapture
```

Before declaring Windows support, verify `devices`, real non-silent loopback capture, TXT and JSONL simultaneous saving, redirected stdout, Unicode, Ctrl+C final flushing, default output switching (including a 44.1 kHz device), fixed-endpoint disconnection, silence, and reconnect behavior on each claimed architecture. The ignored smoke test only checks packet delivery/decoding and stop; it does not prove that non-silent speech is captured or that ElevenLabs transcription works. These Windows checks are intentionally left for platform testing; no Windows build was attempted during implementation.

Automated tests use generated audio, injected output writers, and local mock WebSocket servers. WAV tests verify PCM order, mixed audio and partial tails, header checkpoints, simultaneous transcript/audio flushing, overwrite protection, invalid paths, disk errors, and bounded backpressure. A separate WSS regression test verifies that TLS starts without a crypto-provider panic and that an incomplete TLS handshake returns a connection error. No real API key is needed. The ignored capture test is not counted as real-device verification unless explicitly run successfully. Full transcription additionally requires an actual account/key, permission, and audible speech. CI cannot establish live capture correctness in a headless runner.

Protocol sources: [ElevenLabs realtime API](https://elevenlabs.io/docs/api-reference/speech-to-text/v-1-speech-to-text-realtime), [commit strategies](https://elevenlabs.io/docs/eleven-api/guides/how-to/speech-to-text/realtime/transcripts-and-commit-strategies), and [server-side final commit example](https://elevenlabs.io/docs/eleven-api/guides/how-to/speech-to-text/realtime/server-side-streaming). The implementation uses the WebSocket API directly rather than an SDK or its external recording helpers.

## Security and privacy

The selected system and/or microphone audio is transmitted over TLS to ElevenLabs for processing. Provider account settings and retention policies apply; this CLI does not promise zero retention. Final transcripts are plaintext local files, and optional WAV recordings are unencrypted local audio. API credentials stay in the environment or ignored `.env` and are sent only in the authentication header. The CLI does not log keys, raw audio, or transcript bodies as diagnostic messages, but stdout and selected output files intentionally contain transcript text.

Recording or transcribing meetings may require participant consent. Obtain any necessary permission and protect saved transcripts and API credentials. Only transcribe audio you are authorized to capture.
