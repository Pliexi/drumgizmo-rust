# Building, running and installing

## Requirements

* Rust 1.74+ (2021 edition workspace) via rustup
* Linux with PipeWire (or plain ALSA) for the standalone app
* `git` for pulling and pushing

Setup once:

```sh
scripts/setup.sh
```

## Build

```sh
scripts/build.sh            # cargo build --release + bundle staging
scripts/build.sh --debug    # dev profile
```

Artifacts land in `target/staged/`:

```
drumgizmo-rust              → standalone binary (target/release directly)
drumgizmo-rust.lv2/         → LV2 bundle (TTL + libdg_lv2_plugin.so)
drumgizmo-rust.vst3/        → VST3 bundle (moduleinfo.json + .so)
libdg_clap_plugin.so        → CLAP
```

## Run

```sh
scripts/run_standalone.sh --kit /path/to/kit --midi-in "MP3" --gain 1.5
```

CLI contract (`--help`):

```
USAGE:
    drumgizmo-rust [OPTIONS] [KIT_PATH]

OPTIONS:
    --kit <PATH>       initial kit directory (same as the positional argument)
    --midi-in <NAME>   MIDI input port name fragment; "default" uses the first port
    --gain <VALUE>     master gain 0.0..=8.0 (default 1.0)
    --scale <FACTOR>   UI scale override (default: platform DPI)
    --list-midi        list MIDI input ports and exit
    --help             this text
    --version          print the version and exit
```

No sound card: the audio path warns loudly on stderr and the GUI runs
silent (everything but rendering still works). GUI tabs:

* **Kit** — directory entry + load, per-instrument gain, Hit preview.
* **Sequencer** — 16-lane step grid, drag paint, right-click choke,
  bar markers, swing/humanize sliders, tempo, transport.
* **Inspector** — selected step velocity/probability/micro timing,
  ratchet and toggle flags.
* Shortcuts: Space = transport, Ctrl+Z/Y = undo/redo, 1-9 = lane
  select. MIDI SMF import/export from the Sequencer tab.

## Install as plugins

```sh
scripts/install_local.sh
```

copies the bundles into `~/.lv2/`, `~/.vst3/`, `~/.clap/` and the
binary into `~/.local/bin/`. Your DAW needs a plugin rescan afterwards.
(Host smoke checks still require `jalv`/`clap-info`/`carla-discovery`;
see docs/decisions.md blockers.)

## Tests

```sh
scripts/test.sh             # cargo test --workspace --all-features
cargo fmt --all             # formatting contract
cargo clippy --workspace --all-targets   # lint contract
```

All tests run without audio hardware (see docs/architecture.md).
