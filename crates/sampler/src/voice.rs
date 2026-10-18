// SPDX-License-Identifier: GPL-3.0-or-later
//! Realtime voice rendering.
//!
//! Port of the voice/`SampleEvent` handling of upstream `events_ds.h`,
//! `audiocacheeventhandler.cc` and the `renderSampleEvent`/`getSamples` part
//! of `drumgizmo.cc`:
//!
//! * one voice per (sample, file → kit channel) mapping,
//! * fractional read position with linear interpolation,
//! * choke ramp-down as a counting-down envelope
//!   (`envelope = rampdown_count / ramp_length`, removed at `== 0`),
//! * no fade-in at onset (upstream has none),
//! * per-instrument voice limit that chokes the eldest voice
//!   (`InputProcessor::limitVoices`, 0.5 ms default ramp),
//! * pool stealing: when all slots are busy the oldest voice is reused.
//!
//! No allocation happens while rendering: the pool is fixed and
//! preallocated, sample data is accessed through borrowed slices.

use dg_common::Sample;
use dg_kit::LoadedKit;

/// Ceiling of simultaneously rendered voices.
pub const VOICE_POOL: usize = 1024;

/// Upstream `voice_limit_max_default`.
pub const VOICE_LIMIT_MAX_DEFAULT: usize = 15;
/// Upstream `voice_limit_rampdown_default` (in ms here).
pub const VOICE_LIMIT_RAMPDOWN_DEFAULT_MS: f64 = 0.5;
/// Fixed group rampdown time of upstream (`inputprocessor.cc`).
pub const GROUP_RAMPDOWN_MS: f64 = 68.0;
/// Choke event rampdown time of upstream (`processChoke`).
pub const CHOKE_EVENT_RAMPDOWN_MS: f64 = 450.0;

/// Upstream `applyChoke`: ramp length in frames from a millisecond value.
#[inline]
pub fn ramp_frames(ramp_ms: f64, samplerate: u64) -> u64 {
    ((ramp_ms / 1000.0) * samplerate as f64) as u64
}

/// Read-only sample data view for the realtime thread.
///
/// The engine implements this on top of [`dg_kit::LoadedKit`] file table;
/// tests use synthetic data. Unknown indices yield an empty slice, which
/// keeps the render loop free of panics.
pub trait SampleBank {
    /// Channel data of `(file_index, filechannel)`, empty when absent.
    fn channel(&self, file_index: usize, filechannel: usize) -> &[Sample];
}

/// Instantiation data for a voice handed from selection to the pool.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceEvent {
    /// Producing instrument (choke group and voice limit bookkeeping).
    pub instrument_id: usize,
    /// Selected sample within the instrument.
    pub sample_index: usize,
    /// Which sample audio file backs the voice (index into the file table).
    pub file_index: usize,
    /// Channel within that file.
    pub filechannel: usize,
    /// Kit channel the voice renders into.
    pub kit_channel: usize,
    /// Sub-sample start offset in frames from "now" (>= 0).
    pub offset: f64,
    /// Static gain of the voice (velocity when normalizing, bleed factor).
    pub scale: f32,
    /// Choke group id for the voice (`!=""` semantics upstream, hashed).
    pub group_id: u32,
}

/// A single playing sample voice.
#[derive(Debug, Clone)]
pub struct Voice {
    pub active: bool,
    pub instrument_id: usize,
    pub sample_index: usize,
    pub file_index: usize,
    pub filechannel: usize,
    pub kit_channel: usize,

    /// Fractional position within the file (in file frames); negative while
    /// the voice has not reached its onset offset yet.
    pub position: f64,
    /// Increment per output frame: kit_rate / engine_rate (usually 1.0).
    pub position_step: f64,

    /// Length of the underlying audio channel in frames.
    pub end_frame: u64,

    /// Remaining ramp-down frames; `-1` == not ramping. Decremented per
    /// rendered frame; zero removes the voice.
    pub rampdown_count: i64,
    /// Voice position at which the ramp begins.
    pub rampdown_start: f64,
    /// Total ramp length in frames (envelope denominator).
    pub ramp_length: u64,

    pub scale: f32,
    pub group_id: u32,
    /// Monotonic spawn counter; "eldest" order for the voice limit.
    pub spawned: u64,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            active: false,
            instrument_id: 0,
            sample_index: 0,
            file_index: 0,
            filechannel: 0,
            kit_channel: 0,
            position: 0.0,
            position_step: 1.0,
            end_frame: 0,
            rampdown_count: -1,
            rampdown_start: f64::NEG_INFINITY,
            ramp_length: 0,
            scale: 1.0,
            group_id: 0,
            spawned: 0,
        }
    }
}

