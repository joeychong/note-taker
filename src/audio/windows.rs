//! WASAPI objects are created, used, and released on one COM-initialized thread.
//! Only copied PCM packets cross into the shared processing pipeline.
use super::{AudioCapture, AudioFrame, AudioPacket, AudioSource, CaptureSession, CapturedPacket};
use crate::{audio::converter::decode_f32_le, error::PipelineError};
use ::windows::Win32::{
    Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
        Threading::WaitForSingleObject,
    },
};
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc as sync,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc};
use wasapi::{
    AudioCaptureClient, AudioClient, AudioSessionControl, Device, DeviceEnumerator,
    DeviceEventCallbacks, DeviceState, Direction, EventCallbacks, EventRegistration, Handle, Role,
    SampleType, StreamMode, WasapiError, WaveFormat,
};

const RATE: u32 = 48000;
const CHANNELS: usize = 2;
const BLOCK_ALIGN: usize = CHANNELS * 4;
const DEVICE_HELP: &str = "Check Windows Sound settings and the Windows Audio service; reconnect the output device or select another ID from stt devices.";

/// WASAPI packet timestamps are QPC timestamps expressed in 100 ns units.
/// Windows guarantees these QPC calls succeed on Windows XP and later.
pub fn host_seconds() -> f64 {
    static FREQUENCY: OnceLock<i64> = OnceLock::new();
    let frequency = *FREQUENCY.get_or_init(|| {
        let mut value = 0;
        unsafe { QueryPerformanceFrequency(&mut value) }
            .expect("Windows performance counter unavailable");
        value
    });
    let mut value = 0;
    unsafe { QueryPerformanceCounter(&mut value) }
        .expect("Windows performance counter unavailable");
    value as f64 / frequency as f64
}

struct ComApartment;
impl ComApartment {
    fn enter() -> Result<Self> {
        wasapi::initialize_mta()
            .ok()
            .context("Cannot initialize COM for WASAPI")?;
        Ok(Self)
    }
}
impl Drop for ComApartment {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

pub fn devices() -> Result<Vec<AudioSource>> {
    let _com = ComApartment::enter()?;
    let enumerator = DeviceEnumerator::new().context("Cannot enumerate Windows audio endpoints")?;
    let default_id = enumerator
        .get_default_device(Direction::Render)
        .ok()
        .and_then(|d| d.get_id().ok());
    let mut result = Vec::new();
    if default_id.is_some() {
        result.push(AudioSource {
            id: "default".into(),
            name: "System Audio (default output; follows changes)".into(),
            is_default: true,
        });
    }
    let collection = enumerator.get_device_collection(Direction::Render)?;
    for index in 0..collection.get_nbr_devices()? {
        let device = collection.get_device_at_index(index)?;
        let id = device.get_id()?;
        result.push(AudioSource {
            id: format!("wasapi:{id}"),
            name: format!(
                "{}{}",
                device.get_friendlyname()?,
                if Some(&id) == default_id.as_ref() {
                    " [current default output]"
                } else {
                    ""
                }
            ),
            is_default: false,
        });
    }
    ensure!(
        !result.is_empty(),
        "No active Windows output endpoint is available. {DEVICE_HELP}"
    );
    Ok(result)
}

struct WindowsPacket {
    bytes: Vec<u8>,
    timestamp: f64,
}
impl AudioPacket for WindowsPacket {
    fn decode(self: Box<Self>) -> Result<AudioFrame> {
        ensure!(
            self.bytes.len().is_multiple_of(BLOCK_ALIGN),
            "Misaligned WASAPI stereo packet"
        );
        Ok(AudioFrame {
            sample_rate: RATE,
            channels: CHANNELS,
            samples: decode_f32_le(&self.bytes)?,
            timestamp: self.timestamp,
        })
    }
}

struct Loopback {
    // Unregister callbacks before releasing the session/client and before COM teardown.
    _registration: EventRegistration,
    _session: AudioSessionControl,
    capture: AudioCaptureClient,
    event: Handle,
    client: AudioClient,
    device: Device,
    id: String,
    buffer: Vec<u8>,
    seen_packet: bool,
    running: bool,
}

impl Loopback {
    fn open(device: Device, invalidated: Arc<AtomicBool>) -> Result<Self> {
        ensure!(
            device.get_direction() == Direction::Render,
            "Selected endpoint is not an output device; run stt devices"
        );
        ensure!(
            device.get_state()? == DeviceState::Active,
            "Selected output device is inactive. {DEVICE_HELP}"
        );
        let id = device.get_id()?;
        let mut client = device.get_iaudioclient()?;
        // Let the native shared audio engine convert the endpoint's mix format.
        // Both backends then feed the same 48 kHz -> 16 kHz processing pipeline.
        let format = WaveFormat::new(32, 32, SampleType::Float, RATE as usize, CHANNELS, None);
        let (period, _) = client.get_device_period()?;
        client.initialize_client(&format, Direction::Capture,
            StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: period }, None)
            .context("Cannot initialize 48 kHz stereo shared-mode WASAPI loopback; Windows 10 version 1703+ or Windows 11 is required")?;
        let event = client.set_get_eventhandle()?;
        let frames = client.get_buffer_size()? as usize;
        ensure!(
            (1..=RATE as usize).contains(&frames),
            "WASAPI returned an invalid or oversized capture buffer: {frames} frames"
        );
        let capture = client.get_audiocaptureclient()?;
        let session = client.get_audiosessioncontrol()?;
        let mut callbacks = EventCallbacks::new();
        callbacks.set_disconnected_callback(move |_| {
            invalidated.store(true, Ordering::Release);
        });
        let registration = session.register_session_notification(callbacks)?;
        let mut stream = Self {
            _registration: registration,
            _session: session,
            capture,
            event,
            client,
            device,
            id,
            buffer: vec![0; frames * BLOCK_ALIGN],
            seen_packet: false,
            running: false,
        };
        stream
            .client
            .start_stream()
            .context("Cannot start WASAPI loopback")?;
        stream.running = true;
        Ok(stream)
    }

