use super::{AudioCapture, AudioFrame, AudioPacket, AudioSource, CaptureSession, CapturedPacket};
use anyhow::{Context, Result, anyhow, ensure};
use screencapturekit::{cm::CMClock, prelude::*, stream::delegate_trait::ErrorHandler};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Semaphore, mpsc};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

const PERMISSION: &str = "Allow your terminal (or stt) in System Settings → Privacy & Security → Screen & System Audio Recording, then restart it. A connected display and logged-in desktop session are required.";

pub fn host_seconds() -> f64 {
    CMClock::host_time_clock()
        .time()
        .as_seconds()
        .unwrap_or(0.0)
}

fn content() -> Result<SCShareableContent> {
    // Explicitly request TCC permission only when a capture command is invoked.
    if !unsafe { CGPreflightScreenCaptureAccess() } && !unsafe { CGRequestScreenCaptureAccess() } {
        anyhow::bail!("System audio permission denied. {PERMISSION}");
    }
    SCShareableContent::get()
        .with_context(|| format!("Unable to enumerate ScreenCaptureKit sources. {PERMISSION}"))
}

pub fn devices() -> Result<Vec<AudioSource>> {
    let content = content()?;
    let main = unsafe { CGMainDisplayID() };
    let mut result = vec![AudioSource {
        id: "default".into(),
        name: "System Audio (main display scope)".into(),
        is_default: true,
    }];
    for display in content.displays() {
        result.push(AudioSource {
            id: format!("display:{}", display.display_id()),
            name: format!(
                "System Audio via display {} ({}×{}){}",
                display.display_id(),
                display.width(),
                display.height(),
                if display.display_id() == main {
                    "; main"
                } else {
                    ""
                }
            ),
            is_default: false,
        });
    }
    ensure!(result.len() > 1, "No display is available. {PERMISSION}");
    Ok(result)
}

struct MacPacket(CMSampleBuffer);

impl AudioPacket for MacPacket {
    fn decode(self: Box<Self>) -> Result<AudioFrame> {
        let sample = &self.0;
        ensure!(
            sample.is_valid() && sample.data_is_ready(),
            "Invalid native audio buffer"
        );
        let format = sample
            .format_description()
            .context("Missing native audio format")?;
        ensure!(
            format.media_subtype_raw() == u32::from_be_bytes(*b"lpcm"),
            "Native audio must be linear PCM"
        );
        let rate = format.audio_sample_rate().context("Missing sample rate")?;
        ensure!(
            rate == 48000.0,
            "ScreenCaptureKit changed the requested 48 kHz format to {rate}; restart capture"
        );
        let channels = format.audio_channel_count().context("Missing channels")? as usize;
        ensure!(
            (1..=32).contains(&channels),
            "Unsupported channel count {channels}"
        );
        let bits = format
            .audio_bits_per_channel()
            .context("Missing sample width")?;
        let float = format.audio_is_float();
        let big = format.audio_is_big_endian();
        let signed = format.audio_format_flags().unwrap_or(0) & 4 != 0;
        ensure!(
            (float && matches!(bits, 32 | 64)) || (signed && matches!(bits, 16 | 32)),
            "Unsupported native sample format: {bits} bits, float={float}"
        );
        let frames = sample.num_samples() as usize;
        let buffers = sample
            .audio_buffer_list()
            .map_err(|code| anyhow!("Unable to access native PCM: OSStatus {code}"))?;
        let mut samples = vec![0.0; frames * channels];
        let mut channel_offset = 0;
        for buffer in &buffers {
            let count = buffer.number_channels as usize;
            let width = bits as usize / 8;
            ensure!(
                count > 0 && channel_offset + count <= channels,
                "Invalid native PCM layout"
            );
            let data = buffer.data();
            ensure!(
                data.len() == frames * count * width,
                "Truncated native audio buffer"
            );
            for (index, bytes) in data.chunks_exact(width).enumerate() {
                let value = match (float, bits) {
                    (true, 32) => {
                        let b = bytes.try_into()?;
                        if big {
                            f32::from_be_bytes(b)
                        } else {
                            f32::from_le_bytes(b)
                        }
                    }
                    (true, 64) => {
                        let b = bytes.try_into()?;
                        (if big {
                            f64::from_be_bytes(b)
                        } else {
                            f64::from_le_bytes(b)
                        }) as f32
                    }
                    (false, 16) => {
                        let b = bytes.try_into()?;
                        f32::from(if big {
                            i16::from_be_bytes(b)
                        } else {
                            i16::from_le_bytes(b)
                        }) / 32768.0
                    }
                    (false, 32) => {
                        let b = bytes.try_into()?;
                        (if big {
                            i32::from_be_bytes(b)
                        } else {
                            i32::from_le_bytes(b)
                        }) as f32
                            / 2147483648.0
                    }
                    _ => unreachable!(),
                };
                samples[(index / count) * channels + channel_offset + index % count] = value;
            }
            channel_offset += count;
        }
        ensure!(channel_offset == channels, "Missing native audio channels");
        let timestamp = sample
            .presentation_timestamp()
            .as_seconds()
            .context("Missing native audio timestamp")?;
        Ok(AudioFrame {
            sample_rate: rate as u32,
            channels,
            samples,
            timestamp,
        })
    }
}

