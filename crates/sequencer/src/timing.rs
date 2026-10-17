// SPDX-License-Identifier: GPL-3.0-or-later
//! Sequencer timing engine.
//!
//! Converts a [`Pattern`] into absolute-frame hits with sample accuracy.
//! Musical positions are tracked in beats (float), so host tempo changes
//! take effect immediately on the next scheduled step. The rng stream
//! (probability, humanize) is a single deterministic stream seeded by
//! [`Sequencer::seed`]; identical seeds and identical poll sequences
//! produce identical output (ТЗ "humanize deterministic mode").
//!
//! Implementation notes (documented in docs/sequencer.md as well):
//! * swing delays odd steps by `swing * 0.5 * step_duration`,
//! * humanize timing jitters a hit by up to `humanize_timing * 0.05`
//!   step durations in either direction,
//! * humanize velocity jitters multiplicatively by up to
//!   `humanize_velocity * 0.25` around the step velocity,
//! * one probability roll decides the fate of *all* hits of a step
//!   (flam grace/ratchet daughters inherit the decision),
//! * a ratchet divides the swung step into `count` equal parts,
//! * choke steps emit a mute event instead of a hit,
//! * accents/ghosts scale the velocity, the final velocity is clamped
//!   to `0.01..=1.0`.

use dg_common::rng::Rand;

use crate::model::{Articulation, ChainEntry, Pattern, Repeat};

/// What the engine receives from the sequencer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepEventKind {
    /// A normal drum hit.
    Hit,
    /// Mute/choke trigger (engine ramps the instrument down, 450 ms).
    Choke,
}

/// One engine event of the sequencer.
#[derive(Debug, Clone, PartialEq)]
pub struct StepEvent {
    /// Absolute output frame of the event on the sequencer timeline.
    pub frame: u64,
    /// Ordering id: the driven instrument.
    pub instrument_id: usize,
    /// Velocity after all modifiers (`0.01..=1.0`; chokes emit 0.0).
    pub velocity: f32,
    pub kind: StepEventKind,
    /// Optional articulation tag (engine passthrough).
    pub articulation: Option<Articulation>,
    /// Step index inside the pattern (UI/debugging).
    pub step: usize,
}

/// Velocities stay audible.
const MIN_VELOCITY: f32 = 0.01;

/// Builds the events of one step; `step_beats` includes the swing shift.
fn schedule_step(
    pattern: &Pattern,
    step: usize,
    step_beats: f64,
    frames_per_beat: f64,
    rng: &mut Rand,
) -> Vec<StepEvent> {
    let beats_per_step = pattern.division.beats_per_step();
    let mut events = Vec::new();
    let frames_of = |beats: f64| -> u64 {
        let frames = beats * frames_per_beat;
        if frames <= 0.0 || !frames.is_finite() {
            0
        } else {
            frames as u64
        }
    };

    for lane in &pattern.lanes {
        let Some(step_def) = lane.steps.get(step) else {
            continue;
        };
        if !step_def.enabled {
            continue;
        }
        let probability = step_def.probability.clamp(0.0, 1.0);
        if probability < 1.0 && rng.next_f32() > probability {
            continue;
        }

        // Velocity chain: accent boost, ghost reduction, humanize jitter.
        let mut velocity = step_def.clamped_velocity();
        if step_def.accent {
            velocity *= pattern.accent_gain.max(0.0);
        }
        if step_def.ghost {
            velocity *= pattern.ghost_gain.max(0.0);
        }
        if pattern.humanize_velocity > 0.0 {
            let centred = rng.next_f32() * 2.0 - 1.0;
            velocity *= 1.0 + pattern.humanize_velocity * 0.25 * centred;
        }
        velocity = velocity.clamp(MIN_VELOCITY, 1.0);

        // Micro timing + humanize timing, both in step fractions.
        let mut offset_steps = step_def.micro_offset.clamp(-0.5, 0.5);
        if pattern.humanize_timing > 0.0 {
            let centred = rng.next_f32() * 2.0 - 1.0;
            offset_steps += pattern.humanize_timing * 0.05 * centred;
        }
        let hit_beats = step_beats + f64::from(offset_steps) * beats_per_step;

        let make = |frame: u64, velocity: f32| StepEvent {
            frame,
            instrument_id: lane.instrument_id,
            velocity,
            kind: StepEventKind::Hit,
            articulation: step_def.articulation.clone(),
            step,
        };

        if step_def.choke {
            events.push(StepEvent {
                frame: frames_of(hit_beats),
                instrument_id: lane.instrument_id,
                velocity: 0.0,
                kind: StepEventKind::Choke,
                articulation: step_def.articulation.clone(),
                step,
            });
            continue;
        }

        if step_def.flam {
            let grace_beats = hit_beats - f64::from(pattern.flam_time.max(0.0)) * beats_per_step;
            events.push(make(
                frames_of(grace_beats),
                (velocity * pattern.flam_velocity).clamp(MIN_VELOCITY, 1.0),
            ));
        }

        events.push(make(frames_of(hit_beats), velocity));

        if let Some(Repeat { count, falloff }) = step_def.repeat {
            let falloff = falloff.clamp(0.05, 0.95);
            for k in 1..count.max(1) as usize {
                let sub_beats = hit_beats + (k as f64 / count.max(1) as f64) * beats_per_step;
                events.push(make(
                    frames_of(sub_beats),
                    (velocity * falloff.powi(k as i32)).clamp(MIN_VELOCITY, 1.0),
                ));
            }
        }
    }

    events
}