    fn read_available(
        &mut self,
        tx: &mpsc::Sender<CapturedPacket>,
        budget: &Arc<Semaphore>,
        stop: &AtomicBool,
    ) -> Result<()> {
        // Drain only a bounded amount per iteration so device changes and stop
        // requests cannot be starved by a continuously producing endpoint.
        for _ in 0..128 {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let frames = self
                .capture
                .get_next_packet_size()?
                .context("Loopback unexpectedly left shared mode")?;
            if frames == 0 {
                break;
            }
            ensure!(
                frames as usize * BLOCK_ALIGN <= self.buffer.len(),
                "WASAPI packet exceeds the negotiated buffer"
            );
            let permit = budget
                .clone()
                .try_acquire_many_owned(frames)
                .map_err(|_| PipelineError::Backpressure("native audio (one second)"))?;
            let (read, info) = self.capture.read_from_device(&mut self.buffer)?;
            ensure!(
                read == frames,
                "WASAPI changed packet size during a read; stopping to avoid losing audio"
            );
            ensure!(
                !info.flags.timestamp_error,
                "WASAPI reported an unreliable audio timestamp. {DEVICE_HELP}"
            );
            // The initial packet may carry a discontinuity flag at stream startup.
            ensure!(
                !self.seen_packet || !info.flags.data_discontinuity,
                "WASAPI reported a capture discontinuity; audio may have been lost. {DEVICE_HELP}"
            );
            self.seen_packet = true;
            // read_from_device already zeroes SILENT buffers, including null data pointers.
            let packet = WindowsPacket {
                bytes: self.buffer[..read as usize * BLOCK_ALIGN].to_vec(),
                timestamp: info.timestamp as f64 / 10_000_000.0,
            };
            tx.try_send(CapturedPacket {
                packet: Box::new(packet),
                permit,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => PipelineError::Backpressure("native audio"),
                mpsc::error::TrySendError::Closed(_) => {
                    PipelineError::OutputClosed("audio processing")
                }
            })?;
        }
        Ok(())
    }

