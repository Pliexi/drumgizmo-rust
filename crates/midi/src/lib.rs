// SPDX-License-Identifier: GPL-3.0-or-later
//! Standard MIDI File (SMF) import/export for the sequencer.
//!
//! Import merges all tracks (SMF type 0/1) into a tick-ordered note list
//! with the file's metrical timing. Export writes a GM-drums style type 0
//! file (all hits on channel 10, i.e. `u4 9`) with a tempo meta event —
//! exactly what DrumGizmo consumes: note-ons trigger instruments,
//! aftertouch on a note triggers a choke, note-offs are ignored.
//!
//! Known lossy areas (documented in docs/decisions.md):
//! * flam graces and ratchet extras quantize onto their host step on
//!   import and are de-duplicated to the loudest hit,
//! * chokes roundtrip as aftertouch events placed on the same grid step.

use dg_sequencer::{Division, Lane, Pattern, Step};

/// Failure of MIDI parsing/writing.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiError(pub String);

impl std::fmt::Display for MidiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "midi: {}", self.0)
    }
}

impl std::error::Error for MidiError {}

/// One note-on extracted from a MIDI file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteOn {
    pub channel: u8,
    pub note: u8,
    /// Raw 7-bit velocity (1..=127).
    pub velocity: u8,
    /// Absolute tick within the file.
    pub tick: u32,
    /// After-touch on the same note (choke trigger for DrumGizmo).
    pub choke: bool,
}

/// Parsed contents of a MIDI file (merged over all tracks).
#[derive(Debug, Clone, PartialEq)]
pub struct MidiFile {
    /// Ticks per quarter note (metrical timing required).
    pub ticks_per_beat: u32,
    /// Note-ons and chokes in file order.
    pub notes: Vec<NoteOn>,
    /// (tick, bpm) tempo changes at their tick positions.
    pub tempos: Vec<(u32, f64)>,
}

impl MidiFile {
    /// Last used tick + 1 (musical length estimate).
    pub fn length_ticks(&self) -> u32 {
        self.notes.iter().map(|n| n.tick).max().unwrap_or(0) + 1
    }

    /// Effective tempo at `tick` (the last tempo event before it);
    /// defaults to 120 bpm.
    pub fn tempo_at(&self, tick: u32) -> f64 {
        let mut bpm = 120.0;
        for (t, b) in &self.tempos {
            if *t <= tick {
                bpm = *b;
            } else {
                break;
            }
        }
        bpm
    }
}

/// Parses SMF bytes, merging note events of all tracks by tick.
pub fn parse_smf(bytes: &[u8]) -> Result<MidiFile, MidiError> {
    let smf = midly::Smf::parse(bytes).map_err(|e| MidiError(e.to_string()))?;
    let ticks_per_beat = match smf.header.timing {
        midly::Timing::Metrical(tpq) => tpq.as_int() as u32,
        _ => return Err(MidiError("only metrical timing is supported".into())),
    };

    let mut notes: Vec<NoteOn> = Vec::new();
    let mut tempos: Vec<(u32, f64)> = Vec::new();

    for track in &smf.tracks {
        let mut tick: u32 = 0;
        for event in track {
            tick += event.delta.as_int() as u32;
            match &event.kind {
                midly::TrackEventKind::Midi { channel, message } => match message {
                    midly::MidiMessage::NoteOn { key, vel } => {
                        if vel.as_int() > 0 {
                            notes.push(NoteOn {
                                channel: channel.as_int(),
                                note: key.as_int(),
                                velocity: vel.as_int(),
                                tick,
                                choke: false,
                            });
                        }
                    }
                    midly::MidiMessage::Aftertouch { key, vel } => {
                        // DrumGizmo convention: aftertouch on a note chokes it.
                        if vel.as_int() > 0 {
                            notes.push(NoteOn {
                                channel: channel.as_int(),
                                note: key.as_int(),
                                velocity: 0,
                                tick,
                                choke: true,
                            });
                        }
                    }
                    _ => {}
                },
                midly::TrackEventKind::Meta(midly::MetaMessage::Tempo(us_per_beat)) => {
                    let us = us_per_beat.as_int() as f64;
                    if us > 0.0 {
                        tempos.push((tick, 60_000_000.0 / us));
                    }
                }
                _ => {}
            }
        }
    }

    notes.sort_by_key(|n| n.tick);
    tempos.sort_by_key(|(t, _)| *t);
    tempos.dedup_by_key(|(t, _)| *t);

    Ok(MidiFile { ticks_per_beat, notes, tempos })
}