/// Scheduling and chain/song engine over the pattern bank.
pub struct Sequencer {
    pub samplerate: u64,
    /// Host-provided tempo. Used when [`Sequencer::host_sync`] is set.
    pub host_tempo: Option<f64>,
    /// Internal tempo fallback (bpm).
    pub internal_tempo: f64,
    /// Whether the host tempo is used.
    pub host_sync: bool,
    /// Pattern bank.
    pub patterns: Vec<Pattern>,
    /// Chain/song mode: which pattern plays how often, in which order.
    pub chain: Vec<ChainEntry>,
    /// Loop the chain forever (otherwise stop at its end).
    pub loop_chain: bool,
    /// Loop the current pattern (when no chain is active this is always on).
    pub loop_pattern: bool,

    /// Pattern index used while the chain is empty.
    pub pattern_index: usize,
    /// Rng seed for the deterministic humanize/probability stream.
    pub seed: u64,

    playing: bool,
    rng: Rand,
    origin_beats: f64,
    cursor: usize,
    chain_index: usize,
    chain_repeat_count: u32,
}

impl Sequencer {
    pub fn new(samplerate: u64, seed: u64) -> Self {
        Self {
            samplerate,
            host_tempo: None,
            internal_tempo: 120.0,
            host_sync: false,
            patterns: Vec::new(),
            chain: Vec::new(),
            loop_chain: true,
            loop_pattern: true,
            pattern_index: 0,
            seed,
            playing: false,
            rng: Rand::new(seed),
            origin_beats: 0.0,
            cursor: 0,
            chain_index: 0,
            chain_repeat_count: 0,
        }
    }

    /// Effective tempo: host tempo when synced to a healthy value,
    /// otherwise the internal fallback.
    pub fn tempo(&self) -> f64 {
        let tempo = if self.host_sync && self.host_tempo.is_some() {
            self.host_tempo.unwrap_or(self.internal_tempo)
        } else {
            self.internal_tempo
        };
        if tempo <= 1.0 || !tempo.is_finite() {
            self.internal_tempo
        } else {
            tempo
        }
    }

    /// Frames that one beat spans at the current effective tempo.
    pub fn frames_per_beat(&self) -> f64 {
        self.samplerate as f64 * 60.0 / self.tempo()
    }

    /// Whether the transport is running.
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Current chain position (`None` when no chain is active).
    pub fn chain_position(&self) -> Option<usize> {
        if self.chain.is_empty() {
            None
        } else {
            Some(self.chain_index)
        }
    }

    /// Starts the transport at `beats` (0 = pattern origin) with a fresh
    /// deterministic rng stream.
    pub fn start(&mut self, beats: f64) {
        self.origin_beats = beats;
        self.cursor = 0;
        self.chain_repeat_count = 0;
        self.rng = Rand::new(self.seed);
        self.playing = true;
    }

    /// Same as [`Sequencer::start`] but keeps the rng stream running.
    pub fn resume(&mut self, beats: f64) {
        self.origin_beats = beats;
        self.cursor = 0;
        self.playing = true;
    }

