# drumgizmo-rust

[![CI](https://github.com/Pliexi/drumgizmo-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/Pliexi/drumgizmo-rust/actions/workflows/ci.yml)

DrumGizmo re-implementation in pure Rust: a multi-core kit sampler with
the upstream kit format (`drumkit.xml`/`midimap.xml`), velocity layers
and round robin, a sample-accurate step sequencer, egui GUI, and
plugin wrappers for CLAP, LV2 and VST3 — plus a standalone app.

Runs on **Linux and Windows x86_64**; every release tag publishes
zips, a `.deb`, an `.rpm` and a pacman package. See
[docs/run_guide.md](docs/run_guide.md) for the no-toolchain install.

## Quick start

```sh
scripts/setup.sh              # toolchain check + dependency fetch
scripts/build.sh              # release build + LV2/VST3/CLAP staging
scripts/run_standalone.sh --kit path/to/drumkit --midi-in default
```

Plugin install for DAWs:

```sh
scripts/install_local.sh      # ~\.lv2, ~\.vst3, ~\.clap, ~\.local/bin
```

Test suite (runs headless, no sound card needed):

```sh
scripts/test.sh
```

## Feature summary

* **Kit engine** — full upstream kit format support: channels with
  `main`/bleed relationships, instruments with velocity layers and
  samples, round robin, choketime, midimap note binding, metadata;
  background decoding (WAV/FLAC/AIFF via symphonia) with rubato
  resampling to the engine rate.
* **Sampler** — voice pool with per-kit voice limit, choke group
  rampdown, selection by velocity.
* **Sequencer** — lanes per midimap note, 16th grid with swing,
  humanize (timing + velocity), probability, micro timing, ratchets,
  pattern chaining/looping, SMF import/export in the GUI.
* **Engine** — lock-free command ring control (single realtime
  consumer), asynchronous kit swap with generation guarding,
  `render_stereo` main-channel mixdown.
* **Wrappers** — CLAP (`clap_entry`), LV2 (hand-rolled ABI + TTL), VST3
  (COM classes + moduleinfo bundle); identical PluginCore underneath
  with samplerate rebuild + state replay, JSON `PluginState` v1,
  parameters 0 = gain, 1 = tempo.
* **GUI** — egui/eframe: Kit loading, per-instrument gains with Hit
  preview, step grid editing (drag paint, right-click choke), undo,
  shortcuts, inspector for velocity/probability/micro flags.
* **Standalone** — CLI (`--kit`, `--midi-in`, `--gain`, `--scale`,
  `--list-midi`), cpal stream, MIDI input driver, silent fallback when
  no sound card exists.

## Documentation

* [docs/run_guide.md](docs/run_guide.md) — how to run on any machine: releases, distro builds, DAW setup
* [docs/architecture.md](docs/architecture.md) — crate map and data flow
* [docs/sequencer.md](docs/sequencer.md) — pattern model and timing
* [docs/build.md](docs/build.md) — build/run/install contract
* [docs/decisions.md](docs/decisions.md) — every design decision and the
  current honest blockers (host smoke tests for CLAP, VST3 GUI)

## License

GPL-3.0-or-later, like upstream DrumGizmo.
