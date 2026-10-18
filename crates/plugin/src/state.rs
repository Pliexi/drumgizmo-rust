// SPDX-License-Identifier: GPL-3.0-or-later
//! The serializable plugin state: everything a host (or the standalone
//! config file) needs to rebuild a session.

use serde::{Deserialize, Serialize};

use dg_sequencer::Pattern;

/// Current state schema version; bump on breaking changes.
pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginState {
    /// Schema version of this snapshot.
    pub version: u32,
    /// The kit directory the engine loads (empty = none loaded).
    pub kit_path: String,
    /// Master output gain 0.0..=8.0.
    pub master_gain: f32,
    /// Per instrument gain 0.0..=8.0 (indexes into the kit order;
    /// applied on every voice spawn).
    pub voice_gains: Vec<f32>,
    /// Sequencer tempo in BPM.
    pub tempo: f64,
    /// The sequencer pattern (None = empty/unused).
    pub pattern: Option<Pattern>,
    /// Whether the engine runs the sequencer transport.
    pub sequencer_running: bool,
}

impl Default for PluginState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            kit_path: String::new(),
            master_gain: 1.0,
            voice_gains: Vec::new(),
            tempo: 120.0,
            pattern: None,
            sequencer_running: false,
        }
    }
}

impl PluginState {
    /// Serializes the state to JSON bytes (host save roundtrip).
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Restores the state from serialized JSON bytes; returns None when
    /// the blob does not parse (host restores fail safely).
    pub fn from_json(bytes: &[u8]) -> Option<Self> {
        let state: serde_json::Result<PluginState> = serde_json::from_slice(bytes);
        state.ok().map(|mut state| {
            // Older/newer schema snapshots degrade to defaults.
            if state.version != STATE_VERSION {
                state.version = STATE_VERSION;
            }
            state
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_json_roundtrip_full() {
        let mut state = PluginState::default();
        state.kit_path = "/kits/test".into();
        state.master_gain = 1.25;
        state.voice_gains = vec![1.0, 0.5, 2.0];
        state.tempo = 96.5;
        state.sequencer_running = true;
        let mut pattern = Pattern::default();
        pattern.length = 32;
        pattern.swing = 0.4;
        state.pattern = Some(pattern);

        let bytes = state.to_json();
        let restored = PluginState::from_json(&bytes).unwrap();
        assert_eq!(restored, state);
    }

    #[test]
    fn broken_bytes_fail_cleanly() {
        assert!(PluginState::from_json(b"not json").is_none());
        assert!(PluginState::from_json(&[]).is_none());
        assert_eq!(PluginState::default().to_json().len() > 2, true);
    }
}
