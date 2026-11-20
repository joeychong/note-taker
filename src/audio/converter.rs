use anyhow::{Result, ensure};
use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use std::collections::VecDeque;

/// Decode packed little-endian float PCM outside the native capture worker.
pub fn decode_f32_le(bytes: &[u8]) -> Result<Vec<f32>> {
    let (samples, remainder) = bytes.as_chunks::<4>();
    ensure!(remainder.is_empty(), "Truncated float32 PCM sample");
    Ok(samples
        .iter()
        .map(|sample| f32::from_le_bytes(*sample))
        .collect())
}

pub fn downmix(samples: &[f32], channels: usize, out: &mut VecDeque<f32>) -> Result<()> {
    ensure!(
        channels > 0 && channels <= 32 && samples.len().is_multiple_of(channels),
        "Invalid audio channel layout"
    );
    for frame in samples.chunks_exact(channels) {
        // Equal channel gain; do not amplify a multichannel signal. Non-finite native samples are silence.
        let sum: f64 = frame
            .iter()
            .map(|&x| if x.is_finite() { f64::from(x) } else { 0.0 })
            .sum();
        out.push_back((sum / channels as f64) as f32);
    }
    Ok(())
}

pub fn pcm16(sample: f32) -> [u8; 2] {
    let value = if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    ((value * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes()
}

/// Streaming conversion with delay compensation and an exact, unpadded final duration.
pub struct Converter {
    rate: u32,
    resampler: Option<Fft<f32>>,
    pending: VecDeque<f32>,
    input: Vec<f32>,
    output: Vec<f32>,
    delay: usize,
    total_input: usize,
    total_output: usize,
    pcm: VecDeque<u8>,
    chunk_bytes: usize,
}

impl Converter {
    pub fn new(rate: u32, chunk_ms: u32) -> Result<Self> {
        ensure!(
            (8000..=192000).contains(&rate),
            "Unsupported sample rate {rate}"
        );
        ensure!(
            (100..=1000).contains(&chunk_ms),
            "Chunk duration must be 100–1000 ms"
        );
        let resampler = if rate == 16000 {
            None
        } else {
            Some(Fft::new(rate as usize, 16000, 1024, 1, FixedSync::Both)?)
        };
        let delay = resampler.as_ref().map_or(0, Resampler::output_delay);
        let input_len = resampler
            .as_ref()
            .map_or(1024, Resampler::input_frames_next);
        let output_len = resampler
            .as_ref()
            .map_or(1024, Resampler::output_frames_max);
        Ok(Self {
            rate,
            resampler,
            pending: VecDeque::new(),
            input: vec![0.0; input_len],
            output: vec![0.0; output_len],
            delay,
            total_input: 0,
            total_output: 0,
            pcm: VecDeque::with_capacity(chunk_ms as usize * 32),
            chunk_bytes: chunk_ms as usize * 32,
        })
    }

    pub fn push(&mut self, samples: &[f32], channels: usize) -> Result<Vec<Vec<u8>>> {
        downmix(samples, channels, &mut self.pending)?;
        self.total_input += samples.len() / channels;
        if self.resampler.is_none() {
            while let Some(sample) = self.pending.pop_front() {
                self.pcm.extend(pcm16(sample));
                self.total_output += 1;
            }
        } else {
            while self.pending.len() >= self.input.len() {
                self.process(false)?;
            }
        }
        Ok(self.chunks(false))
    }

    fn process(&mut self, finishing: bool) -> Result<()> {
        for value in &mut self.input {
            *value = self.pending.pop_front().unwrap_or(0.0);
        }
        let input = InterleavedSlice::new(&self.input, 1, self.input.len())?;
        let output_len = self.output.len();
        let mut output = InterleavedSlice::new_mut(&mut self.output, 1, output_len)?;
        let (_, written) = self
            .resampler
            .as_mut()
            .expect("resampling enabled")
            .process_into_buffer(&input, &mut output, None)?;
        let expected = self.total_input * 16000 / self.rate as usize;
        for &sample in &self.output[..written] {
            if self.delay > 0 {
                self.delay -= 1;
            } else if !finishing || self.total_output < expected {
                self.pcm.extend(pcm16(sample));
                self.total_output += 1;
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<Vec<u8>>> {
        let expected = self.total_input * 16000 / self.rate as usize;
        while self.total_output < expected && self.resampler.is_some() {
            self.process(true)?;
        }
        Ok(self.chunks(true))
    }

    fn chunks(&mut self, final_tail: bool) -> Vec<Vec<u8>> {
        let mut result = Vec::new();
        while self.pcm.len() >= self.chunk_bytes || (final_tail && !self.pcm.is_empty()) {
            let count = self.chunk_bytes.min(self.pcm.len());
            result.push(self.pcm.drain(..count).collect());
        }
        result
    }
}
