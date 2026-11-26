#[cfg(target_os = "macos")]
use super::backend;
use super::{AudioCapture, AudioFrame, AudioPacket, AudioSource, CaptureSession, CapturedPacket};
use anyhow::{Context, Result, anyhow, ensure};
use cpal::{
    SampleFormat, StreamInstant,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc};

#[cfg(target_os = "macos")]
const DEVICE_HELP: &str = "Check System Settings → Sound → Input. Reconnect the microphone or select an available input using stt devices --audio-source microphone and --microphone-device <ID>.";
#[cfg(target_os = "windows")]
const DEVICE_HELP: &str = "Check Settings → System → Sound → Input and test the microphone. Reconnect the microphone or select an available input using stt devices --audio-source microphone and --microphone-device <ID>. If it still fails, check the audio driver and Windows Audio service.";

#[cfg(target_os = "macos")]
const PERMISSION_HELP: &str = "Open System Settings → Privacy & Security → Microphone and allow access for the app launching stt (for example, Terminal). Restart that app after changing permission.";
#[cfg(target_os = "windows")]
const PERMISSION_HELP: &str = "Open Windows microphone privacy settings (Settings → Privacy & security → Microphone on Windows 11). Enable microphone access and Let desktop apps access your microphone. If your Windows build shows a separate permission for this app, allow it too.";

fn microphone_error(operation: &str, error: cpal::Error) -> anyhow::Error {
    let help = if error.kind() == cpal::ErrorKind::PermissionDenied {
        PERMISSION_HELP
    } else {
        DEVICE_HELP
    };
    anyhow!("{operation}: {error}. {help}")
}

#[cfg(any(target_os = "windows", test))]
fn handle_wasapi_microphone_error(error: cpal::Error, errors: &mpsc::Sender<anyhow::Error>) {
    if error.kind() == cpal::ErrorKind::Xrun {
        // WASAPI reports a packet discontinuity through CPAL's error callback
        // and then continues capture. Keep receiving PCM; the timeline fills
        // short timestamp gaps with silence and rejects unsafe clock jumps.
        tracing::warn!(
            "Microphone capture discontinuity; continuing with timestamp-aligned audio. Audio may be missing."
        );
        return;
    }
    let _ = errors.try_send(microphone_error("Microphone stream failed", error));
}

#[cfg(any(target_os = "windows", test))]
fn qpc_microphone_timestamp(
    previous: &mut Option<StreamInstant>,
    stamp: cpal::InputStreamTimestamp,
) -> Result<f64> {
    ensure!(
        previous.is_none_or(|last| stamp.capture >= last),
        "Microphone source clock moved backwards"
    );
    *previous = Some(stamp.capture);
    // CPAL's WASAPI capture instant is GetBuffer's QPC timestamp, already on
    // the same clock as system loopback and backend::host_seconds(). Its
    // callback instant comes from a separate IAudioClock::GetPosition call;
    // do not use that device-clock snapshot to validate or rebase capture time.
    Ok(stamp.capture.as_nanos() as f64 / 1_000_000_000.0)
}

pub fn devices() -> Result<Vec<AudioSource>> {
    let host = cpal::default_host();
    let mut result = Vec::new();
    if let Some(device) = host.default_input_device() {
        result.push(AudioSource {
            id: "default".into(),
            name: format!("Microphone (default: {})", device.description()?.name()),
            is_default: true,
        });
    }
    for device in host.input_devices()? {
        result.push(AudioSource {
            id: format!("mic:{}", device.id()?),
            name: format!("Microphone: {}", device.description()?.name()),
            is_default: false,
        });
    }
    ensure!(
        !result.is_empty(),
        "No microphone input device is available. {DEVICE_HELP}"
    );
    Ok(result)
}

struct MicrophonePacket {
    bytes: Vec<u8>,
    format: SampleFormat,
    rate: u32,
    channels: usize,
    timestamp: f64,
}

/// Native byte decoding is deliberately outside the CPAL realtime callback.
pub fn decode_samples(bytes: &[u8], format: SampleFormat) -> Result<Vec<f32>> {
    macro_rules! decode {
        ($type:ty, $convert:expr) => {{
            let (values, remainder) = bytes.as_chunks::<{ std::mem::size_of::<$type>() }>();
            ensure!(remainder.is_empty(), "Truncated microphone PCM");
            values
                .iter()
                .map(|b| ($convert)(<$type>::from_ne_bytes(*b)))
                .collect()
        }};
    }
    Ok(match format {
        SampleFormat::F32 => decode!(f32, |v| v),
        SampleFormat::F64 => decode!(f64, |v: f64| v as f32),
        SampleFormat::I8 => decode!(i8, |v: i8| f32::from(v) / 128.0),
        SampleFormat::I16 => decode!(i16, |v: i16| f32::from(v) / 32768.0),
        SampleFormat::I32 => decode!(i32, |v: i32| (f64::from(v) / 2147483648.0) as f32),
        SampleFormat::I64 => decode!(i64, |v: i64| (v as f64 / 9223372036854775808.0) as f32),
        SampleFormat::U8 => decode!(u8, |v: u8| (f32::from(v) - 128.0) / 128.0),
        SampleFormat::U16 => decode!(u16, |v: u16| (f32::from(v) - 32768.0) / 32768.0),
        SampleFormat::U32 => decode!(u32, |v: u32| ((f64::from(v) - 2147483648.0) / 2147483648.0)
            as f32),
        SampleFormat::U64 => decode!(u64, |v: u64| ((v as f64 - 9223372036854775808.0)
            / 9223372036854775808.0) as f32),
        _ => anyhow::bail!("Unsupported microphone sample format: {format:?}"),
    })
}

