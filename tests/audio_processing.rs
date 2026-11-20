use note_taker::audio::{
    AudioFrame,
    converter::{Converter, decode_f32_le, downmix, pcm16},
    pipeline::Timeline,
};
use std::collections::VecDeque;

fn convert(rate: u32, samples: &[f32], channels: usize, block_frames: usize) -> Vec<u8> {
    let mut converter = Converter::new(rate, 200).unwrap();
    let mut bytes = Vec::new();
    for block in samples.chunks(block_frames * channels) {
        for chunk in converter.push(block, channels).unwrap() {
            assert_eq!(chunk.len(), 6400);
            bytes.extend(chunk);
        }
    }
    for chunk in converter.finish().unwrap() {
        assert!(chunk.len() <= 6400);
        bytes.extend(chunk);
    }
    bytes
}

#[test]
fn pcm_endianness_clipping_and_nonfinite_samples() {
    assert_eq!(pcm16(1.0), 32767_i16.to_le_bytes());
    assert_eq!(pcm16(-1.0), (-32768_i16).to_le_bytes());
    assert_eq!(pcm16(2.0), pcm16(1.0));
    assert_eq!(pcm16(f32::NAN), [0, 0]);
    assert_eq!(pcm16(0.5), [0, 64]);
}

#[test]
fn decodes_windows_float_pcm_without_endianness_or_sample_loss() {
    let expected = [0.0_f32, 0.5, -0.5, 1.0, -1.0];
    let bytes: Vec<_> = expected
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
    assert_eq!(decode_f32_le(&bytes).unwrap(), expected);
    assert!(decode_f32_le(&bytes[..bytes.len() - 1]).is_err());
    assert!(decode_f32_le(&[]).unwrap().is_empty());
}

#[test]
fn mixes_stereo_and_multiple_channels_without_gain() {
    let mut mono = VecDeque::new();
    downmix(&[1.0, -1.0, 0.25, 0.75], 2, &mut mono).unwrap();
    assert_eq!(mono.into_iter().collect::<Vec<_>>(), vec![0.0, 0.5]);
    let mut mono = VecDeque::new();
    downmix(&[1.0; 6], 6, &mut mono).unwrap();
    assert_eq!(mono[0], 1.0);
    assert!(downmix(&[1.0], 2, &mut mono).is_err());
}

#[test]
fn exact_duration_and_callback_boundary_independence() {
    for rate in [16000, 44100, 48000] {
        // An odd length tests a real final tail, rather than padded silence.
        let frames = rate as usize + 123;
        let samples: Vec<_> = (0..frames).map(|i| (i as f32 * 0.1).sin() * 0.5).collect();
        let reference = convert(rate, &samples, 1, 97);
        assert_eq!(reference.len(), frames * 16000 / rate as usize * 2);
        assert_eq!(reference, convert(rate, &samples, 1, 2137));
    }
    assert!(convert(48000, &[], 1, 97).is_empty());
}

fn tone_rms(frequency: f32) -> f32 {
    let samples: Vec<_> = (0..48000)
        .map(|i| (i as f32 * std::f32::consts::TAU * frequency / 48000.0).sin() * 0.5)
        .collect();
    let bytes = convert(48000, &samples, 1, 512);
    let signal: Vec<_> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
        .collect();
    (signal[1000..15000].iter().map(|x| x * x).sum::<f32>() / 14000.0).sqrt()
}

#[test]
fn resampler_preserves_speech_band_and_rejects_aliasing() {
    assert!((tone_rms(1000.0) - 0.3535).abs() < 0.02);
    assert!(tone_rms(12000.0) < 0.005);
}

#[test]
fn silent_timeline_and_late_audio_are_explicit() {
    let mut timeline = Timeline::new(200, 100.0).unwrap();
    assert!(timeline.idle(100.2).unwrap().is_empty());
    assert!(!timeline.idle(100.9).unwrap().is_empty());
    let frame = AudioFrame {
        sample_rate: 48000,
        channels: 1,
        samples: vec![0.0; 480],
        timestamp: 100.0,
    };
    assert!(
        timeline
            .push(frame)
            .unwrap_err()
            .to_string()
            .contains("already sent")
    );
    assert!(timeline.idle(110.0).is_err());
}