impl Voice {
    /// Current envelope multiplier in `[0, 1]`; 1.0 in steady state.
    #[inline]
    pub fn envelope(&self) -> f32 {
        if !self.rampdown_in_progress() || self.position < self.rampdown_start {
            1.0
        } else {
            (self.rampdown_count.max(0) as f32 / self.ramp_length as f32).min(1.0)
        }
    }

    /// `rampdownInProgress()` upstream: a ramp has been armed.
    #[inline]
    pub fn rampdown_in_progress(&self) -> bool {
        self.rampdown_count != -1
    }

    /// True when the voice finished all processing.
    #[inline]
    pub fn finished(&self) -> bool {
        !self.active
    }

    /// Renders one frame of the voice. Advances position/ramp bookkeeping
    /// exactly once per frame.
    #[inline]
    fn render_frame(&mut self, frame: usize, data: &[Sample], out_channels: &mut [&mut [Sample]]) {
        if self.kit_channel < out_channels.len() {
            let out = &mut out_channels[self.kit_channel][frame];

            if self.position >= 0.0 && self.position < self.end_frame as f64 && !data.is_empty() {
                let ipos = (self.position as usize).min(data.len() - 1);
                let hp = (ipos + 1).min(data.len() - 1);
                let frac = (self.position - ipos as f64) as f32;
                let value = data[ipos] + (data[hp] - data[ipos]) * frac;
                *out += value * self.scale * self.envelope();
            }
        }

        self.finish_frame();
    }

    /// One frame of bookkeeping: ramp-down count, advance, natural end.
    #[inline]
    fn finish_frame(&mut self) {
        // Only rendered frames decrement the ramp, mimicking upstream where
        // `evt.t` never advances over skipped onset frames.
        if self.rampdown_in_progress()
            && self.position >= 0.0
            && self.position >= self.rampdown_start
        {
            self.rampdown_count -= 1;
            if self.rampdown_count == 0 {
                self.active = false;
                return;
            }
        }

        self.position += self.position_step;

        if self.position >= self.end_frame as f64 {
            self.active = false;
        }
    }
}

/// Fixed pool of voices for the realtime thread.
#[derive(Debug)]
pub struct VoiceManager {
    pub voices: Vec<Voice>,
    spawn_seq: u64,
    robbed: u64,
}

impl Default for VoiceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceManager {
    pub fn new() -> Self {
        Self {
            voices: vec![Voice::default(); VOICE_POOL],
            spawn_seq: 0,
            robbed: 0,
        }
    }

    /// How many voices were reused across the manager lifetime (statistics).
    pub fn stolen_voices(&self) -> u64 {
        self.robbed
    }

    /// Starts a voice; when the pool is exhausted the eldest voice is
    /// reused (stolen) for the new event.
    pub fn spawn(
        &mut self,
        event: &VoiceEvent,
        sample_frames: u64,
        position_step: f64,
    ) -> Option<usize> {
        let index = match self.find_free_slot() {
            Some(index) => index,
            None => {
                let oldest = (0..VOICE_POOL)
                    .filter(|&i| self.voices[i].active)
                    .min_by_key(|&i| self.voices[i].spawned)?;
                self.robbed += 1;
                oldest
            }
        };

        let spawned = {
            self.spawn_seq += 1;
            self.spawn_seq
        };
        let voice = &mut self.voices[index];
        *voice = Voice {
            active: true,
            instrument_id: event.instrument_id,
            sample_index: event.sample_index,
            file_index: event.file_index,
            filechannel: event.filechannel,
            kit_channel: event.kit_channel,
            position: -event.offset.max(0.0),
            position_step,
            end_frame: sample_frames,
            rampdown_count: -1,
            rampdown_start: f64::NEG_INFINITY,
            ramp_length: 0,
            scale: event.scale,
            group_id: event.group_id,
            spawned,
        };
        Some(index)
    }

    /// `applyChokeGroup` upstream: every non-ramping voice of `group_id`
    /// that does not belong to the triggering instrument fades over
    /// `ramp_length` frames. `delay_frames` (usually the block-relative
    /// onset offset of the choking event) delays the ramp in voice time.
    pub fn apply_choke_group(
        &mut self,
        group_id: u32,
        trigger_instrument: usize,
        ramp_length: u64,
        delay_frames: f64,
    ) {
        if group_id == 0 {
            return; // "" group upstream
        }
        for voice in &mut self.voices {
            if voice.active
                && !voice.rampdown_in_progress()
                && voice.group_id == group_id
                && voice.instrument_id != trigger_instrument
            {
                arm_ramp_down(voice, ramp_length, delay_frames);
            }
        }
    }