/// Tick resolution of the SMF export (quarter note).
pub const MIDI_TICKS_PER_BEAT: u16 = 960;

/// One requested MIDI note event for export.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportNote {
    /// 7-bit note number.
    pub note: u8,
    /// Velocity in `0.0..=1.0` (7-bit quantized on write).
    pub velocity: f32,
    /// Absolute tick (fractional ticks are rounded).
    pub tick: f64,
    /// Emit an aftertouch (choke) instead of a note-on.
    pub choke: bool,
}

/// Writes the notes as a GM-drum type 0 SMF file (all on channel 10);
/// velocities are 7-bit; fractional ticks are rounded.
pub fn write_smf(notes: &[ExportNote], ticks_per_beat: u16, tempo_bpm: f64) -> Result<Vec<u8>, MidiError> {
    if notes.iter().any(|n| n.tick < 0.0) {
        return Err(MidiError("negative tick".into()));
    }
    let tempo: u32 = (60_000_000.0 / tempo_bpm.max(1.0)).round() as u32;

    let mut sorted: Vec<&ExportNote> = notes.iter().collect();
    sorted.sort_by(|a, b| a.tick.total_cmp(&b.tick));

    let mut track: Vec<midly::TrackEvent> = Vec::new();
    let mut previous_tick: u32 = 0;
    for entry in sorted {
        let tick = entry.tick.round() as u32;
        let delta = tick.saturating_sub(previous_tick);
        previous_tick = tick;
        let message = if entry.choke {
            midly::MidiMessage::Aftertouch {
                key: midly::num::u7::new(entry.note),
                vel: midly::num::u7::new(127),
            }
        } else {
            let velocity = ((entry.velocity.clamp(0.0, 1.0) * 127.0).round() as u8).max(1);
            midly::MidiMessage::NoteOn {
                key: midly::num::u7::new(entry.note),
                vel: midly::num::u7::new(velocity),
            }
        };
        track.push(midly::TrackEvent {
            delta: midly::num::u28::new(delta),
            kind: midly::TrackEventKind::Midi {
                channel: midly::num::u4::new(9),
                message,
            },
        });
    }

    let mut smf = midly::Smf::new(midly::Header::new(
        midly::Format::SingleTrack,
        midly::Timing::Metrical(midly::num::u15::new(ticks_per_beat)),
    ));

    // Tempo meta first, then the notes: SMF permits both at tick 0.
    let mut head = vec![midly::TrackEvent {
        delta: midly::num::u28::new(0),
        kind: midly::TrackEventKind::Meta(midly::MetaMessage::Tempo(midly::num::u24::new(tempo))),
    }];
    head.extend(track);
    smf.tracks = vec![head];

    let mut out = Vec::new();
    smf.write_std(&mut out).map_err(|e| MidiError(e.to_string()))?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Pattern <-> MIDI mapping
// ---------------------------------------------------------------------------

/// Maps a MIDI note to a lane position (instrument) of a new pattern.
///
/// `midi_note(u8) -> Option<usize>` is usually the kit's MIDI map
/// (`LoadedKit::build_midi_map`); `None` skips the note.
pub fn pattern_from_midi(
    file: &MidiFile,
    length: usize,
    division: Division,
    upto_beats: f64,
    note_to_lane: impl Fn(u8) -> Option<usize>,
) -> Pattern {
    let mut pattern = Pattern {
        division,
        length,
        ..Pattern::default()
    };
    let ticks_per_step = file.ticks_per_beat as f64 * division.beats_per_step();
    if ticks_per_step <= 0.0 || length == 0 {
        return pattern;
    }
    let total_ticks = upto_beats * file.ticks_per_beat as f64;

    for note in &file.notes {
        if note.tick as f64 >= total_ticks {
            continue; // past the requested range
        }
        let Some(instrument_id) = note_to_lane(note.note) else {
            continue;
        };
        let lane_index = match pattern.lanes.iter().position(|l| l.instrument_id == instrument_id) {
            Some(index) => index,
            None => {
                let lane = Lane::new(instrument_id, note.note, length);
                pattern.lanes.push(lane);
                pattern.lanes.len() - 1
            }
        };

        // Grid position: nearest step + the fractional rest as micro offset.
        let step_position = note.tick as f64 / ticks_per_step;
        let step = step_position.round() as usize;
        if step >= length {
            continue; // beyond the pattern grid
        }
        let micro = (step_position - step_position.round()).clamp(-0.5, 0.5);

        let lane = &mut pattern.lanes[lane_index];
        if note.choke {
            lane.steps[step] = Step { enabled: true, choke: true, ..Step::default() };
            continue;
        }
        let velocity = (note.velocity as f32 / 127.0).clamp(0.05, 1.0);

        // De-duplicate to the loudest hit of the step (ratchet/flam extras
        // quantize onto the same step; their data is not reconstructable).
        let existing = &mut lane.steps[step];
        if existing.enabled && !existing.choke {
            if velocity <= existing.velocity {
                continue;
            }
        }
        *existing = Step {
            enabled: true,
            velocity,
            micro_offset: micro as f32,
            ..Step::default()
        };
    }

    pattern
}

/// Converts a pattern into exportable MIDI events.
///
/// Choke steps become aftertouch events; every other hit (including flam
/// graces and ratchet extras, with their timing) becomes a note-on. The
/// tick grid is [`MIDI_TICKS_PER_BEAT`].
pub fn pattern_to_midi(pattern: &Pattern) -> Vec<ExportNote> {
    let mut out = Vec::new();

    for lane in &pattern.lanes {
        if lane.midi_note == 0 {
            continue; // unassigned notes cannot be exported
        }
        let ticks_per_step =
            MIDI_TICKS_PER_BEAT as f64 * pattern.division.beats_per_step();
        for (step, step_def) in lane.steps.iter().enumerate() {
            if !step_def.enabled {
                continue;
            }
            let hit_tick = |offset_steps: f64| {
                (step as f64 + offset_steps) * ticks_per_step
                    + f64::from(step_def.micro_offset) * ticks_per_step
            };

            if step_def.choke {
                out.push(ExportNote {
                    note: lane.midi_note,
                    velocity: 0.0,
                    tick: hit_tick(0.0),
                    choke: true,
                });
                continue;
            }

            if step_def.flam {
                let grace = hit_tick(-f64::from(pattern.flam_time.max(0.0)));
                out.push(ExportNote {
                    note: lane.midi_note,
                    velocity: step_def.velocity * pattern.flam_velocity,
                    tick: grace.max(0.0),
                    choke: false,
                });
            }

            out.push(ExportNote {
                note: lane.midi_note,
                velocity: step_def.velocity,
                tick: hit_tick(0.0),
                choke: false,
            });

            if let Some(Repeat { count, falloff }) = step_def.repeat {
                for k in 1..count.max(1) as usize {
                    out.push(ExportNote {
                        note: lane.midi_note,
                        velocity: (step_def.velocity * falloff.powi(k as i32))
                            .clamp(0.01, 1.0),
                        tick: hit_tick(k as f64 / count.max(1) as f64),
                        choke: false,
                    });
                }
            }
        }
    }

    out.sort_by(|a, b| a.tick.total_cmp(&b.tick));
    out
}

use dg_sequencer::Repeat;

#[cfg(test)]
mod tests {
    use super::*;
    use dg_sequencer::Division::*;

    fn sample_pattern() -> Pattern {
        let mut pattern = Pattern {
            name: "RT".into(),
            division: Sixteenth,
            length: 16,
            ..Pattern::default()
        };
        let mut kick = Lane::new(0, 36, 16);
        kick.steps[0] = Step::hit(1.0);
        kick.steps[4] = Step::hit(0.8);
        let mut snare = Lane::new(1, 38, 16);
        snare.steps[2] = Step {
            enabled: true,
            velocity: 0.9,
            micro_offset: 0.25,
            ..Step::default()
        };
        let mut hat = Lane::new(2, 42, 16);
        hat.steps[1] = Step { choke: true, ..Step::hit(0.8) };
        pattern.lanes = vec![kick, snare, hat];
        pattern
    }

    #[test]
    fn export_import_roundtrip_grid_positions() {
        let pattern = sample_pattern();
        let notes = pattern_to_midi(&pattern);
        // 3 main hits + 1 choke
        assert_eq!(notes.len(), 4);

        let bytes = write_smf(&notes, MIDI_TICKS_PER_BEAT, 120.0).unwrap();
        let file = parse_smf(&bytes).unwrap();
        assert_eq!(file.tempo_at(0), 120.0);

        // Rebuild with the same note→lane mapping the kit would deliver.
        let back = pattern_from_midi(&file, 16, Sixteenth, f64::INFINITY, |note| match note {
            36 => Some(0),
            38 => Some(1),
            42 => Some(2),
            _ => None,
        });

        let lanes: Vec<(usize, Vec<u64>)> = back
            .lanes
            .iter()
            .map(|lane| {
                (
                    lane.instrument_id,
                    lane.steps
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.enabled)
                        .map(|(i, _)| i as u64)
                        .collect(),
                )
            })
            .collect();
        assert_eq!(lanes.len(), 3);
        // kick on steps 0, 4; snare on 2; hat choke on 1
        let kick = &lanes.iter().find(|l| l.0 == 0).unwrap().1;
        assert_eq!(kick, &[0, 4]);
        let snare = &lanes.iter().find(|l| l.0 == 1).unwrap().1;
        assert_eq!(snare, &[2]);
        let hat = &lanes.iter().find(|l| l.0 == 2).unwrap().1;
        assert_eq!(hat, &[1]);
        // choke survives
        let hat_lane = &back.lanes.iter().find(|l| l.instrument_id == 2).unwrap();
        assert!(hat_lane.steps[1].choke);
        assert!(hat_lane.steps[1].micro_offset.abs() < 1e-6, "no drift");
        // micro offset of the snare (+0.25 step) survives rounding
        let snare_lane = &back.lanes.iter().find(|l| l.instrument_id == 1).unwrap();
        assert!((snare_lane.steps[2].micro_offset - 0.25).abs() < 1e-3);
        // velocities survive 7-bit round-trip (worst case 1/127)
        assert!((snare_lane.steps[2].velocity - 0.9).abs() < 1.0 / 127.0 + 1e-3);
    }

    #[test]
    fn tempo_change_meta_is_parsed() {
        let notes = vec![ExportNote { note: 36, velocity: 0.8, tick: 0.0, choke: false }];
        let bytes = write_smf(&notes, 480, 90.0).unwrap();
        let file = parse_smf(&bytes).unwrap();
        assert_eq!(file.ticks_per_beat, 480);
        // tempo quantizes to microseconds per beat on the wire
        assert!((file.tempo_at(0) - 90.0).abs() < 1e-3, "{}", file.tempo_at(0));
    }

    #[test]
    fn pattern_to_midi_includes_flam_and_ratchet() {
        let mut pattern = Pattern::default();
        let mut lane = Lane::new(0, 36, 4);
        // step 1: step 0's grace would clamp to tick 0
        lane.steps[1] = Step {
            flam: true,
            repeat: Some(Repeat { count: 3, falloff: 0.5 }),
            ..Step::hit(1.0)
        };
        pattern.lanes = vec![lane];
        let notes = pattern_to_midi(&pattern);
        assert_eq!(notes.len(), 4, "grace + main + 2 ratchets (count=3)");
        let ticks: Vec<f64> = notes.iter().map(|n| n.tick).collect();
        assert!(ticks.windows(2).all(|w| w[0] <= w[1]));
        // 1/16 step = 240 ticks at 960 tpq
        assert!((ticks[0] - (240.0 - 0.03 * 240.0)).abs() < 1e-5, "grace");
        assert!((ticks[1] - 240.0).abs() < 1e-9, "main");
        // ratchet spacing = step / count = 80 ticks apart
        assert!((ticks[2] - 320.0).abs() < 1e-9);
        assert!((ticks[3] - 400.0).abs() < 1e-9);
        assert!(ticks[0] < ticks[1]);
    }

    #[test]
    fn truncated_import_respects_upto_beats() {
        let pattern = sample_pattern();
        let bytes = write_smf(&pattern_to_midi(&pattern), 960, 120.0).unwrap();
        let file = parse_smf(&bytes).unwrap();
        // only one beat (4 steps at 1/16): kick step 4 sits at tick 960,
        // i.e. the start of beat 2, and must be dropped
        let short = pattern_from_midi(&file, 16, Sixteenth, 1.0, |note| match note {
            36 => Some(8), // kick gets its own lane
            _ => Some(7),  // hat + snare share a lane
        });
        let lane_hits = |instrument: usize| -> Vec<usize> {
            short
                .lanes
                .iter()
                .find(|l| l.instrument_id == instrument)
                .map(|l| {
                    l.steps
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.enabled)
                        .map(|(i, _)| i)
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(lane_hits(8), vec![0], "kick step 4 dropped");
        assert_eq!(lane_hits(7), vec![1, 2], "choke + snare within beat 1");
    }

    #[test]
    fn bad_file_fails_cleanly() {
        let error = parse_smf(b"not a midi file at all");
        assert!(error.is_err());
    }
}