struct MacCapture {
    stream: SCStream,
    accepting: Arc<AtomicBool>,
    stopped: bool,
}

impl AudioCapture for MacCapture {
    fn stop(&mut self) -> Result<()> {
        if !self.stopped {
            self.accepting.store(false, Ordering::Release);
            self.stream
                .stop_capture()
                .context("ScreenCaptureKit failed to stop")?;
            self.stopped = true;
        }
        Ok(())
    }
}

impl Drop for MacCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub fn start(device: &str) -> Result<CaptureSession> {
    let content = content()?;
    let displays = content.displays();
    let display_id = if device == "default" {
        unsafe { CGMainDisplayID() }
    } else {
        device
            .strip_prefix("display:")
            .context("Unknown audio source; run stt devices")?
            .parse::<u32>()
            .context("Expected display:<id>; run stt devices")?
    };
    let display = displays
        .iter()
        .find(|d| d.display_id() == display_id)
        .context("Selected display is unavailable; run stt devices")?;
    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build()?;
    let config = SCStreamConfiguration::new()
        .with_width(2)
        .with_height(2)
        .with_shows_cursor(false)
        .with_queue_depth(3)
        .with_minimum_frame_interval(&CMTime::new(1, 1))
        .with_captures_audio(true)
        .with_sample_rate(48000)
        .with_channel_count(2);
    let (tx, frames) = mpsc::channel(128);
    let (error_tx, errors) = mpsc::channel(1);
    let budget = Arc::new(Semaphore::new(48000));
    let accepting = Arc::new(AtomicBool::new(true));
    let delegate_tx = error_tx.clone();
    let delegate = ErrorHandler::new(move |error| {
        let _ = delegate_tx.try_send(anyhow!("ScreenCaptureKit stopped: {error}. {PERMISSION}"));
    });
    let mut stream = SCStream::new_with_delegate(&filter, &config, delegate)?;
    let callback_accepting = accepting.clone();
    stream.add_output_handler(
        move |sample: CMSampleBuffer, kind| {
            if kind != SCStreamOutputType::Audio || !callback_accepting.load(Ordering::Acquire) {
                return;
            }
            let count = sample.num_samples();
            if !(1..=48000).contains(&count) {
                let _ = error_tx.try_send(anyhow!(
                    "Invalid or oversized native audio packet: {count} frames"
                ));
                return;
            }
            match budget.clone().try_acquire_many_owned(count as u32) {
                Ok(permit) => {
                    if tx
                        .try_send(CapturedPacket {
                            packet: Box::new(MacPacket(sample)),
                            permit,
                        })
                        .is_err()
                    {
                        let _ = error_tx.try_send(
                            crate::error::PipelineError::Backpressure("native audio").into(),
                        );
                    }
                }
                Err(_) => {
                    let _ = error_tx.try_send(
                        crate::error::PipelineError::Backpressure("native audio (one second)")
                            .into(),
                    );
                }
            }
        },
        SCStreamOutputType::Audio,
    )?;
    // Register a cheap video sink so display-stream implementations never accumulate video.
    stream.add_output_handler(|_: CMSampleBuffer, _| {}, SCStreamOutputType::Screen)?;
    stream
        .start_capture()
        .with_context(|| format!("Cannot start system audio capture. {PERMISSION}"))?;
    Ok(CaptureSession {
        sample_rate: 48000,
        capture: Box::new(MacCapture {
            stream,
            accepting,
            stopped: false,
        }),
        frames,
        errors,
    })
}