    fn wait(&self) -> Result<()> {
        // A finite native wait also services silence, device changes, and shutdown.
        match unsafe { WaitForSingleObject(self.event.as_handle(), 100) } {
            WAIT_OBJECT_0 | WAIT_TIMEOUT => Ok(()),
            WAIT_FAILED => Err(std::io::Error::last_os_error()).context("WASAPI event wait failed"),
            status => anyhow::bail!("Unexpected WASAPI wait status: {status:?}"),
        }
    }

    fn stop(&mut self) -> Result<()> {
        if self.running {
            self.running = false;
            self.client
                .stop_stream()
                .context("Cannot stop WASAPI stream")?;
        }
        Ok(())
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::warn!("WASAPI stream cleanup failed: {error:#}");
        }
    }
}

fn select_device(enumerator: &DeviceEnumerator, selected: &str) -> Result<Device> {
    if selected == "default" {
        enumerator
            .get_default_device(Direction::Render)
            .with_context(|| format!("No default output device is available. {DEVICE_HELP}"))
    } else {
        let id = selected
            .strip_prefix("wasapi:")
            .filter(|id| !id.is_empty())
            .context("Expected default or wasapi:<endpoint-id>; run stt devices")?;
        enumerator
            .get_device(id)
            .with_context(|| format!("Selected output endpoint is unavailable. {DEVICE_HELP}"))
    }
}

fn invalidated_device(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<WasapiError>(), Some(WasapiError::Windows(error))
        if matches!(error.code().0 as u32, 0x8889_0004 | 0x8889_0026))
}

fn reopen(
    enumerator: &DeviceEnumerator,
    selected: &str,
    invalidated: &Arc<AtomicBool>,
    stop: &AtomicBool,
) -> Result<Option<Loopback>> {
    let mut last_error = None;
    // Endpoint changes can briefly leave no default device. Wait at most two
    // seconds, with an interruptible 100 ms cadence and an explicitly reported gap.
    for attempt in 0..20 {
        if stop.load(Ordering::Acquire) {
            return Ok(None);
        }
        invalidated.store(false, Ordering::Release);
        match select_device(enumerator, selected)
            .and_then(|device| Loopback::open(device, invalidated.clone()))
        {
            Ok(stream) => return Ok(Some(stream)),
            Err(error) => last_error = Some(error),
        }
        if attempt < 19 {
            thread::sleep(Duration::from_millis(100));
        }
    }
    Err(last_error.expect("at least one open attempt"))
        .context("Could not resume loopback after the output device changed")
}

