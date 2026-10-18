// SPDX-License-Identifier: GPL-3.0-or-later
//! [`PluginCore`]: one realtime engine per plugin instance, re-created
//! when the host samplerate changes, driven by a lock-free command ring
//! and rendered per block through [`dg_engine::Engine::render_stereo`].

use dg_common::{MessageType, MidiEvent, RtCommand};
use dg_engine::Engine;
use dg_sequencer::Pattern;
use rtrb::Producer;

use crate::state::PluginState;

/// The shared plugin abstraction: owns the engine and the command
/// producer; wrapper traits only forward host calls here.
pub struct PluginCore {
    state: PluginState,
    engine: Option<Engine>,
    producer_unlocked: Producer<RtCommand>,
    samplerate: u32,
    /// Optimistic transport memory (commands only; the engine stays the
    /// authority on actual playback).
    sequencer_running: bool,
    /// Track which tempo was already pushed (hosts may send tempo per
    /// block; only changes are forwarded).
    last_pushed_tempo: f64,
    /// Commands already queued but not yet rendered matter for a fresh
    /// engine only through the state replay, so the previous rate's
    /// engine is simply owned until dropped.
    _engine_guard: (),
}

impl Default for PluginCore {
    fn default() -> Self {
        let (engine, producer) = dg_engine::engine_pair(44100, 256);
        // The engine exists from the start at 44.1 kHz; hosts usually
        // call set_samplerate before activating, which re-creates the
        // pair at their rate and replays the state.
        Self {
            state: PluginState::default(),
            engine: Some(engine),
            producer_unlocked: producer,
            samplerate: 44100,
            sequencer_running: false,
            last_pushed_tempo: 0.0,
            _engine_guard: (),
        }
    }
}