    /// `processChoke` upstream: ramps all events of one instrument down
    /// (called with 450 ms from the MIDI choke mapping).
    pub fn apply_choke_instrument(
        &mut self,
        instrument_id: usize,
        ramp_length: u64,
        delay_frames: f64,
    ) {
        for voice in &mut self.voices {
            if voice.active && !voice.rampdown_in_progress() && voice.instrument_id == instrument_id
            {
                arm_ramp_down(voice, ramp_length, delay_frames);
            }
        }
    }

    /// `InputProcessor::limitVoices`: when more than `max_voices`
    /// non-ramping voices of an instrument are active, the eldest is choked
    /// with `ramp_ms` (upstream default 0.5 ms).
    pub fn limit_voices(
        &mut self,
        instrument_id: usize,
        max_voices: usize,
        ramp_ms: f64,
        samplerate: u64,
    ) {
        let mut non_ramping: Vec<usize> = self
            .voices
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                v.active && v.instrument_id == instrument_id && !v.rampdown_in_progress()
            })
            .map(|(i, _)| i)
            .collect();

        if non_ramping.len() <= max_voices {
            return;
        }

        non_ramping.sort_by_key(|&i| self.voices[i].spawned);
        let eldest = non_ramping[0];
        arm_ramp_down(
            &mut self.voices[eldest],
            ramp_frames(ramp_ms, samplerate),
            0.0,
        );
    }

    /// Number of active voices.
    pub fn active_count(&self) -> usize {
        self.voices.iter().filter(|v| v.active).count()
    }

    /// Percentage of used voices.
    pub fn load(&self) -> f32 {
        self.active_count() as f32 / VOICE_POOL as f32
    }

    fn find_free_slot(&self) -> Option<usize> {
        self.voices.iter().position(|v| !v.active)
    }

    /// Renders one block across all voices into `out_channels` (one buffer
    /// per rendered kit channel, equal lengths).
    pub fn render(&mut self, bank: &dyn SampleBank, out_channels: &mut [&mut [Sample]]) {
        if out_channels.is_empty() {
            return;
        }
        let frames = out_channels[0].len();

        for voice in &mut self.voices {
            if !voice.active {
                continue;
            }
            let data = bank.channel(voice.file_index, voice.filechannel);
            for frame in 0..frames {
                if !voice.active {
                    break;
                }
                voice.render_frame(frame, data, out_channels);
            }
        }
    }
}

/// Arms a ramp: it starts `delay_frames` after the voice's current
/// position and counts down `ramp_length` rendered frames.
fn arm_ramp_down(voice: &mut Voice, ramp_length: u64, delay_frames: f64) {
    let ramp_length = ramp_length.max(1);
    voice.ramp_length = ramp_length;
    voice.rampdown_start = voice.position + delay_frames.clamp(0.0, f64::MAX);
    voice.rampdown_count = ramp_length as i64;
}

