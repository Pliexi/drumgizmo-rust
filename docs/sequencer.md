# Sequencer

The `dg-sequencer` crate: a pattern-driven drum sequencer with
sample-accurate scheduling inside the realtime engine block loop.

## Model

```text
Pattern
├── lanes[]            one per midimap note (kick, snare, …)
│   └── steps[]        16th notes per bar (variable length chains)
│       ├── active: bool
│       ├── velocity: 0.0..=1.0
│       ├── choke: bool          (note is a choke/hit-again marker)
│       ├── probability: 0.0..=1.0     (playing-time gate)
│       ├── micro_timing: beats offset (−0.25..=0.25 typical)
│       ├── ratchet: u16         (N subdivisions played in a row)
│       └── flags: rolls, flams, toggles…
├── swing: 0.0..=1.0   odd-step phase shift
└── humanize_timing: 0.0..=1.0   random ± offset per hit
```

* Steps own absolute positions inside a chain; `chain[]` orders
  patterns, `loop_chain` makes the whole sequence loop.
* The GUI serializes patterns as JSON `SeqSnapshot` and pushes it via
  `RtCommand::SeqPattern { json }` (dirty-push on drag-end only).
* SMF round trip: `dg-midi` converts `Pattern ↔ Standard MIDI File`
  through midly (ticks at fixed 480 PPQ step grid mapping).

## Timing

`timing.rs` keeps:

* `internal_tempo` (BPM) — the standalone/param tempo, 30..360 by CLI
  contract but the sequencer accepts any > 0 value;
* `host_tempo: Option<f64>` — host-provided tempo, used when
  `host_sync == true`;
* `tempo()` — picks host tempo when valid, falls back to internal
  (broken values like 0 or NaN never reached the cursor: guarded).

Scheduling walks the beat cursor per audio block:

1. `playhead_frames += frames_this_block` with tempo-conversion
   (BPM → frames per 16th at the engine samplerate);
2. all step positions falling inside the block's frame range are
   emitted with their exact block-relative sample offset;
3. tempo changes inside a block shift upcoming steps (the cursor is
   recomputed from the change frame — tests cover the shift behaviour).

`RtCommand::SeqTempo` sets `internal_tempo` (and clears `host_sync`) —
fixed erratum, see docs/decisions.md.

## Playing-time randomization

`probability` and `humanize` are resolved at trigger time inside the
engine (not while building events), so playback stays deterministic per
step except for the explicit random envelope; persisted patterns keep
raw values so SMF export stays faithful.

## Engine integration

`Engine::apply_command` owns the sequencer state transitions
(SeqStart/SeqStop/SeqTempo/SeqPattern) and `render_stereo` merges
sequencer-produced `MidiEvent`s with the incoming MIDI/CLAP/LV2/VST3
stream: choke groups and voice dispatch are identical for both paths.