impl PluginCore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current state copy (host `getState`).
    pub fn state(&self) -> PluginState {
        self.state.clone()
    }

    /// Replaces the whole state (host `setState`): forwards the parts the
    /// realtime side needs when an engine already exists, otherwise they
    /// are replayed on activation.
    pub fn set_state(&mut self, state: PluginState) {
        self.state = state;
        if self.engine.is_some() {
            self.replay_state();
        }
    }

    /// Host samplerate. Re-creates the engine when necessary (the engine
    /// bakes its rate in); the state is replayed afterwards (kit load,
    /// gains, tempo, pattern, transport).
    pub fn set_samplerate(&mut self, samplerate: u32) {
        if self.engine.is_some() && self.samplerate == samplerate {
            return;
        }
        let (engine, producer) = dg_engine::engine_pair(samplerate as u64, 256);
        self.samplerate = samplerate;
        self.engine = Some(engine);
        self.producer_unlocked = producer;
        self.replay_state();
    }

    /// Re-applies the state to the current engine (call after swap).
    fn replay_state(&mut self) {
        if !self.state.kit_path.is_empty() {
            let path = self.state.kit_path.clone();
            self.push_command(RtCommand::LoadKit { path });
        }
        if !self.state.voice_gains.is_empty() {
            let gains: Vec<(usize, f32)> =
                self.state.voice_gains.iter().copied().enumerate().collect();
            for (instrument, gain) in gains {
                self.push_command(RtCommand::VoiceGain { instrument, gain });
            }
        }
        self.last_pushed_tempo = 0.0; // force tempo re-push
        self.set_tempo(self.state.tempo);
        if let Some(pattern) = self.state.pattern.clone() {
            self.set_pattern(pattern);
        }
        self.master_gain(self.state.master_gain);
        if self.state.sequencer_running {
            self.push_command(RtCommand::SeqStart);
        } else {
            self.push_command(RtCommand::SeqStop);
        }
        self.sequencer_running = self.state.sequencer_running;
    }

    fn push_command(&mut self, command: RtCommand) {
        let _ = self.producer_unlocked.push(command);
    }

    // ------------------------------------------------------------------
    // parameter setters (host thread only)
    // ------------------------------------------------------------------

    pub fn set_kit(&mut self, path: &str) {
        self.state.kit_path = path.to_string();
        self.push_command(RtCommand::LoadKit {
            path: path.to_string(),
        });
    }

    pub fn master_gain(&mut self, gain: f32) {
        self.state.master_gain = gain;
        self.push_command(RtCommand::MasterGain { gain });
    }

    /// Per-instrument gain (id from the kit order) or -1 for all.
    pub fn voice_gain(&mut self, instrument: i32, gain: f32) {
        if instrument == -1 {
            self.state
                .voice_gains
                .iter_mut()
                .for_each(|slot| *slot = gain);
        } else if let Some(slot) = self.state.voice_gains.get_mut(instrument as usize) {
            *slot = gain;
        }
        self.push_command(RtCommand::VoiceGain {
            instrument: instrument.max(0) as usize,
            gain,
        });
    }

    pub fn set_tempo(&mut self, bpm: f64) {
        self.state.tempo = bpm;
        if (bpm - self.last_pushed_tempo).abs() > f64::EPSILON {
            self.last_pushed_tempo = bpm;
            self.push_command(RtCommand::SeqTempo { bpm });
        }
    }

    pub fn set_pattern(&mut self, pattern: Pattern) {
        self.state.pattern = Some(pattern);
        if let Ok(json) = serde_json::to_string(&self.state.pattern.as_ref().unwrap()) {
            self.push_command(RtCommand::SeqPattern { json });
        }
    }

    pub fn seq_transport(&mut self, running: bool) {
        self.state.sequencer_running = running;
        self.sequencer_running = running;
        if running {
            self.push_command(RtCommand::SeqStart);
        } else {
            self.push_command(RtCommand::SeqStop);
        }
    }

    pub fn panic(&mut self) {
        self.push_command(RtCommand::Panic);
    }

    // ------------------------------------------------------------------
    // realtime processing (audio thread)
    // ------------------------------------------------------------------

    /// Renders one stereo block; `midi` events must carry block relative
    /// offsets. The engine's own command ring was fed from the host
    /// thread.
    pub fn process_stereo(&mut self, left: &mut [f32], right: &mut [f32], midi: &[MidiEvent]) {
        if let Some(engine) = &mut self.engine {
            engine.render_stereo(left, right, midi);
        } else {
            left.fill(0.0);
            right.fill(0.0);
        }
    }

    /// Direct raw MIDI shortcut for wrappers that decode bytes per event.
    pub fn midi_event(
        &mut self,
        kind: MessageType,
        note: u8,
        velocity: f32,
        offset: usize,
    ) -> MidiEvent {
        MidiEvent {
            kind,
            note,
            velocity,
            offset,
        }
    }

    /// Test/preview hook: build a MidiEvent in the byte velocity scale
    /// for a 0..=1 host velocity.
    pub fn velocity_byte_event(
        kind: MessageType,
        note: u8,
        host_velocity: f32,
        offset: usize,
    ) -> MidiEvent {
        MidiEvent {
            kind,
            note,
            velocity: crate::velocity_to_byte(host_velocity),
            offset,
        }
    }

    /// The samplerate the engine was built with.
    pub fn current_samplerate(&self) -> u32 {
        self.samplerate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dg_common::MessageType;

    #[test]
    fn default_core_is_44100_and_silent() {
        let mut core = PluginCore::new();
        assert_eq!(core.current_samplerate(), 44100);
        let mut left = vec![0.7f32; 64];
        let mut right = vec![-0.4f32; 64];
        core.process_stereo(&mut left, &mut right, &[]);
        assert!(left.iter().all(|s| *s == 0.0));
        assert!(right.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn samplerate_rebuild_keeps_commands_working() {
        let mut core = PluginCore::new();
        core.set_samplerate(48000);
        assert_eq!(core.current_samplerate(), 48000);
        // No kit yet: parameter flow must not block or panic.
        core.master_gain(0.5);
        core.set_tempo(90.0);
        core.set_pattern(Pattern::default());
        core.seq_transport(true);
        core.panic();
        // Same rate: the pair is kept (no errors) — call idempotent.
        core.set_samplerate(48000);
        core.voice_gain(-1, 0.7);
        core.set_kit(""); // empty path: still a valid command flow
    }

    #[test]
    fn velocity_byte_scale_matches_upstream_bounds() {
        assert_eq!(crate::velocity_to_byte(0.0), 1.0);
        assert_eq!(crate::velocity_to_byte(1.0), 127.0);
        let mid = crate::velocity_to_byte(0.5);
        assert!((64.0..=66.0).contains(&mid));
        // Event helper covers the same transform.
        let event = PluginCore::velocity_byte_event(MessageType::OnSet, 36, 0.8, 3);
        assert_eq!(event.velocity, 102.0);
        assert_eq!(event.offset, 3);
    }

    #[test]
    fn state_set_forwards_without_engine_swap() {
        let mut core = PluginCore::new();
        let state = PluginState {
            master_gain: 0.75,
            tempo: 140.0,
            ..PluginState::default()
        };
        core.set_state(state.clone());
        assert_eq!(core.state(), state);
    }
}