fn capture_loop(
    selected: &str,
    stop: &AtomicBool,
    tx: &mpsc::Sender<CapturedPacket>,
    ready: &sync::SyncSender<Result<()>>,
) -> Result<()> {
    let _com = ComApartment::enter()?;
    let enumerator = DeviceEnumerator::new()?;
    let changed = Arc::new(AtomicBool::new(false));
    let invalidated = Arc::new(AtomicBool::new(false));
    let mut callbacks = DeviceEventCallbacks::new();
    let flag = changed.clone();
    callbacks.set_default_device_callback(move |direction, role, _| {
        if direction == Direction::Render && role == Role::Console {
            flag.store(true, Ordering::Release);
        }
    });
    let flag = changed.clone();
    callbacks.set_device_state_callback(move |_, _| {
        flag.store(true, Ordering::Release);
    });
    let flag = changed.clone();
    callbacks.set_device_removed_callback(move |_| {
        flag.store(true, Ordering::Release);
    });
    let _notifications = enumerator.register_notification_callback(callbacks)?;
    let mut stream = Some(Loopback::open(
        select_device(&enumerator, selected)?,
        invalidated.clone(),
    )?);
    let budget = Arc::new(Semaphore::new(RATE as usize));
    if ready.send(Ok(())).is_err() {
        return Ok(());
    }
    while !stop.load(Ordering::Acquire) {
        let current = stream.as_mut().expect("active stream");
        let session_invalid = invalidated.swap(false, Ordering::AcqRel);
        let topology_changed = changed.swap(false, Ordering::AcqRel);
        let default_changed = if selected == "default" && topology_changed {
            match enumerator
                .get_default_device(Direction::Render)
                .and_then(|d| d.get_id())
            {
                Ok(id) => id != current.id,
                Err(_) => true,
            }
        } else {
            false
        };
        if selected != "default" && (session_invalid || topology_changed) {
            ensure!(
                !session_invalid && current.device.get_state()? == DeviceState::Active,
                "Selected output endpoint disconnected or its audio session was invalidated. {DEVICE_HELP}"
            );
        }
        let result = if session_invalid || default_changed {
            Ok(false)
        } else {
            current
                .read_available(tx, &budget, stop)
                .and_then(|()| current.wait())
                .map(|()| true)
        };
        match result {
            Ok(true) => (),
            Ok(false) => {
                tracing::warn!(
                    "Default output device/session changed; restarting WASAPI loopback. Audio during this gap may be lost; no audio is replayed."
                );
                drop(stream.take());
                stream = reopen(&enumerator, selected, &invalidated, stop)?;
                if stream.is_none() {
                    break;
                }
            }
            Err(error) if selected == "default" && invalidated_device(&error) => {
                tracing::warn!(
                    "Output device was invalidated; reopening default loopback. Audio during this gap may be lost; no audio is replayed."
                );
                drop(stream.take());
                stream = reopen(&enumerator, selected, &invalidated, stop)?;
                if stream.is_none() {
                    break;
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Windows system audio capture failed. {DEVICE_HELP}")
                });
            }
        }
    }
    if let Some(stream) = stream.as_mut() {
        stream.stop()?;
    }
    Ok(())
}

struct WindowsCapture {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    completed: sync::Receiver<Result<()>>,
}
impl AudioCapture for WindowsCapture {
    fn stop(&mut self) -> Result<()> {
        if self.worker.is_none() {
            return Ok(());
        }
        self.stop.store(true, Ordering::Release);
        let result = self.completed.recv_timeout(Duration::from_secs(5))
            .context("WASAPI worker did not stop within five seconds; the audio driver may be unresponsive")?;
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow!("WASAPI worker panicked"))?;
        }
        result
    }
}
impl Drop for WindowsCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub fn start(device: &str) -> Result<CaptureSession> {
    let selected = device.to_owned();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let (tx, frames) = mpsc::channel(128);
    let (error_tx, errors) = mpsc::channel(1);
    let (ready_tx, ready_rx) = sync::sync_channel(1);
    let (completed_tx, completed) = sync::channel();
    let worker = thread::Builder::new()
        .name("wasapi-loopback".into())
        .spawn(move || {
            let result = capture_loop(&selected, &worker_stop, &tx, &ready_tx)
                .with_context(|| format!("Windows system audio capture failed. {DEVICE_HELP}"));
            if let Err(error) = &result {
                let _ = ready_tx.try_send(Err(anyhow!("{error:#}")));
                let _ = error_tx.try_send(anyhow!("{error:#}"));
            }
            let _ = completed_tx.send(result);
        })
        .context("Cannot start the WASAPI worker thread")?;
    let mut capture = WindowsCapture {
        stop,
        worker: Some(worker),
        completed,
    };
    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => Ok(CaptureSession {
            sample_rate: RATE,
            capture: Box::new(capture),
            frames,
            errors,
        }),
        Ok(Err(error)) => {
            let _ = capture.stop();
            Err(error)
        }
        Err(error) => {
            let _ = capture.stop();
            Err(error).context("WASAPI startup failed or timed out")
        }
    }
}
