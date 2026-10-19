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

## GUI

* **Keyboard ownership is explicit.** Global shortcuts (Space, 1–9,
  Ctrl+Z/Y) only fire when no text field has focus — the unconditional
  shortcut handler used to grab digits into the lane selectors and
  toggle the transport on Space while the user typed a kit path, which
  made copy/pasting look broken. The Kit tab now also offers a
  paste-from-clipboard button (arboard), an inline directory browser
  and drag&drop, because a Wayland clipboard bridge can be silent even
  when the app is healthy.
* **The sequencer is a home-built Channel Rack.** No ready-made egui
  drum-rack widget covers this app's semantics (shared lane model with
  velocity accents/ghosts/flam, chain bank, engine-driven playhead), so
  the grid is implemented directly on egui primitives: one allocated
  cell response per step, fixed left column, spacing-profile ruler that
  reuses the exact same spacing math as the rows, and a playhead fed by
  `EngineWatch` (engine-side atomics, polled — no reverse command
  channel, no locks on the audio path).
* **Silence is surfaced, never silent-invisible.** Kits load through
  the ring exactly like every other command; when a load fails the
  engine sets `EngineWatch.kit_error` and the GUI status bar shows
  `⚠ no kit loaded` instead of only a stderr line nobody watches. The
  standalone additionally passes an `AudioStatusSnapshot` so the bar
  distinguishes "streaming 48 kHz / 2 ch" from "no device".
* **Demo beat on kit load.** A loaded kit with an empty pattern seeds a
  classic one-bar groove (kick 0/10, snare 4/12, closed hats 2/6/10/14,
  name-matched with deterministic fallbacks, unit-tested) — the first
  user action after `Load Kit` is audibly rewarded.

## Wrappers

* **Windows port is compile-guarded, not DAW-validated.** The vst3
  crate's ABI constants are `i32` on Windows but `u32` on Linux; the
  wrapper normalizes every comparison with a targeted
  `#[allow(clippy::unnecessary_cast)]` on the one ABI-touching `fn`,
  and CI runs `cargo check + clippy` on `windows-latest`. The Release
  workflow ships `.exe` + `.vst3 (Contents\\x86_64-win\\drumgizmo-rust.vst3)`
  + `.clap (dll)` + `.lv2 (dll, TTL rewritten)` bundles. Nobody here has
  run a Windows DAW, so treat the Windows artifacts as
  build-verified-host-unverified.
* **Distro packages** (deb, rpm, pacman `pkg.tar.zst`) are produced by
  nfpm from one `packaging/nfpm.yaml`: binary into `/usr/bin`, plugins
  into `/usr/lib/{lv2,vst3,clap}`, run guide into doc paths. System
  paths make the plugin visible to DAWs without manual copying.

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

## Infrastructure

* **CI** (`.github/workflows/ci.yml`) runs on ubuntu-22.04 on every
  push: `cargo fmt --check`, clippy with `-D warnings`, the full test
  suite, the release build, and a distributable zip artifact
  (standalone binary + all three plugin bundles).
* **Releases** (`.github/workflows/release.yml`) fire on `v*` tags and
  publish a zip containing the standalone app, the LV2/VST3 bundles,
  the CLAP binary and the run guide — the "download and run" path in
  `docs/run_guide.md` is produced by this workflow (`v0.1.0` exists).
* The third-party CLAP validation was done with `clap-info` built from
  the free-audio upstream source (C, external tool, not part of this
  repository).

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

### v0.1.3

1. **CLAP + VST3 moved onto nice-plug (a ready-made wrapper), GUIs
   included.** The user asked for the editor to actually appear in
   hosts. Rather than hand-rolling both wrappers a second time (the
   v0.1.x ones had no editor), we chose the maintained RustAudio
   framework `nice-plug` (0.4.2, the maintained fork of NIH-plug) and
   `nice-plug-egui` (0.6.0): both are pure Rust, depend on the exact
   versions we already pin (clap-sys 0.5, vst3 0.3 bindings, egui
   0.37), and provide the CLAP/VST3 std-C entry machinery, state
   persistence, and egui baseview embedding. The `dg-clap-plugin` /
   `dg-vst3-plugin` crates are now ~15-line export shims; all of the
   logic lives in one place, `dg_plugin::nih`. The LV2 wrapper stayed
   hand-written (lv2-sys) and GUI-less — that is a documented gap, not
   a regression.
2. **The host sees zero automatable parameters — all state rides in
   `#[persist]` fields** (kit path, pattern JSON, master gain, tempo).
   Reason: nice-plug exposes no sanctioned way for the plugin's own
   GUI to write parameter values (the only supported path is a
   `ParamSetter` built from a host callback plugins don't receive);
   carrying an out-of-sync automatable gain into host-saved state
   would be worse than not automating it — drum gain automation is
   normally done with the DAW track fader anyway. Honest consequence:
   the (nice-plug) `clap.audio-ports-config` extension is advertised
   unconditionally, and `clap-info`'s JSON writer crashes on the
   spec-valid *null* `main_input_port_type` (no input port). The
   local clap-info build used for validation was patched by that
   two-line null guard; hosts are not affected.
3. **VST3 class IDs changed.** v0.1.2 registered two legacy classes
   (processor CID `5D4B52F0-…`, controller `8F87F495-…`). nice-plug
   exports the modern single combined "Audio Module Class" with CID
   `6472756D-…` (`*b"drumgizmo-rustV3"`), moduleinfo.json updated.
   Old DAW sessions with the v0.1.2 VST3 will not restore instances —
   documented in the run guide, cannot be avoided when switching to a
   different wrapper generation.
4. **Host tempo outranks the GUI tempo, but only while the host
   transport is playing.** When playing, `transport.tempo` syncs the
   sequencer; when stopped, the GUI's tempo slider rules. Persisted
   tempo is always the GUI's value.
5. **cpal output now accepts more sample formats (the "hit is silent"
   fix).** `OutputSetup::choose()` used to accept only f32 configs;
   many devices (e.g. the EVO4 USB interface field-tested) offer i16
   or a broken 1-channel f32 map, so the standalone silently ran with
   no stream. The chooser is now hardware-first (tier 1: any format
   with a stereo-or-wider config; tier 2: mono fallback) and a single
   generic callback converts rendered f32 samples to the device
   format (`cpal::FromSample<f32>`). `--list-audio` (new) prints the
   full device/config inventory for diagnosis; the per-format tiering
   is unit-tested without hardware.
6. **The kit file chooser is `rfd` 0.15 (xdg-portal backend), pure
   Rust and free of GTK3 system deps**, so the Linux CI/runtime deps
   did not grow. If a portal host is unavailable (some bare WMs), the
   button reports a status hint and the manual path entry still works
   — no hard failure.