impl SampleBank for LoadedKit {
    fn channel(&self, file_index: usize, filechannel: usize) -> &[Sample] {
        self.pub_files
            .get(file_index)
            .and_then(|f| f.data.get(filechannel))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bank with one mono file of constant value.
    struct Flat(Vec<Vec<Sample>>);

    impl SampleBank for Flat {
        fn channel(&self, file_index: usize, filechannel: usize) -> &[Sample] {
            if filechannel == 0 {
                self.0.get(file_index).map(|v| v.as_slice()).unwrap_or(&[])
            } else {
                &[]
            }
        }
    }

    fn ev(instrument: usize, group: u32, kit_channel: usize, file: usize) -> VoiceEvent {
        VoiceEvent {
            instrument_id: instrument,
            sample_index: 0,
            file_index: file,
            filechannel: 0,
            kit_channel,
            offset: 0.0,
            scale: 1.0,
            group_id: group,
        }
    }

    #[test]
    fn spawn_fills_pool_then_steals_oldest() {
        let mut manager = VoiceManager::new();
        for _ in 0..VOICE_POOL {
            assert!(manager.spawn(&ev(1, 1, 0, 0), 100_000, 1.0).is_some());
        }
        assert_eq!(manager.active_count(), VOICE_POOL);

        let index = manager.spawn(&ev(2, 2, 0, 0), 100_000, 1.0).unwrap();
        assert_eq!(manager.stolen_voices(), 1);
        assert_eq!(manager.voices[index].instrument_id, 2);
        assert_eq!(manager.voices[index].spawned, (VOICE_POOL + 1) as u64);
    }

    #[test]
    fn choke_group_ramps_other_instruments_same_group() {
        let mut manager = VoiceManager::new();
        let a = manager.spawn(&ev(1, 7, 0, 0), 100_000, 1.0).unwrap();
        let b = manager.spawn(&ev(2, 7, 0, 0), 100_000, 1.0).unwrap();
        let c = manager.spawn(&ev(3, 9, 0, 0), 100_000, 1.0).unwrap();
        // Excluding the trigger instrument, like upstream.
        manager.apply_choke_group(7, 1, ramp_frames(GROUP_RAMPDOWN_MS, 44100), 0.0);
        assert!(
            !manager.voices[a].rampdown_in_progress(),
            "trigger's own voice stays untouched"
        );
        assert!(manager.voices[b].rampdown_in_progress());
        assert!(!manager.voices[c].rampdown_in_progress(), "other group");
    }

    #[test]
    fn empty_group_never_chokes() {
        let mut manager = VoiceManager::new();
        let a = manager.spawn(&ev(1, 0, 0, 0), 100_000, 1.0).unwrap();
        manager.apply_choke_group(0, 2, 100, 0.0);
        assert!(!manager.voices[a].rampdown_in_progress());
    }

    #[test]
    fn choke_instrument_ramps_same_instrument() {
        let mut manager = VoiceManager::new();
        let a = manager.spawn(&ev(1, 7, 0, 0), 100_000, 1.0).unwrap();
        let b = manager.spawn(&ev(2, 7, 0, 0), 100_000, 1.0).unwrap();
        manager.apply_choke_instrument(1, ramp_frames(CHOKE_EVENT_RAMPDOWN_MS, 44100), 0.0);
        assert!(manager.voices[a].rampdown_in_progress());
        assert_eq!(
            manager.voices[a].ramp_length,
            ramp_frames(CHOKE_EVENT_RAMPDOWN_MS, 44100)
        );
        assert!(!manager.voices[b].rampdown_in_progress());
    }

    #[test]
    fn choke_is_applied_once_only() {
        let mut manager = VoiceManager::new();
        let a = manager.spawn(&ev(1, 7, 0, 0), 100_000, 1.0).unwrap();
        manager.apply_choke_group(7, 2, 8, 0.0);
        let first = manager.voices[a].rampdown_count;
        manager.apply_choke_group(7, 2, 8, 0.0);
        assert_eq!(manager.voices[a].rampdown_count, first);
    }

    #[test]
    fn linear_interpolation_render() {
        // File data 0, 1, 2, 3 rendered with a step of 0.5 becomes a
        // linear 0, 0.5, 1, .. ramp across 8 output frames.
        let bank = Flat(vec![vec![0.0, 1.0, 2.0, 3.0]]);
        let mut manager = VoiceManager::new();
        manager.spawn(&ev(1, 0, 0, 0), 4, 0.5).unwrap();
        let mut out = vec![0.0f32; 9];
        manager.render(&bank, &mut [&mut out]);
        for (k, value) in out.iter().enumerate() {
            // Frame 7 sits at file position 3.5, clamped to the last frame.
            let expected = if k < 8 { 0.5 * k as f32 } else { 0.0 };
            let polled = if k == 7 { 3.0 } else { expected };
            assert!(
                (value - polled).abs() < 1e-6,
                "frame {k}: {value} != {polled}"
            );
        }
    }

    #[test]
    fn render_is_deterministic() {
        let bank = Flat(vec![vec![0.1, 0.9, 0.1, 0.9, 0.1]]);
        let render_once = |seed_offset: f64| -> Vec<f32> {
            let mut manager = VoiceManager::new();
            let mut event = ev(1, 0, 0, 0);
            event.offset = seed_offset;
            manager.spawn(&event, 5, 1.0).unwrap();
            let mut out = vec![0.0f32; 10];
            manager.render(&bank, &mut [&mut out]);
            out
        };
        assert_eq!(render_once(0.5), render_once(0.5));
        assert_ne!(render_once(0.5), render_once(0.0), "offset matters");
    }

    #[test]
    fn render_start_offset_defers_writing() {
        let bank = Flat(vec![vec![1.0f32; 16]]);
        let mut manager = VoiceManager::new();
        let mut event = ev(1, 0, 0, 0);
        event.offset = 3.0;
        manager.spawn(&event, 16, 1.0).unwrap();
        let mut out = vec![0.0f32; 6];
        manager.render(&bank, &mut [&mut out]);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
        assert_eq!(out[2], 0.0);
        assert!((out[3] - 1.0).abs() < 1e-6, "first audible at frame 3");
        assert!((out[5] - 1.0).abs() < 1e-6, "still audible after");
    }

    #[test]
    fn rampdown_fades_over_length_frames() {
        let bank = Flat(vec![vec![1.0f32; 64]]);
        let mut manager = VoiceManager::new();
        let index = manager.spawn(&ev(1, 0, 0, 0), 64, 1.0).unwrap();
        let ramp = 8u64;
        manager.apply_choke_instrument(1, ramp, 0.0);
        let mut out = vec![0.0f32; ramp as usize + 2];
        manager.render(&bank, &mut [&mut out]);
        // envelope: length/length, .., 1/length, then removed (0 afterwards)
        assert!((out[0] - 1.0).abs() < 1e-6);
        for (k, frame) in out.iter().enumerate().take(ramp as usize).skip(1) {
            let expected = (ramp as f32 - k as f32) / ramp as f32;
            assert!(
                (frame - expected).abs() < 1e-5,
                "frame {k}: {} vs {expected}",
                frame
            );
        }
        assert_eq!(out[ramp as usize], 0.0, "voice removed after ramp");
        assert!(manager.voices[index].finished());
    }

    #[test]
    fn limit_voices_chokes_the_eldest() {
        let mut manager = VoiceManager::new();
        for _ in 0..5 {
            manager.spawn(&ev(3, 0, 0, 0), 100_000, 1.0).unwrap();
        }
        manager.limit_voices(3, 4, VOICE_LIMIT_RAMPDOWN_DEFAULT_MS, 44100);
        let chokes: Vec<_> = manager
            .voices
            .iter()
            .filter(|v| v.active && v.rampdown_in_progress())
            .collect();
        assert_eq!(chokes.len(), 1, "exactly the eldest is choked");
        assert_eq!(chokes[0].spawned, 1);
        assert_eq!(chokes[0].ramp_length, ramp_frames(0.5, 44100));
    }

    #[test]
    fn choke_delay_starts_ramp_later() {
        let bank = Flat(vec![vec![1.0f32; 32]]);
        let mut manager = VoiceManager::new();
        manager.spawn(&ev(1, 0, 0, 0), 32, 1.0).unwrap();
        manager.apply_choke_instrument(1, 4, 4.0);
        let mut out = vec![0.0f32; 10];
        manager.render(&bank, &mut [&mut out]);
        assert_eq!(
            out,
            vec![1.0, 1.0, 1.0, 1.0, 1.0, 0.75, 0.5, 0.25, 0.0, 0.0],
            "ramp begins after the 4 frame delay"
        );
    }

    #[test]
    fn limit_voices_ignores_under_limit() {
        let mut manager = VoiceManager::new();
        for _ in 0..4 {
            manager.spawn(&ev(3, 0, 0, 0), 100_000, 1.0).unwrap();
        }
        manager.limit_voices(3, VOICE_LIMIT_MAX_DEFAULT, 0.5, 44100);
        assert!(manager.voices.iter().all(|v| !v.rampdown_in_progress()));
    }

    #[test]
    fn render_skips_kit_channels_beyond_mix() {
        let bank = Flat(vec![vec![1.0f32; 32]]);
        let mut manager = VoiceManager::new();
        let index = manager.spawn(&ev(1, 0, 5, 0), 32, 1.0).unwrap();
        let mut left = vec![0.0f32; 8];
        let mut right = vec![0.0f32; 8];
        manager.render(&bank, &mut [&mut left, &mut right]);
        assert!(left.iter().all(|v| *v == 0.0));
        assert!(manager.voices[index].active, "voice tracks time silently");
        assert_eq!(manager.voices[index].position, 8.0, "8 frames advanced");
    }

    #[test]
    fn render_misses_unknown_files_without_panic() {
        let bank = Flat(vec![]);
        let mut manager = VoiceManager::new();
        manager.spawn(&ev(1, 0, 0, 42), 32, 1.0).unwrap();
        let mut out = vec![0.0f32; 4];
        manager.render(&bank, &mut [&mut out]);
        assert!(out.iter().all(|v| *v == 0.0));
    }
}
