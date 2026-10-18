# Decisions and honest blockers

Decisions made while building drumgizmo-rust, each with its reason.
Blockers live at the bottom and are never hidden: if something does not
work yet, it says so here.

## Engine and kit semantics

* **`main="true"` channel semantics follow upstream.** The attribute
  lives on `<channelmap>` elements inside instruments (not on the
  drumkit channel list). When any channel of an instrument explicitly
  declares a main state, every other channel defaults to not-main.
* **Master gain byte velocity scale.** MIDI-in events (hosts, MIDI
  devices) carry the 1..=127 byte scale; the engine maps OnSet velocity
  via the upstream `(v - 0.5)/127` centre transform. CLAP note events
  (0..1) are converted through `dg_plugin::velocity_to_byte` so both
  paths stay identical.
* **Chokes are polyphonic pressure (`0xA0`, data1 > 0)** — the
  DrumGizmo convention. MIDI channel numbers are ignored everywhere:
  `midimap`, not the channel, routes drums (upstream behaviour).
* **`render_stereo` = main channels → L+R**, with `Pair(0,1)` fallback
  for kits without main markers. The `MainSum` kitgen flavour pins the
  "explicit main ⇒ others are not main" rule in a test.
* **kit_generation guard** on voice spawning: a kit swap mid-block
  cannot leave voices pointing at freed sample memory; `mem::take`
  scratch snapshots avoid head blocking.

## Sequencer

* **SeqTempo now sets `internal_tempo`.** Erratum fixed: earlier the
  command only wrote `host_tempo` while `host_sync == false` left
  `tempo()` reading the untouched internal value, so GUI tempo drags
  did nothing. Hosts driving sample positions never send SeqTempo, so
  "explicit tempo command ⇒ internal clock" is the consistent rule.
* **Pattern edits flush on drag-end** (not per pixel), matching the
  command ring's small capacity and keeping the audio side from
  re-hydrating JSON every frame.

## Wrappers

* **Params**: 0 = master gain 0.0..8.0 linear, 1 = tempo 30..360 BPM,
  both automatable in every ABI. VST3 exposes them normalized with
  linear plain mapping.
* **State**: JSON `PluginState` v1 blob (kit path, gains, tempo,
  pattern, transport) over the CLAP state extension and VST3
  `IBStream` streams. `state_load` distinguishes read errors from EOF
  and refuses blobs above 4 MiB.
* **CLAP dialect events**: NoteOn/NoteOff/Choke from the CLAP dialect,
  plus raw MIDI bytes decoded through the shared
  `dg_common::decode_midi_status_bytes` (identical semantics to the
  audio_io path; there is no wrapper duplicated code).
* **LV2 hand-rolled ABI**: the atom:Sequence reader mirrors
  `lv2_atom_pad_size`/`is_end` exactly (64-bit aligned events, region
  `[body + 8, body + atom.size)`, 16-byte event headers). `urid:map`
  is resolved at instantiate; without it (non-spec host) every event
  body starting with a status byte is decoded.
* **Shared producer**: one `rtrb` ring per engine, one `Producer`
  wrapped in `Arc<Mutex<_>>` for host-side senders (GUI, MIDI
  callback, CLI). The audio thread owns the consumer and never takes
  the lock; the only host-side contention is GUI ↔ MIDI, both brief
  ring pushes.
* **Linux plugin bundles**: `cp -r` staged bundles land in
  `~/.lv2`, `~/.vst3`, `~/.clap` via `scripts/install_local.sh`. The
  LV2 TTL and VST3 `moduleinfo.json` ship in-repo next to the wrapper
  sources.

## Testing without a sound card

`cargo test --workspace` runs on machines with no audio hardware: the
`null` host covers stream code, GUI tests run headless, engine tests
assert rendered samples, and the audio_io glue test builds a kit via
`kitgen` in temp dirs. Nothing mocks hardware it cannot see.

## Blockers (honest)

1. ~~**GitHub push waits for a PAT.**~~ **RESOLVED.** PAT was provided;
   `main` is pushed to `https://github.com/Pliexi/drumgizmo-rust`
   (verified via `git ls-remote`: local HEAD == `refs/heads/main`).
2. **Host smoke tests: LV2, VST3 and CLAP verified.** With
   `carla-discovery-native` 2.5.10, `pw-jack jalv` (PipeWire JACK
   shim, no sound card needed) and the free-audio `clap-info`
   validator (built from upstream source, 1.2.10):
   - LV2: discovered as synth (2 audio out, 1 midi in, 2 params) and
     instantiated through jalv for 15 s headless.
   - VST3: discovered with the same layout (works only when the module
     inside `.vst3/Contents/<arch>-linux/` is named after the bundle
     stem — `build.sh` now stages `drumgizmo-rust.so` for that reason).
   - CLAP: `clap-info` loads the binary, enumerates the descriptor
     `com.pliexi.drumgizmo-rust` (features instrument/stereo), checks
     audio-ports/note-ports/params and performs a state write/read
     roundtrip — exit 0, empty stderr. What has *not* been run: an
     audio-rendering session inside a DAW (needs carla 2.6+/reaper +
     desktop), so "validated by the reference loader" is the honest
     ceiling here.
3. **VST3 GUI missing.** `IEditController::createView` returns null;
   hosts will show controls but no custom editor (parameter automation
   still works).
4. **CLAP/LV2/VST3 state`:** kit path stored as string; relaunching on
   a machine without the kit directory yields an empty load and a
   warning, not an error.
5. **VST3 tail samples report 0** — the engine has no tail length
   estimation yet; voice rampdown exists, bounce-optimization does not.
6. **No PW/ALSA device enumeration options in the CLI** —
   `drumgizmo-rust` uses the default output device; per-device
   selection is a TODO that the `AudioError` messages make visible.
7. **`--list-midi` under Wayland-only systems without a MIDI client**
   returns an empty list (midir connects via ALSA virtual ports
   through PipeWire when configured; nothing for us to fix, but the
   GUI cannot create virtual ports either — a real missing feature).
