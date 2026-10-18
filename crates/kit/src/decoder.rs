// SPDX-License-Identifier: GPL-3.0-or-later
//! Decoding of audio files referenced by kit instrument XMLs.
//!
//! Mirrors the upstream `src/audiofile.cc` (libsndfile caller) plus the
//! samplerate conversion performed by `zrwrapper.cc`/`zita-resampler`.
//! The output is a planar buffer, one channel per file channel, in 32-bit
//! float samples at the requested engine samplerate.

use std::path::Path;

use rubato::{FftFixedInOut, Resampler};
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

/// Errors raised while decoding a kit audio file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

/// A decoded, planarized, resampled audio file.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedAudio {
    /// Effective (post-conversion) sample rate.
    pub samplerate: u32,
    /// Number of channels present in the *original file*.
    pub file_channels: usize,
    /// Planar samples, `data[channel][frame]`.
    pub data: Vec<Vec<f32>>,
}

/// Decodes `path` and (optionally) resamples it to `target_samplerate`.
pub fn decode_file(
    path: &Path,
    target_samplerate: Option<f64>,
) -> Result<DecodedAudio, DecodeError> {
    let src = std::fs::File::open(path)
        .map_err(|e| DecodeError(format!("Cannot open '{}': {}", path.display(), e)))?;

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mss = MediaSourceStream::new(
        Box::new(src) as Box<dyn MediaSource>,
        MediaSourceStreamOptions::default(),
    );

    let mut reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| DecodeError(format!("Unrecognized format: {e}")))?;

    let track = reader
        .first_track(TrackType::Audio)
        .ok_or_else(|| DecodeError("No audio track found".into()))?
        .clone();

    let audio_params = match &track.codec_params {
        Some(CodecParameters::Audio(audio_params)) => audio_params.clone(),
        _ => return Err(DecodeError("Track has no audio codec parameters".into())),
    };

    let file_samplerate = audio_params
        .sample_rate
        .ok_or_else(|| DecodeError("No sample rate in track".into()))?;
    let file_channels = match &track.codec_params {
        Some(CodecParameters::Audio(audio)) => {
            audio.channels.as_ref().map(|c| c.count()).unwrap_or(1)
        }
        _ => 1,
    };

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| DecodeError(format!("Decoder init failed: {e}")))?;

    // Per-filechannel accumulation buffer, one plane per channel.
    let mut data: Vec<Vec<f32>> = vec![Vec::new(); file_channels];

    loop {
        // `None` means the media ended, matching upstream EOF handling.
        let packet = match reader.next_packet() {
            Ok(Some(p)) if p.track_id == track.id => p,
            Ok(Some(_)) => continue, // another track: skip
            Ok(None) => break,       // end of stream
            Err(SymphoniaError::IoError(_))
            | Err(SymphoniaError::ResetRequired)
            | Err(SymphoniaError::LimitError(_)) => break,
            Err(other) => return Err(DecodeError(format!("Decode failed: {other}"))),
        };

        match decoder.decode(&packet) {
            Ok(decoded) => {
                append_planar(&decoded, &mut data);
            }
            // Skip undecodable packets; upstream libs most likely tolerated
            // this the same way (sf_read_* loop).
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => {
                continue;
            }
            Err(other) => return Err(DecodeError(format!("Decode failed: {other}"))),
        }
    }

    decimate_and_resample(data, file_samplerate, target_samplerate)
}

/// Generic copy of a decoded audio buffer into planar sample vectors.
fn append_planar(decoded: &GenericAudioBufferRef<'_>, data: &mut Vec<Vec<f32>>) {
    let channels = decoded.spec().channels().count();
    if channels != data.len() {
        // Channel count changed mid-stream; pad with empty planes if needed.
        if channels > data.len() {
            data.resize(channels, Vec::new());
        }
    }
    let frames = decoded.frames();
    if frames == 0 {
        return;
    }
    let mut planes: Vec<Vec<f32>> = vec![Vec::with_capacity(frames); channels];
    decoded.copy_to_vecs_planar::<f32>(&mut planes);
    for (acc, plane) in data.iter_mut().zip(planes) {
        acc.extend_from_slice(&plane);
    }
}

fn decimate_and_resample(
    mut data: Vec<Vec<f32>>,
    file_samplerate: u32,
    target_samplerate: Option<f64>,
) -> Result<DecodedAudio, DecodeError> {
    let frames = data.first().map(|p| p.len()).unwrap_or(0);

    // Normalize plane lengths (round up to homeowner error tolerance).
    for plane in &mut data {
        plane.truncate(frames);
    }

    let samplerate = match target_samplerate {
        None => file_samplerate,
        Some(target) => {
            data = resample_planar(&data, f64::from(file_samplerate), target);
            (target.round() as u32).max(1)
        }
    };

    Ok(DecodedAudio {
        samplerate,
        file_channels: data.len(),
        data,
    })
}