#[test]
fn mixer_aligns_tracks_averages_without_clipping_and_flushes_a_short_tail() {
    use note_taker::audio::mixer::Mixer;
    let mut mixer = Mixer::new(2, 100).unwrap();
    let system: Vec<_> = vec![10000_i16; 1603]
        .into_iter()
        .flat_map(i16::to_le_bytes)
        .collect();
    let microphone: Vec<_> = vec![20000_i16; 1600]
        .into_iter()
        .flat_map(i16::to_le_bytes)
        .collect();
    mixer.push(0, &system).unwrap();
    assert!(mixer.chunks().is_empty());
    mixer.push(1, &microphone).unwrap();
    let chunks = mixer.chunks();
    assert_eq!(chunks.len(), 1);
    assert!(
        chunks[0]
            .as_chunks::<2>()
            .0
            .iter()
            .all(|b| i16::from_le_bytes(*b) == 15000)
    );
    mixer.finish(0);
    mixer.finish(1);
    let tail = mixer.chunks();
    assert_eq!(
        tail[0],
        vec![5000_i16; 3]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect::<Vec<_>>()
    );
    assert!(mixer.chunks().is_empty());
}

#[test]
fn mixer_single_track_has_unity_gain_and_reports_backpressure() {
    use note_taker::audio::mixer::Mixer;
    let mut single = Mixer::new(1, 100).unwrap();
    let bytes: Vec<_> = vec![32767_i16; 1600]
        .into_iter()
        .flat_map(i16::to_le_bytes)
        .collect();
    single.push(0, &bytes).unwrap();
    assert_eq!(single.chunks(), vec![bytes]);
    let mut mixed = Mixer::new(2, 100).unwrap();
    mixed.push(0, &vec![0; 64000]).unwrap();
    assert!(mixed.push(0, &[0, 0]).is_err());
    assert!(mixed.push(1, &[0]).is_err());
}

#[test]
fn microphone_pcm_decoding_and_clock_alignment_preserve_duration() {
    use cpal::SampleFormat;
    use note_taker::audio::microphone::decode_samples;
    let pcm: Vec<_> = [-32768_i16, 0, 16384, 32767]
        .into_iter()
        .flat_map(i16::to_ne_bytes)
        .collect();
    assert_eq!(
        decode_samples(&pcm, SampleFormat::I16).unwrap(),
        vec![-1.0, 0.0, 0.5, 32767.0 / 32768.0]
    );
    assert!(decode_samples(&[0], SampleFormat::I16).is_err());
    assert_eq!(
        decode_samples(&[0, 128], SampleFormat::U16).unwrap(),
        vec![0.0]
    );
    let mut timeline = Timeline::at_rate(44100, 200, 100.0, true).unwrap();
    let mut chunks = timeline
        .push(AudioFrame {
            sample_rate: 44100,
            channels: 1,
            samples: vec![0.5; 44100],
            timestamp: 100.1,
        })
        .unwrap();
    chunks.extend(timeline.finish().unwrap());
    let bytes: Vec<_> = chunks.into_iter().flatten().collect();
    assert_eq!(bytes.len(), 35200); // 100 ms of initial silence, then one second of speech.
    assert!(bytes[..1600].iter().all(|b| *b == 0));
}

#[test]
fn mixed_native_rates_stay_ordered_and_drain_to_the_same_audio_duration() {
    use note_taker::audio::mixer::Mixer;
    let mut tracks = [
        Timeline::at_rate(44100, 200, 100.0, true).unwrap(),
        Timeline::at_rate(48000, 200, 100.0, true).unwrap(),
    ];
    let mut mixer = Mixer::new(2, 200).unwrap();
    let mut result = Vec::new();
    for tick in 0..50 {
        for (track, rate, value) in [(0, 44100, 0.25), (1, 48000, 0.75)] {
            let frame = AudioFrame {
                sample_rate: rate,
                channels: 1,
                samples: vec![value; rate as usize / 50],
                timestamp: 100.0 + tick as f64 / 50.0,
            };
            for chunk in tracks[track].push(frame).unwrap() {
                mixer.push(track, &chunk).unwrap();
            }
            result.extend(mixer.chunks().into_iter().flatten());
        }
    }
    for (index, track) in tracks.into_iter().enumerate() {
        for chunk in track.finish().unwrap() {
            mixer.push(index, &chunk).unwrap();
        }
        mixer.finish(index);
        result.extend(mixer.chunks().into_iter().flatten());
    }
    assert_eq!(result.len(), 32000);
    let stable: Vec<_> = result[4000..28000]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    assert!(
        stable
            .iter()
            .all(|sample| (i32::from(*sample) - 16384).abs() <= 3)
    );
}