impl AudioPacket for MicrophonePacket {
    fn decode(self: Box<Self>) -> Result<AudioFrame> {
        Ok(AudioFrame {
            sample_rate: self.rate,
            channels: self.channels,
            samples: decode_samples(&self.bytes, self.format)?,
            timestamp: self.timestamp,
        })
    }
}

pub struct PreparedMicrophone {
    stream: Option<cpal::Stream>,
    accepting: Arc<AtomicBool>,
    frames: Option<mpsc::Receiver<CapturedPacket>>,
    errors: Option<mpsc::Receiver<anyhow::Error>>,
    rate: u32,
}

impl PreparedMicrophone {
    /// Build a paused native stream before system capture starts, allowing microphone
    /// permission/configuration errors to surface without accumulating system audio.
    pub fn prepare(selected: &str) -> Result<Self> {
        let host = cpal::default_host();
        let device = if selected == "default" {
            host.default_input_device()
        } else {
            let id = selected.strip_prefix("mic:").context(
                "Expected default or mic:<device-id>; run stt devices --audio-source microphone",
            )?;
            host.device_by_id(&id.parse().context("Invalid microphone device ID")?)
        }
        .with_context(|| format!("Selected microphone is unavailable. {DEVICE_HELP}"))?;
        let supported = device
            .default_input_config()
            .map_err(|error| microphone_error("Cannot read microphone format", error))?;
        let rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let format = supported.sample_format();
        ensure!(
            (8000..=192000).contains(&rate) && (1..=32).contains(&channels),
            "Unsupported microphone rate/channels: {rate}/{channels}"
        );
        decode_samples(&[], format)?;
        let (tx, frames) = mpsc::channel(128);
        let (error_tx, errors) = mpsc::channel(1);
        let callback_errors = error_tx.clone();
        let accepting = Arc::new(AtomicBool::new(false));
        let enabled = accepting.clone();
        let budget = Arc::new(Semaphore::new(rate as usize));
        #[cfg(target_os = "macos")]
        let mut clock: Option<(StreamInstant, f64)> = None;
        #[cfg(target_os = "windows")]
        let mut clock: Option<StreamInstant> = None;
        let stream = device
            .build_input_stream_raw(
                supported.into(),
                format,
                move |data, info| {
                    if !enabled.load(Ordering::Acquire) {
                        return;
                    }
                    let frame_bytes = channels * format.sample_size();
                    let count = data.bytes().len() / frame_bytes;
                    if count == 0 {
                        return;
                    }
                    if count > rate as usize || !data.bytes().len().is_multiple_of(frame_bytes) {
                        let _ = callback_errors
                            .try_send(anyhow!("Invalid or oversized microphone packet"));
                        return;
                    }
                    let stamp = info.timestamp();
                    #[cfg(target_os = "windows")]
                    let timestamp = match qpc_microphone_timestamp(&mut clock, stamp) {
                        Ok(timestamp) => timestamp,
                        Err(error) => {
                            let _ = callback_errors.try_send(error);
                            return;
                        }
                    };
                    #[cfg(target_os = "macos")]
                    let timestamp = {
                        if stamp.callback.checked_duration_since(stamp.capture).is_none()
                            || clock.is_some_and(|(origin, _)| stamp.capture.checked_duration_since(origin).is_none()) {
                            let _ = callback_errors.try_send(anyhow!("Microphone source clock moved backwards or returned inconsistent timestamps"));
                            return;
                        }
                        let (native_origin, host_origin) = *clock.get_or_insert_with(|| {
                            (
                                stamp.capture,
                                backend::host_seconds()
                                    - stamp.callback.duration_since(stamp.capture).as_secs_f64(),
                            )
                        });
                        host_origin + stamp.capture.duration_since(native_origin).as_secs_f64()
                    };
                    match budget.clone().try_acquire_many_owned(count as u32) {
                        Ok(permit) => {
                            let packet = MicrophonePacket {
                                bytes: data.bytes().to_vec(),
                                format,
                                rate,
                                channels,
                                timestamp,
                            };
                            if tx
                                .try_send(CapturedPacket {
                                    packet: Box::new(packet),
                                    permit,
                                })
                                .is_err()
                            {
                                let _ = callback_errors.try_send(anyhow!(
                                    "Microphone queue stalled; audio may be lost"
                                ));
                            }
                        }
                        Err(_) => {
                            let _ = callback_errors.try_send(anyhow!(
                                "Microphone buffer exceeded one second; audio may be lost"
                            ));
                        }
                    }
                },
                move |error| {
                    #[cfg(target_os = "windows")]
                    handle_wasapi_microphone_error(error, &error_tx);
                    #[cfg(target_os = "macos")]
                    let _ = error_tx.try_send(microphone_error("Microphone stream failed", error));
                },
                Some(Duration::from_secs(10)),
            )
            .map_err(|error| microphone_error("Cannot prepare microphone capture", error))?;
        Ok(Self {
            stream: Some(stream),
            accepting,
            frames: Some(frames),
            errors: Some(errors),
            rate,
        })
    }

