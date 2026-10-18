# Architecture

DrumGizmo re-implementation in pure Rust. One cargo workspace, thirteen
crates, zero C/C++ in the tree; all system-level needs (PipeWire/ALSA,
MIDI devices, host plugin ABIs, OpenGL) go through library FFI
(`cpal`, `midir`, `clap-sys`, hand-rolled LV2/VST3 COM structs,
`egui/eframe`).

## Crate map

```
dg-common      shared types: RtCommand ring protocol, MidiEvent, SharedProducer
dg-kit         full DrumGizmo kit format (drumkit.xml + midimap.xml);
               async KitLoader (poll → decoded+resampled samples);
               kitgen test-kit generator (Small / Vintage / MainSum flavours)
dg-sampler     voice allocation: choke rampdown, voice limit, per-sample
               layer selection (velocity layers, round robin)
dg-sequencer   pattern model + sample-accurate timing engine
dg-midi        SMF import/export (midly) and pattern conversion
dg-engine      realtime core: block loop, kit lifecycle, sequencer drive,
               render_stereo main-channel mixdown, kit generation guard
dg-audio-io    cpal output stream (engine lives in the callback),
               midir input → RtCommand::Midi, raw MIDI decode
dg-gui         eframe app: Kit / Sequencer / Inspector tabs, undo/redo,
               MIDI SMF in/out, dirty-push pattern commands
dg-plugin      host-agnostic PluginCore (owned engine pair, lock-free
               command producer, PluginState JSON v1) — the wrapper base
dg-clap-plugin clap_entry; params/state/note/audio-ports extensions
dg-lv2-plugin  hand-rolled LV2 ABI + atom:Sequence MIDI parsing + TTL bundle
dg-vst3-plugin VST3 COM classes (processor + edit controller) via vst3 bindings
dg-standalone  CLI + cpal stream + MIDI input + GUI window
```

Dependency direction: `common ← {kit, mixer?}` — strictly one-way;
wrappers depend on `dg-plugin`, never on each other.

## Data flow per audio block

1. **Commands in.** The GUI, MIDI callback and standalone CLI share one
   `rtrb` ring through `SharedProducer` (`Arc<Mutex<Producer>>`; the
   audio callback side never takes the mutex). The engine drains the
   ring at the top of every block.
2. **Sequencer clocks.** When active the sequencer produces
   sample-accurate note events (with swing/humanize and micro timing)
   for this block's delta.
3. **Voice dispatch.** Each note event selects sample layers by
   velocity/round-robin, spawns voices into the pool (`dg-sampler`),
   applies choke groups and the -1 voice limit.
4. **Mixdown.** Active voices render into the kit's channel plan;
   `render_stereo` mixes the upstream-relevant `main="true"` channels
   into L+R (MainSum flavour exercises "any explicit main ⇒ others are
   not main", channelmap-based).
5. **Out.** `dg-audio-io` scatter-writes planar engine scratch into the
   host buffer (mono/stereo/surround layouts), `StreamStats` tracks a
   CAS-protected peak and failure flag.

## Kit loading

`KitLoader::start(path, samplerate)` decodes every sample channel (WAV,
FLAC, AIFF through symphonia) in a background thread, resamples with
rubato when the file rate differs, and hands the finished `Kit` to the
engine inside `apply_command(LoadKit)`. The engine keeps the previous
kit alive until the swap completes (no drop of voices mid-block); a
generation counter invalidates voices pending against the old kit.

## Wrapper parity

All three plugin wrappers use `dg_plugin::PluginCore` and therefore get
the same behaviour: engine pair re-created on host samplerate change
with full state replay, JSON `PluginState` v1, parameters 0 = master
gain (0..8) and 1 = tempo (30..360 BPM), note events in the 1..=127
byte velocity scale, block-relative offsets.

* CLAP: `clap_entry`, params/state/note-ports/audio-ports extensions.
* LV2: `lv2_descriptor` accessor, atom:Sequence MIDI input, two control
* ports, TTL bundle in `crates/lv2_plugin/drumgizmo-rust.lv2`.
* VST3: processor + edit controller classes, `IBStream` JSON state,
  `moduleinfo.json` bundle in `crates/vst3_plugin/drumgizmo-rust.vst3`.

## Concurrency contract

* The audio callback owns the engine; it only sees `&mut Engine`.
* Host threads only touch `Producer` (mutexed) and `PluginState`
  snapshots; the engine stays the authority on actual playback.
* Kit decode happens off-thread on a plain `JoinHandle`+polled
  mailbox; no locks shared with the audio thread.
* Test runtime: all tests avoid sound cards entirely (the `null` host,
  headless GUI tests, rendered-sample assertions).