    /// Stops the transport and rewinds to the pattern origin.
    pub fn stop(&mut self) {
        self.playing = false;
        self.origin_beats = 0.0;
        self.cursor = 0;
        self.chain_index = 0;
        self.chain_repeat_count = 0;
        self.rng = Rand::new(self.seed);
    }

    /// Sets the seed and restarts the deterministic stream.
    pub fn set_seed(&mut self, seed: u64) {
        self.seed = seed;
        self.rng = Rand::new(seed);
    }

    /// Index of the pattern that is currently scheduled.
    pub fn current_pattern_index(&self) -> usize {
        if let Some(entry) = self.chain.get(self.chain_index) {
            if entry.pattern < self.patterns.len() {
                return entry.pattern;
            }
        }
        if self.patterns.is_empty() {
            0
        } else {
            self.pattern_index.min(self.patterns.len() - 1)
        }
    }

    /// The pattern that is currently scheduled.
    pub fn current_pattern(&self) -> &Pattern {
        &self.patterns[self.current_pattern_index()]
    }

    /// Emits all events whose absolute start lies before `until_frame`,
    /// advancing the internal cursor. Call once per engine block with
    /// `absolute_position + block_size`; events never get lost or
    /// duplicated across block boundaries.
    pub fn poll(&mut self, until_frame: u64) -> Vec<StepEvent> {
        let mut events = Vec::new();
        if !self.playing || self.patterns.is_empty() {
            return events;
        }
        let frames_per_beat = self.frames_per_beat();

        loop {
            let index = self.current_pattern_index();
            let pattern = &self.patterns[index];
            if pattern.length == 0 {
                if !self.advance_pattern() {
                    break; // chain ended
                }
                continue;
            }
            let beats_per_step = pattern.division.beats_per_step();
            let mut step_beats = self.origin_beats + self.cursor as f64 * beats_per_step;
            // Swing shifts odd steps (1, 3, 5, ...) later.
            if self.cursor % 2 == 1 {
                step_beats += f64::from(pattern.swing.clamp(0.0, 1.0)) * 0.5 * beats_per_step;
            }
            let base_frame = frames_of(step_beats, frames_per_beat);
            if base_frame >= until_frame {
                break;
            }

            events.extend(schedule_step(
                pattern,
                self.cursor,
                step_beats,
                frames_per_beat,
                &mut self.rng,
            ));

            self.cursor += 1;
            if self.cursor >= pattern.length {
                self.cursor = 0;
                self.origin_beats += pattern.beats();
                if !self.advance_pattern() {
                    break; // chain ended and looping is disabled
                }
            }
        }

        events
    }

    /// Advances over a pattern boundary. Returns false when the transport
    /// should stop (non looping chain exhausted).
    fn advance_pattern(&mut self) -> bool {
        if self.chain.is_empty() {
            if self.loop_pattern {
                return true;
            }
            self.playing = false;
            return false;
        }
        self.chain_repeat_count += 1;
        let entry = self.chain[self.chain_index];
        if self.chain_repeat_count < entry.repeats.max(1) {
            return true; // same pattern once more
        }
        self.chain_repeat_count = 0;
        self.chain_index += 1;
        if self.chain_index >= self.chain.len() {
            if self.loop_chain {
                self.chain_index = 0;
            } else {
                self.playing = false;
                return false;
            }
        }
        true
    }
}

