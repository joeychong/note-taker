use anyhow::{Result, ensure};
use std::collections::VecDeque;

/// Input tracks share a host-clock origin and are independently resampled to 16 kHz.
/// No track can accumulate more than two seconds while waiting for another track.
pub struct Mixer {
    queues: Vec<VecDeque<i16>>,
    finished: Vec<bool>,
    chunk_samples: usize,
}

impl Mixer {
    pub fn new(tracks: usize, chunk_ms: u32) -> Result<Self> {
        ensure!(
            (1..=2).contains(&tracks),
            "Expected one or two audio tracks"
        );
        Ok(Self {
            queues: vec![VecDeque::new(); tracks],
            finished: vec![false; tracks],
            chunk_samples: chunk_ms as usize * 16,
        })
    }
    pub fn push(&mut self, track: usize, pcm: &[u8]) -> Result<()> {
        let (samples, tail) = pcm.as_chunks::<2>();
        ensure!(tail.is_empty(), "Misaligned mixer PCM");
        ensure!(
            self.queues[track].len() + samples.len() <= 32000,
            "Mixed audio track exceeded two seconds; another input may have stalled"
        );
        self.queues[track].extend(samples.iter().map(|b| i16::from_le_bytes(*b)));
        Ok(())
    }
    pub fn finish(&mut self, track: usize) {
        self.finished[track] = true;
    }
    pub fn chunks(&mut self) -> Vec<Vec<u8>> {
        let mut result = Vec::new();
        loop {
            let all_done = self.finished.iter().all(|done| *done);
            let count = if all_done {
                self.queues
                    .iter()
                    .map(VecDeque::len)
                    .max()
                    .unwrap_or(0)
                    .min(self.chunk_samples)
            } else {
                self.chunk_samples
            };
            if count == 0
                || self
                    .queues
                    .iter()
                    .zip(&self.finished)
                    .any(|(queue, done)| !done && queue.len() < count)
            {
                break;
            }
            let mut bytes = Vec::with_capacity(count * 2);
            for _ in 0..count {
                let sum: i32 = self
                    .queues
                    .iter_mut()
                    .map(|queue| i32::from(queue.pop_front().unwrap_or(0)))
                    .sum();
                bytes.extend_from_slice(&((sum / self.queues.len() as i32) as i16).to_le_bytes());
            }
            result.push(bytes);
        }
        result
    }
}
