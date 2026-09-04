//! Microphone capture.
//!
//! Replaces `Sources/AudioRecorder.swift`, which used AVFoundation's capture
//! session plus an `AVAudioConverter` to reach 16 kHz mono PCM16.
//!
//! On this stack that conversion is free. PipeWire's ALSA device advertises
//! one-channel capture across 1..384000 Hz, so the stream is opened directly
//! at 16 kHz mono and PipeWire does the resampling in its own graph. There is
//! no resampler here, and none is needed.
//!
//! cpal's stream handle is not `Send`, so the stream lives on a dedicated
//! thread and communicates through shared state.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use thiserror::Error;

/// What the transcription endpoint expects, and what whisper.cpp consumes
/// natively so its `--convert` (which shells out to ffmpeg) stays off.
pub const SAMPLE_RATE: u32 = 16_000;
pub const CHANNELS: u16 = 1;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("no input device is available")]
    NoDevice,
    #[error("input device {0:?} was not found; run `freeflow devices` to list them")]
    DeviceNotFound(String),
    #[error("could not open the input stream: {0}")]
    Stream(String),
    #[error("audio capture failed: {0}")]
    Capture(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Value to put in `audio.device`. This is the ALSA name, which is stable
    /// across reboots, unlike the human-readable description.
    pub id: String,
    pub description: String,
    pub is_default: bool,
}

/// Lists capture devices.
///
/// The entry to prefer is usually the default, which on this machine resolves
/// through PipeWire and therefore follows whatever `wpctl` has selected. Named
/// hardware devices bypass PipeWire's routing, so they are offered but not
/// recommended.
pub fn list_input_devices() -> Result<Vec<DeviceInfo>, AudioError> {
    let host = cpal::default_host();
    let default_id = host
        .default_input_device()
        .and_then(|d| d.id().ok())
        .map(|id| id.1);

    let mut devices = Vec::new();
    for device in host
        .input_devices()
        .map_err(|e| AudioError::Stream(e.to_string()))?
    {
        let Ok(id) = device.id() else { continue };
        let id = id.1;
        let description = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| id.clone());
        let is_default = Some(&id) == default_id.as_ref();
        devices.push(DeviceInfo {
            id,
            description,
            is_default,
        });
    }
    Ok(devices)
}

fn open_device(requested: Option<&str>) -> Result<cpal::Device, AudioError> {
    let host = cpal::default_host();
    let Some(requested) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return host.default_input_device().ok_or(AudioError::NoDevice);
    };
    for device in host
        .input_devices()
        .map_err(|e| AudioError::Stream(e.to_string()))?
    {
        if device.id().ok().map(|id| id.1).as_deref() == Some(requested) {
            return Ok(device);
        }
    }
    Err(AudioError::DeviceNotFound(requested.to_string()))
}

/// Shared between the capture thread and the caller.
#[derive(Default)]
struct Shared {
    samples: Mutex<Vec<i16>>,
    /// Peak amplitude of the most recent block, scaled to 0..=10_000 so it can
    /// live in an atomic. Drives the overlay's level meter.
    level: AtomicU32,
    stop: AtomicBool,
    error: Mutex<Option<String>>,
}

/// A capture in progress.
pub struct Recording {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    max_samples: usize,
}

impl Recording {
    /// Current input level in 0.0..=1.0, for the recording overlay.
    ///
    /// This is the fastest way for a user to notice they have the wrong
    /// capture device selected, which on a machine with several microphones
    /// is otherwise a silent failure.
    pub fn level(&self) -> f32 {
        self.shared.level.load(Ordering::Relaxed) as f32 / 10_000.0
    }