/// Absolute output frame of a musical beat position (clamps to 0).
#[inline]
fn frames_of(beats: f64, frames_per_beat: f64) -> u64 {
    let frames = beats * frames_per_beat;
    if frames <= 0.0 || !frames.is_finite() {
        0
    } else {
        frames as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Division, Lane, Step};
    use crate::timing::StepEventKind::{Choke, Hit};

    const RATE: u64 = 44_100;

    fn sequencer(seed: u64) -> Sequencer {
        let mut seq = Sequencer::new(RATE, seed);
        seq.patterns.push(Pattern::default()); // 16 steps of 1/16
        seq
    }

    fn kick_lane() -> Lane {
        let mut lane = Lane::new(0, 36, 16);
        for step in &mut lane.steps[0..4] {
            *step = Step::hit(0.8);
        }
        lane
    }

    #[test]
    fn timing_at_120bpm_sixteenths() {
        let mut seq = sequencer(1);
        seq.patterns[0].lanes.push(kick_lane());
        seq.start(0.0);
        // step 0 at frame 0, step 1 at 5512.5 -> frame 5512
        let events = seq.poll(20_000);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].frame, 0);
        assert_eq!(events[1].frame, 5512, "cursor 1 starts at 5512.5");
        assert_eq!(events[2].frame, 11025, "cursor 2 at 2*5512.5");
        assert_eq!(events[3].frame, 16537, "cursor 3 at 3*5512.5");
        assert_eq!(events.iter().map(|e| e.instrument_id).collect::<Vec<_>>(), vec![0; 4]);
    }

    #[test]
    fn poll_continues_across_block_boundaries() {
        let mut seq = sequencer(2);
        seq.patterns[0].lanes.push(kick_lane());
        seq.start(0.0);
        let block_a = seq.poll(10_000);
        let block_b = seq.poll(20_000);
        let block_c = seq.poll(30_000);
        let frames: Vec<u64> = block_a
            .iter()
            .chain(&block_b)
            .chain(&block_c)
            .map(|e| e.frame)
            .collect();
        // kick_lane enables steps 0..4 only -> 4 events in total
        assert_eq!(frames, vec![0, 5512, 11025, 16537]);
        assert!(frames.windows(2).all(|w| w[0] <= w[1]), "sorted by frame");
    }

    #[test]
    fn empty_poll_does_not_lose_events() {
        let mut seq = sequencer(3);
        seq.patterns[0].lanes.push(kick_lane());
        seq.start(0.0);
        // zero-width polls cannot fire anything and must not consume steps
        let _ = seq.poll(0);
        let _ = seq.poll(0);
        let events = seq.poll(6000);
        // poll(6000) covers steps 0 (frame 0) and 1 (frame 5512)
        assert_eq!(events.len(), 2, "steps continued correctly");
        assert_eq!(events[0].frame, 0);
        assert_eq!(events[1].frame, 5512);
    }

    #[test]
    fn swing_shifts_odd_steps() {
        let mut straight = sequencer(4);
        let mut swung = sequencer(4);
        for seq in [&mut straight, &mut swung] {
            seq.patterns[0].lanes.push(kick_lane());
            seq.start(0.0);
        }
        swung.patterns[0].swing = 0.6;
        let a = straight.poll(12000);
        let b = swung.poll(12000);
        // step 0 identical
        assert_eq!(a.len(), b.len());
        assert_eq!(a[0].frame, b[0].frame);
        // step 1 later
        assert!(b[1].frame > a[1].frame, "{} > {}", b[1].frame, a[1].frame);
        // shift = swing * 0.5 * step = 0.6 * 0.5 * 5512.5 = 1653.75
        assert_eq!(b[1].frame - a[1].frame, 1654);
        // even steps are untouched
        assert_eq!(a[2].frame, b[2].frame);
    }

    #[test]
    fn micro_offset_moves_within_half_step() {
        let mut seq = sequencer(5);
        let mut lane = Lane::new(0, 36, 16);
        lane.steps[0] = Step { micro_offset: 0.5, ..Step::hit(0.8) };
        seq.patterns[0].lanes.push(lane);
        seq.start(0.0);
        let events = seq.poll(11000);
        assert_eq!(events.len(), 1);
        // 0.5 * 5512.5 = 2756.25 -> 2756
        assert_eq!(events[0].frame, 2756);
    }

    #[test]
    fn humanize_is_deterministic_and_neutral_without_amount() {
        let mut a = sequencer(7);
        let mut b = sequencer(7);
        let mut c = sequencer(8);
        for seq in [&mut a, &mut b, &mut c] {
            let mut lane = Lane::new(0, 36, 16);
            for step in &mut lane.steps[0..4] {
                *step = Step::hit(0.8);
            }
            seq.patterns[0].lanes.push(lane);
            seq.patterns[0].humanize_timing = 1.0;
            seq.patterns[0].humanize_velocity = 0.5;
            seq.start(0.0);
        }
        let (ea, eb, ec) = (a.poll(20_000), b.poll(20_000), c.poll(20_000));
        assert_eq!(ea, eb, "same seed -> identical events");
        assert_ne!(ea[..], ec[..], "different seed -> different jitter");
        // velocities stay in the musical range
        for event in &ea {
            assert!(event.velocity >= MIN_VELOCITY && event.velocity <= 1.0);
            assert!(event.velocity > 0.4, "jitter is moderate: {}", event.velocity);
        }
    }

    #[test]
    fn probability_and_choke() {
        let mut seq = sequencer(9);
        let mut lane = Lane::new(0, 36, 16);
        lane.steps[0] = Step { probability: 0.0, ..Step::hit(0.8) };
        lane.steps[1] = Step { probability: 1.0, ..Step::hit(0.8) };
        lane.steps[2] = Step { choke: true, ..Step::hit(0.8) };
        seq.patterns[0].lanes.push(lane);
        seq.start(0.0);
        let events = seq.poll(20_000);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, Hit);
        assert_eq!(events[1].kind, Choke);
    }

    #[test]
    fn accent_ghost_scale_velocity() {
        let mut seq = sequencer(10);
        let mut lane = Lane::new(0, 36, 16);
        lane.steps[0] = Step { accent: true, ..Step::hit(0.8) };
        lane.steps[1] = Step { ghost: true, ..Step::hit(0.8) };
        lane.steps[2] = Step::hit(0.8);
        seq.patterns[0].lanes.push(lane);
        seq.start(0.0);
        let events = seq.poll(20_000);
        assert!(
            (events[0].velocity - 0.96).abs() < 1e-6,
            "accent gain 1.2 * 0.8"
        );
        assert!(
            (events[1].velocity - 0.32).abs() < 1e-6,
            "ghost gain 0.4 * 0.8"
        );
        assert!((events[2].velocity - 0.8).abs() < 1e-6);
    }

    #[test]
    fn flam_grace_before_hit() {
        let mut seq = sequencer(11);
        let mut lane = Lane::new(0, 36, 16);
        // flam on step 1: step 0's grace would clamp to frame 0
        lane.steps[1] = Step { flam: true, ..Step::hit(1.0) };
        seq.patterns[0].lanes.push(lane);
        seq.start(0.0);
        let events = seq.poll(6000);
        assert_eq!(events.len(), 2, "grace + main");
        let (grace, main) = (&events[0], &events[1]);
        assert!(grace.frame < main.frame);
        assert_eq!(grace.velocity, 0.4);
        assert_eq!(main.velocity, 1.0);
        // grace sits flam_time steps before the hit: 0.03 * 5512.5 = 165
        assert_eq!(main.frame - grace.frame, 165);
    }

    #[test]
    fn ratchet_extra_hits_evenly_spaced() {
        let mut seq = sequencer(12);
        let mut lane = Lane::new(0, 36, 16);
        lane.steps[0] = Step {
            repeat: Some(Repeat { count: 4, falloff: 0.5 }),
            ..Step::hit(1.0)
        };
        seq.patterns[0].lanes.push(lane);
        seq.start(0.0);
        let events = seq.poll(20_000);
        assert_eq!(events.len(), 4);
        let frames: Vec<u64> = events.iter().map(|e| e.frame).collect();
        // spacing = step / count = 5512.5 / 4 = 1378.125 frames
        assert_eq!(frames, vec![0, 1378, 2756, 4134]);
        // falloff halves the velocity each ratchet
        assert_eq!(events[3].velocity, 0.125);
    }

    #[test]
    fn internal_tempo_fallback_beats_bad_host_values() {
        let mut seq = sequencer(13);
        assert_eq!(seq.tempo(), 120.0);
        seq.host_sync = true;
        seq.host_tempo = Some(90.0);
        assert_eq!(seq.tempo(), 90.0);
        seq.host_tempo = Some(0.0);
        assert_eq!(seq.tempo(), 120.0, "broken host tempo -> internal");
        seq.host_tempo = Some(-3.0);
        assert_eq!(seq.tempo(), 120.0);
    }

    #[test]
    fn tempo_changes_shift_upcoming_steps() {
        let mut seq = sequencer(14);
        seq.patterns[0].lanes.push(kick_lane());
        seq.start(0.0);
        // at 120 bpm: steps 0 and 1 (frame 0, 5512)
        let first = seq.poll(6000);
        assert_eq!(first.len(), 2);
        seq.host_sync = true;
        seq.host_tempo = Some(60.0);
        // upcoming steps land at the new tempo: cursor 2 -> 0.5 beats at
        // 44100 frames per beat = 22050
        let second = seq.poll(30_000);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].frame, 22050);
    }

    #[test]
    fn chain_mode_plays_patterns_in_order() {
        let mut seq = sequencer(15);
        let mut lane_b = Lane::new(0, 36, 16);
        lane_b.steps[0] = Step::hit(1.0);
        seq.patterns.push(Pattern {
            name: "B".into(),
            division: Division::Quarter,
            lanes: vec![lane_b],
            ..Pattern::default()
        });

        seq.chain = vec![
            ChainEntry { pattern: 0, repeats: 1 },
            ChainEntry { pattern: 1, repeats: 1 },
        ];
        seq.loop_chain = false;
        seq.patterns[0].lanes = vec![kick_lane()];
        seq.start(0.0);

        let a = seq.poll(10_000); // pattern A steps 0..1
        assert_eq!(a.len(), 2);
        assert_eq!(seq.chain_position(), Some(0));
        // pattern A fully through: 16 * 5512.5 = 88_200 frames
        let b = seq.poll(90_000); // A steps 2..3 + B step 0
        assert_eq!(b.len(), 3);
        assert_eq!(b[2].frame, 88_200);
        assert_eq!(seq.chain_position(), Some(1), "advanced to B");
        // The chain is exhausted once B finishes (20 beats = 441_000);
        // B lanes besides step 0 are off, so nothing is audible anymore.
        let c = seq.poll(500_000);
        assert!(c.is_empty(), "B's remaining steps are disabled");
        assert!(!seq.is_playing(), "chain done without looping");
    }

    #[test]
    fn chain_repeats_respected() {
        let mut seq = sequencer(17);
        let mut lane = Lane::new(0, 36, 16);
        lane.steps[0] = Step::hit(1.0);
        seq.patterns[0].lanes.push(lane);
        // pattern 0 plays twice before the chain moves on
        seq.chain = vec![ChainEntry { pattern: 0, repeats: 2 }];
        seq.loop_chain = false;
        seq.start(0.0);
        // first pass starts at 0, the second one at 16 * 5512.5 = 88_200
        let events = seq.poll(90_000);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].frame, 88_200);
        assert_eq!(seq.chain_position(), Some(0), "repeats keep the entry");
        assert_eq!(seq.current_pattern_index(), 0);
        // the second pass plays out; then the chain is exhausted
        let tail = seq.poll(180_000);
        assert!(tail.is_empty(), "no enabled steps after the second pass");
        assert!(!seq.is_playing(), "repeats counted, chain done");
    }

    #[test]
    fn chain_loops_forever_by_default() {
        let mut seq = sequencer(16);
        seq.patterns.push(Pattern::default());
        seq.chain = vec![
            ChainEntry { pattern: 0, repeats: 1 },
            ChainEntry { pattern: 0, repeats: 1 },
        ];
        seq.patterns[0].lanes = vec![kick_lane()];
        seq.start(0.0);
        // full chain = 2 * 16 * 5512.5 = 176_400 frames
        let events = seq.poll(180_000);
        assert!(seq.is_playing(), "loop_chain keeps it running");
        assert_eq!(seq.chain_position(), Some(0), "wrapped around");
        // 8 scheduled kicks + step 0 of the next wrap
        assert_eq!(events.len(), 9);
    }

    #[test]
    fn serialization_roundtrip() {
        let mut pattern = Pattern::default();
        pattern.lanes.push(Lane::new(3, 40, 16));
        pattern.lanes[0].steps[0] = Step {
            enabled: true,
            velocity: 0.55,
            probability: 0.8,
            micro_offset: -0.25,
            articulation: Some("rim".into()),
            flam: true,
            ..Step::default()
        };
        pattern.lanes[0].steps[1] = Step {
            repeat: Some(Repeat { count: 3, falloff: 0.6 }),
            ..Step::hit(0.9)
        };
        let json = serde_json::to_string(&pattern).expect("serialize");
        let back: Pattern = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, pattern);
    }
}
