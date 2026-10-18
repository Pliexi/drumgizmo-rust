// SPDX-License-Identifier: GPL-3.0-or-later
//! Pattern model for the drum step sequencer.
//!
//! Plain serializable data (`serde`) so the whole bank of patterns can be
//! stored in plugin state and saved with the project. The timing engine
//! lives in [`crate::timing`].

use serde::{Deserialize, Serialize};

/// Grid division of a pattern: one step spans this many beats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Division {
    /// One step per beat (1/4).
    Quarter,
    /// 1/8.
    Eighth,
    /// 1/16 — the classic drum grid.
    #[default]
    Sixteenth,
    /// 1/32.
    ThirtySecond,
    /// 1/64.
    SixtyFourth,
}

impl Division {
    /// Fraction of one beat covered by one step.
    pub fn beats_per_step(self) -> f64 {
        match self {
            Division::Quarter => 1.0,
            Division::Eighth => 0.5,
            Division::Sixteenth => 0.25,
            Division::ThirtySecond => 0.125,
            Division::SixtyFourth => 0.0625,
        }
    }
}

/// Articulation tag of a step (kit defined, engine passthrough).
pub type Articulation = String;

/// One sequencer lane: the steps of a single instrument.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lane {
    /// Driven instrument (index into the kit instruments).
    pub instrument_id: usize,
    /// MIDI note used when the pattern is exported (and for preview).
    pub midi_note: u8,
    /// Per-step state; exactly `pattern.length` entries.
    pub steps: Vec<Step>,
}

impl Lane {
    /// Lane of `length` empty steps.
    pub fn new(instrument_id: usize, midi_note: u8, length: usize) -> Self {
        Self {
            instrument_id,
            midi_note,
            steps: vec![Step::default(); length],
        }
    }

    /// Grows or shrinks the step vector to `length`.
    pub fn ensure_length(&mut self, length: usize) {
        self.steps.resize(length, Step::default());
    }
}

/// A single sequencer step.
///
/// All "modifier" fields have neutral defaults, so an empty step
/// serializes compactly and `Step::default()` is a valid off-step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// Step fires at all.
    pub enabled: bool,
    /// Base velocity `0.05..=1.0`; accents/ghosts scale it.
    pub velocity: f32,
    /// Accent: boosts the velocity (pattern `accent_gain`).
    pub accent: bool,
    /// Ghost note: lowers the velocity (pattern `ghost_gain`).
    pub ghost: bool,
    /// Probability the step fires at all (`0.0..=1.0`).
    pub probability: f32,
    /// Micro timing offset in steps (`-0.5..=0.5`), sample accurate.
    pub micro_offset: f32,
    /// Optional articulation tag.
    pub articulation: Option<Articulation>,
    /// Fires a choke (mute) event instead of a normal hit.
    pub choke: bool,
    /// Flam grace note.
    pub flam: bool,
    /// Ratchet/repeat: extra evenly spaced hits; `count = 0` disables.
    pub repeat: Option<Repeat>,
}

impl Default for Step {
    fn default() -> Self {
        Self {
            enabled: false,
            velocity: 0.8,
            accent: false,
            ghost: false,
            probability: 1.0,
            micro_offset: 0.0,
            articulation: None,
            choke: false,
            flam: false,
            repeat: None,
        }
    }
}

impl Step {
    /// Convenience constructor for an enabled hit.
    pub fn hit(velocity: f32) -> Self {
        Self {
            enabled: true,
            velocity,
            ..Self::default()
        }
    }

    /// Normalized (`0.05..=1.0`) velocity of the plain step.
    pub fn clamped_velocity(&self) -> f32 {
        self.velocity.clamp(0.05, 1.0)
    }
}

/// Ratchet/repeat parameters: the step is divided into `count` equal
/// parts; the main hit sits at the step position and `count - 1` extra
/// (quieter) hits follow at even spacing. Each extra hit's velocity is
/// multiplied by `falloff` cumulatively.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Repeat {
    /// Total hits inside the step (`1..=16`; `> 1` = ratchet).
    pub count: u8,
    /// Cumulative velocity falloff per extra hit (`0.05..=0.95`).
    pub falloff: f32,
}

impl Default for Repeat {
    fn default() -> Self {
        Self {
            count: 2,
            falloff: 0.75,
        }
    }
}