    pub fn start(mut self) -> Result<CaptureSession> {
        self.accepting.store(true, Ordering::Release);
        self.stream
            .as_ref()
            .expect("prepared stream")
            .play()
            .map_err(|error| microphone_error("Cannot start microphone", error))?;
        Ok(CaptureSession {
            sample_rate: self.rate,
            frames: self.frames.take().expect("prepared receiver"),
            errors: self.errors.take().expect("prepared error receiver"),
            capture: Box::new(MicrophoneCapture {
                stream: self.stream.take(),
                accepting: self.accepting.clone(),
            }),
        })
    }
}

struct MicrophoneCapture {
    stream: Option<cpal::Stream>,
    accepting: Arc<AtomicBool>,
}
impl AudioCapture for MicrophoneCapture {
    fn stop(&mut self) -> Result<()> {
        self.accepting.store(false, Ordering::Release);
        if let Some(stream) = self.stream.take() {
            stream.pause().context("Cannot pause microphone stream")?;
        }
        Ok(())
    }
}
impl Drop for MicrophoneCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::InputStreamTimestamp;

    #[test]
    fn wasapi_xruns_do_not_stop_capture_or_hide_a_later_device_failure() {
        let (errors, mut receiver) = mpsc::channel(1);
        for _ in 0..3 {
            handle_wasapi_microphone_error(cpal::ErrorKind::Xrun.into(), &errors);
            assert!(matches!(
                receiver.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
        handle_wasapi_microphone_error(cpal::ErrorKind::DeviceNotAvailable.into(), &errors);
        let error = receiver.try_recv().unwrap().to_string();
        assert!(error.contains("Microphone stream failed"));
        assert!(error.contains("disconnected"));
    }

    #[test]
    fn wasapi_other_stream_errors_remain_fatal_regardless_of_message() {
        for kind in [
            cpal::ErrorKind::PermissionDenied,
            cpal::ErrorKind::StreamInvalidated,
            cpal::ErrorKind::BackendError,
        ] {
            let (errors, mut receiver) = mpsc::channel(1);
            // Classification must use the error kind rather than match text.
            let error = cpal::Error::with_message(kind, "A buffer underrun or overrun occurred.");
            handle_wasapi_microphone_error(error, &errors);
            assert!(receiver.try_recv().is_ok(), "lost fatal error {kind:?}");
        }
    }

    #[test]
    fn wasapi_capture_uses_shared_qpc_clock_even_when_callback_lags() {
        let mut previous = None;
        // Host uptime, rather than time relative to stream startup. A stale
        // callback clock must neither reject PCM nor offset it from loopback.
        for millis in [123_450, 123_460, 123_480] {
            let stamp = InputStreamTimestamp {
                capture: StreamInstant::from_millis(millis),
                callback: StreamInstant::from_millis(123_440),
            };
            let actual = qpc_microphone_timestamp(&mut previous, stamp).unwrap();
            assert!((actual - millis as f64 / 1000.0).abs() < 1e-9);
        }
    }

    #[test]
    fn wasapi_capture_rejects_regression_between_consecutive_packets() {
        let mut previous = None;
        for millis in [1000, 1020] {
            let stamp = InputStreamTimestamp {
                capture: StreamInstant::from_millis(millis),
                callback: StreamInstant::from_millis(millis + 10),
            };
            qpc_microphone_timestamp(&mut previous, stamp).unwrap();
        }
        let regressed = InputStreamTimestamp {
            capture: StreamInstant::from_millis(1010),
            callback: StreamInstant::from_millis(1040),
        };
        assert!(qpc_microphone_timestamp(&mut previous, regressed).is_err());
        assert_eq!(previous, Some(StreamInstant::from_millis(1020)));
    }
}