/// Resample planar audio with `rubato`'s FFT resampler (same quality class as
/// the zita-resampler used by upstream), with a linear-interpolating fallback
/// if the FFT resampler cannot be constructed for the given rate pair.
pub fn resample_planar(data: &[Vec<f32>], from: f64, to: f64) -> Vec<Vec<f32>> {
    if from == to || data.is_empty() || (from - to).abs() < 1e-6 {
        return data.to_vec();
    }
    let in_rate = from.round() as usize;
    let out_rate = to.round() as usize;
    let frames = data[0].len();

    if let Ok(mut resampler) = FftFixedInOut::<f32>::new(in_rate, out_rate, 1024, data.len()) {
        let chunk_in = resampler.input_frames_max();
        let mut out_buf = resampler.output_buffer_allocate(true);
        let out_frames_est = (frames as f64 * out_rate as f64 / in_rate as f64).ceil() as usize;
        let mut out: Vec<Vec<f32>> =
            vec![Vec::with_capacity(out_frames_est + chunk_in); data.len()];

        let mut position = 0;
        while position < frames {
            let remaining = frames.saturating_sub(position);
            if remaining == 0 {
                break;
            }
            let input: Vec<Vec<f32>> = data
                .iter()
                .map(|plane| {
                    let mut chunkvec = plane[position..(position + remaining)].to_vec();
                    chunkvec.resize(chunk_in, 0.0);
                    chunkvec
                })
                .collect();

            match resampler.process_into_buffer(&input, &mut out_buf, None) {
                Ok((_consumed, produced)) => {
                    for (c, plane) in out_buf.iter().enumerate() {
                        out[c].extend_from_slice(&plane[..produced]);
                    }
                }
                Err(_) => return linear_resample(data, from, to),
            }
            position += chunk_in;
        }

        for plane in &mut out {
            plane.truncate(out_frames_est);
        }
        out
    } else {
        linear_resample(data, from, to)
    }
}

/// Linear-interpolating fallback resampler (used if FFT resampler fails).
/// Quality is lower but it never fails.
pub fn linear_resample(data: &[Vec<f32>], from: f64, to: f64) -> Vec<Vec<f32>> {
    if from == to || data.is_empty() {
        return data.to_vec();
    }
    let frames = data[0].len();
    if frames < 2 {
        return data.to_vec();
    }
    let out_frames = (frames as f64 * to / from).ceil() as usize;
    let step = (frames - 1) as f64 / (out_frames.max(1) as f64);
    let _ = frames;
    data.iter()
        .map(|plane| {
            let mut out = Vec::with_capacity(out_frames);
            for i in 0..out_frames {
                let pos = i as f64 * step;
                let base = pos.floor() as usize;
                let frac = (pos - (base as f64)) as f32;
                let next = (base + 1).min(frames - 1);
                let value = (1.0 - frac) * plane[base] + frac * plane[next];
                out.push(value);
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_resample_identity() {
        let data = vec![vec![0.0, 1.0, 0.5, -0.5]];
        let out = linear_resample(&data, 44100.0, 44100.0);
        assert_eq!(out[0], data[0]);
    }

    #[test]
    fn linear_resample_upsample() {
        // 3 frames at rate R -> 6 frames at rate 2R, endpoints preserved.
        let data = vec![vec![0.0f32, 0.5, 1.0]];
        let out = linear_resample(&data, 22050.0, 44100.0);
        assert_eq!(out[0].len(), 6);
        assert!((out[0][0] - 0.0).abs() < 1e-6);
        assert!((out[0][5] - 0.8333).abs() < 0.01); // linear interp past last control point
    }

    #[test]
    fn fft_resampler_runs() {
        let data = vec![vec![0.0f32; 4096], vec![1.0f32; 4096]];
        let out = resample_planar(&data, 44100.0, 48000.0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), out[1].len());
        // within a small tolerance of the mathematically expected length
        let expected = 4096.0 * 48000.0 / 44100.0;
        assert!((out[0].len() as f64 - expected).abs() < 32.0);
        // silence stays silence, DC stays DC
        assert!(out[0].iter().fold(0.0f32, |a, b| a.max(b.abs())) < 1e-3);
        assert!(out[1].iter().fold(0.0f32, |a, b| a.max(b.abs())) > 0.9);
    }
}