/// One entry of the chain/song mode.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ChainEntry {
    /// Pattern bank index.
    pub pattern: usize,
    /// How often the pattern is played before advancing (min 1).
    pub repeats: u32,
}

/// A complete drum pattern.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pattern {
    /// Display name.
    pub name: String,
    /// Amount of steps (16..128 in steps of the GUI grid).
    pub length: usize,
    /// Shared grid division of all lanes.
    pub division: Division,
    /// One lane per driven instrument.
    pub lanes: Vec<Lane>,
    /// Offbeat shift `0.0..=1.0`: odd steps are delayed by
    /// `swing * 0.5 * step_duration` (0 = straight, 2/3 ≈ triplet).
    pub swing: f32,
    /// Timing randomization `0.0..=1.0`: each hit moves by up to
    /// `humanize_timing * 0.03 * step_duration` either direction.
    pub humanize_timing: f32,
    /// Velocity randomization `0.0..=1.0`: multiplicative jitter of up to
    /// `humanize_velocity * 0.25`.
    pub humanize_velocity: f32,
    /// Accent boost (multiplier of the step velocity).
    pub accent_gain: f32,
    /// Ghost gain (multiplier of the step velocity).
    pub ghost_gain: f32,
    /// Flam grace note time in step fractions (default 0.03 steps).
    pub flam_time: f32,
    /// Flam grace note velocity gain.
    pub flam_velocity: f32,
}

impl Default for Pattern {
    fn default() -> Self {
        Self {
            name: "A".into(),
            length: 16,
            division: Division::Sixteenth,
            lanes: Vec::new(),
            swing: 0.0,
            humanize_timing: 0.0,
            humanize_velocity: 0.0,
            accent_gain: 1.2,
            ghost_gain: 0.4,
            flam_time: 0.03,
            flam_velocity: 0.4,
        }
    }
}

impl Pattern {
    /// Total beats the pattern spans.
    pub fn beats(&self) -> f64 {
        self.length as f64 * self.division.beats_per_step()
    }

    /// Length of one step in output frames for the given tempo/rate.
    pub fn step_frames(&self, samplerate: u64, tempo_bpm: f64) -> f64 {
        self.division.beats_per_step() * 60.0 / tempo_bpm * samplerate as f64
    }

    /// Duration of the full pattern in frames.
    pub fn pattern_frames(&self, samplerate: u64, tempo_bpm: f64) -> f64 {
        self.length as f64 * self.step_frames(samplerate, tempo_bpm)
    }

    /// Extends every lane to `length` (used after a length change).
    pub fn ensure_lengths(&mut self, length: usize) {
        self.length = length;
        for lane in &mut self.lanes {
            lane.ensure_length(length);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divisions_beat_fractions() {
        assert_eq!(Division::Quarter.beats_per_step(), 1.0);
        assert_eq!(Division::Sixteenth.beats_per_step(), 0.25);
        assert_eq!(Division::SixtyFourth.beats_per_step(), 0.0625);
    }

    #[test]
    fn step_frames_at_120bpm() {
        let pattern = Pattern::default();
        let frames = pattern.step_frames(44100, 120.0);
        // 60/120 = 0.5 s per beat; 1/16 of it = 0.125 s = 5512.5 frames
        assert!((frames - 5512.5).abs() < 1e-9);
        assert!((pattern.pattern_frames(44100, 120.0) - length_of_pattern()).abs() < 1e-6);
    }

    fn length_of_pattern() -> f64 {
        16.0 * 5512.5
    }

    #[test]
    fn ensure_lengths_resizes_lanes() {
        let mut pattern = Pattern {
            lanes: vec![Lane::new(0, 36, 16), Lane::new(1, 38, 8)],
            ..Default::default()
        };
        pattern.ensure_lengths(32);
        assert!(pattern.lanes.iter().all(|l| l.steps.len() == 32));
        // shrinks as well
        pattern.ensure_lengths(4);
        assert!(pattern.lanes.iter().all(|l| l.steps.len() == 4));
    }

    #[test]
    fn defaults_are_neutral() {
        let step = Step::default();
        assert!(!step.enabled);
        assert_eq!(step.velocity, 0.8);
        assert!((step.probability - 1.0).abs() < 1e-6);
        assert_eq!(step.repeat, None);
        assert!(step.clamped_velocity() >= 0.05 && step.clamped_velocity() <= 1.0);
    }
}