    pub fn sample_count(&self) -> usize {
        self.shared.samples.lock().map(|s| s.len()).unwrap_or(0)
    }

    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.sample_count() as f64 / SAMPLE_RATE as f64)
    }

    /// True once the hard cap is hit. The daemon polls this so a missed
    /// hotkey release cannot leave the microphone open indefinitely.
    pub fn reached_limit(&self) -> bool {
        self.sample_count() >= self.max_samples
    }

    /// Stops capture and returns the recording as a 16 kHz mono PCM16 WAV.
    pub fn stop(mut self) -> Result<Vec<u8>, AudioError> {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(error) = self.shared.error.lock().ok().and_then(|e| e.clone()) {
            return Err(AudioError::Capture(error));
        }
        let mut samples = self
            .shared
            .samples
            .lock()
            .map_err(|_| AudioError::Capture("sample buffer was poisoned".into()))?
            .clone();
        samples.truncate(self.max_samples);
        Ok(encode_wav(&samples))
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// Opens the microphone and begins capturing.
pub fn start(device: Option<&str>, max_seconds: u64) -> Result<Recording, AudioError> {
    let device = open_device(device)?;
    let shared = Arc::new(Shared::default());
    let max_samples = (max_seconds.max(1) as usize) * SAMPLE_RATE as usize;

    let config = cpal::StreamConfig {
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE,
        buffer_size: cpal::BufferSize::Default,
    };

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let thread_shared = Arc::clone(&shared);

    // cpal's `Stream` is not `Send`, so it must be created and dropped on one
    // thread. That thread owns the stream for the whole recording.
    let thread = std::thread::spawn(move || {
        let capture_shared = Arc::clone(&thread_shared);
        let error_shared = Arc::clone(&thread_shared);

        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut peak = 0.0f32;
                let mut converted = Vec::with_capacity(data.len());
                for &sample in data {
                    peak = peak.max(sample.abs());
                    // Clamp before scaling so a hot input cannot wrap around
                    // into loud noise at the opposite polarity.
                    converted.push((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
                }
                capture_shared
                    .level
                    .store((peak.min(1.0) * 10_000.0) as u32, Ordering::Relaxed);
                if let Ok(mut buffer) = capture_shared.samples.lock() {
                    if buffer.len() < max_samples {
                        buffer.extend_from_slice(&converted);
                    }
                }
            },
            move |err| {
                if let Ok(mut slot) = error_shared.error.lock() {
                    slot.get_or_insert_with(|| err.to_string());
                }
            },
            None,
        );

        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
        };
        if let Err(e) = stream.play() {
            let _ = ready_tx.send(Err(e.to_string()));
            return;
        }
        let _ = ready_tx.send(Ok(()));

        while !thread_shared.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(stream);
    });

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(Recording {
            shared,
            thread: Some(thread),
            max_samples,
        }),
        Ok(Err(e)) => Err(AudioError::Stream(e)),
        Err(_) => Err(AudioError::Stream(
            "input stream did not start within 5s".into(),
        )),
    }
}

/// How many PCM16 samples an encoded WAV carries.
///
/// A very short tap can stop the recorder before the first capture callback
/// has delivered anything, producing a header-only file. whisper-server
/// rejects those with HTTP 400, so callers need a way to notice before
/// posting.
pub fn wav_sample_count(wav: &[u8]) -> usize {
    wav.len().saturating_sub(44) / 2
}

/// Wraps PCM16 samples in a WAV container.
pub fn encode_wav(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&CHANNELS.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * CHANNELS as u32 * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&(CHANNELS * 2).to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_describes_16khz_mono_pcm16() {
        let wav = encode_wav(&[0, 1, -1, i16::MAX, i16::MIN]);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1, "PCM");
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1, "mono");
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            16_000
        );
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16, "bit depth");
        assert_eq!(&wav[36..40], b"data");

        // Declared sizes must match the payload, or whisper.cpp rejects it.
        let data_len = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]);
        assert_eq!(data_len, 10);
        assert_eq!(wav.len(), 44 + 10);
        let riff_len = u32::from_le_bytes([wav[4], wav[5], wav[6], wav[7]]);
        assert_eq!(riff_len as usize, wav.len() - 8);
    }

    #[test]
    fn an_empty_recording_still_produces_a_valid_wav() {
        let wav = encode_wav(&[]);
        assert_eq!(wav.len(), 44);
        assert_eq!(u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]), 0);
    }

    #[test]
    fn sample_counts_round_trip_through_encoding() {
        assert_eq!(wav_sample_count(&encode_wav(&[])), 0);
        assert_eq!(wav_sample_count(&encode_wav(&[0; 3200])), 3200);
        // Garbage shorter than a header is still zero, not a panic.
        assert_eq!(wav_sample_count(b"RIFF"), 0);
    }

    #[test]
    fn samples_round_trip_little_endian() {
        let wav = encode_wav(&[-2, 300]);
        assert_eq!(i16::from_le_bytes([wav[44], wav[45]]), -2);
        assert_eq!(i16::from_le_bytes([wav[46], wav[47]]), 300);
    }
}
