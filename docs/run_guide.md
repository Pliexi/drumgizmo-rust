# Run guide — drumgizmo-rust on any machine

This is the practical guide: get the app running on your machine, load
a DrumGizmo kit, play it from a keyboard, hear it through the speakers
or drop it into a DAW as a plugin.

Supported today: **Linux x86_64** (tested on Arch Linux with PipeWire;
CI builds on Ubuntu 22.04). Windows/macOS are not tested — the Rust
code itself is portable, but the plugin bundles and build scripts here
do not handle them yet.

---

## 1. Fastest path: prebuilt binaries (GitHub Releases)

If the **v0.1.0 release** is up, you need no Rust toolchain at all:

1. Go to <https://github.com/Pliexi/drumgizmo-rust/releases>
   (private repo: your GitHub account must have access).
2. Download `drumgizmo-rust-v0.1.0-linux-x86_64.zip`.
3. Unzip and run:

   ```sh
   unzip drumgizmo-rust-v0.1.0-linux-x86_64.zip
   cd drumgizmo-rust-v0.1.0-linux-x86_64
   ./drumgizmo-rust --help
   ```

4. To install the plugins into DAW-known paths, from the unpacked
   folder:

   ```sh
   mkdir -p ~/.lv2 ~/.vst3 ~/.clap ~/.local/bin
   cp -r drumgizmo-rust.lv2 ~/.lv2/
   cp -r drumgizmo-rust.vst3 ~/.vst3/
   cp libdg_clap_plugin.so ~/.clap/drumgizmo-rust.clap
   cp drumgizmo-rust ~/.local/bin/
   ```

5. Rescan plugins in your DAW (or restart it).

The binaries are built on Ubuntu 22.04 (glibc 2.35) so they run on any
recent-enough distro. If your glibc is older, use the source path
below — it takes about two minutes on a normal desktop.

---

## 2. Build from source

### 2.1 Toolchain

Install Rust (any recent stable release; 1.74+ is the minimum):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# then, in a new shell:
rustup update
```

### 2.2 Native packages

The code is pure Rust (no C/C++ from us), but it links system libraries
through FFI. Pick your distro:

| Distro | Commands |
| --- | --- |
| Arch / Manjaro | `sudo pacman -S --needed base-devel pkgconf alsa-lib` |
| Ubuntu / Debian / Mint | `sudo apt install build-essential pkg-config libasound2-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev` |
| Fedora | `sudo dnf install gcc make pkgconf alsa-lib-devel libxkbcommon-devel wayland-devel` |

Get the code and build:

```sh
git clone https://github.com/Pliexi/drumgizmo-rust.git
cd drumgizmo-rust
scripts/build.sh          # release build + plugin bundle staging
# OR: cargo build --release
```

Verify the workspace is green on your machine (no sound card needed —
nothing in the test suite touches audio hardware):

```sh
scripts/test.sh           # cargo test --workspace
```

---

## 3. Run the standalone app

```sh
./target/release/drumgizmo-rust [KIT_DIR] [options]
```

Useful flags:

| Flag | Meaning |
| --- | --- |
| `--kit <PATH>` | kit directory to load at startup (or pass it positionally) |
| `--midi-in <NAME>` | open the first MIDI-in port whose name contains NAME (`--midi-in default` takes the first port) |
| `--gain <0.0..8.0>` | master gain (default 1.0) |
| `--scale <FACTOR>` | UI scale override (high-DPI displays) |
| `--list-midi` | print available MIDI input ports and exit |
| `--version` / `--help` | what they say |

`scripts/run_standalone.sh` builds if needed and runs with
`DG_PROFILE=${DG_PROFILE:-release}`.

### 3.1 Audio output

The app plays through the **default output device** (ALSA; with
PipeWire/PulseAudio installed it appears as the default). If there is
no usable audio device at launch — e.g. a headless box — the engine
still starts at 44100 Hz as a silent fallback and prints a loud
warning to stderr, so tests, MIDI routing and GUI checks keep working.

### 3.2 MIDI input

```sh
./target/release/drumgizmo-rust --list-midi
# 0: Midi Through:Midi Through Port-0 14:0

./target/release/drumgizmo-rust --midi-in "Keystation"
```

MIDI channel numbers are ignored (upstream DrumGizmo behaviour): a
note-on routes through `midimap.xml`, note-off stops the voice, and a
polyphonic aftertouch (or `0xA0` status) event with pressure > 0
chokes the instrument. Velocity is the byte scale 1..=127.

### 3.3 Loading real DrumGizmo kits

Kits are **100% original-format compatible** (velocity layers,
round-robins, choke groups, `drumkit.xml` + `midimap.xml`):

1. Download a kit from <https://drumgizmo.org/wiki/doku.php?id=kits>
   (e.g. the Mugen drums, PowerDrumKit, MRDrummer...).
2. Unpack it somewhere (`~/kits/mugen/`).
3. Run: `./target/release/drumgizmo-rust ~/kits/mugen/`
   or `--kit ~/kits/mugen` — the folder with `drumkit.xml` inside.

### 3.4 GUI controls

Three tabs: **Kit** (loaded kit tree + sample inspection),
**Sequencer** (pattern editor), **Inspector** (voices/detail watch).

| Key | Action |
| --- | --- |
| `Space` | start/stop the sequencer transport |
| `Ctrl+Z` / `Ctrl+Y` | undo / redo pattern edits |
| `1`–`9` | select lane 1–9 |

---

## 4. Use it as a plugin in any DAW

`scripts/install_local.sh` puts everything where hosts look:

```sh
scripts/install_local.sh
# installed: ~/.lv2/drumgizmo-rust.lv2
# installed: ~/.vst3/drumgizmo-rust.vst3
# installed: ~/.clap/drumgizmo-rust.clap
# installed: ~/.local/bin/drumgizmo-rust
```

Then rescan plugins in your DAW. What to expect in every format:
- **Params**: `Master gain` (0..8, linear) and `Sequencer tempo`
  (30..360 BPM), both automatable.
- **MIDI in**: the same note/choke semantics as the standalone app.
- **Output**: stereo.
- **State** persists (kit path, gains, tempo, pattern) and survives
  project reload.
- **GUI**: no custom editor yet in VST3/CLAP — hosts show controls
  (automation lanes, gain/tempo sliders) but not a custom window.
  Known and documented in `docs/decisions.md`.

### 4.1 Per-format notes

- **LV2** — `carla-jack-single`, `ardour`, `reaper` and anything
  lilv-based will load `~/.lv2/drumgizmo-rust.lv2`. Quick test
  without any DAW (needs PipeWire or JACK running):

  ```sh
  pw-jack jalv https://github.com/Pliexi/drumgizmo-rust/lv2
  ```

  (`jalv` runs as a JACK client; the shim above works with plain
  PipeWire. `-i` runs it non-interactively.)

- **VST3** — `~/.vst3/drumgizmo-rust.vst3` is picked up by REAPER,
  Bitwig, Ardour, Waveform, Carla... Check the bundle layout kept:
  `Contents/<arch>-linux/drumgizmo-rust.so` + `moduleinfo.json` —
  Linux hosts look for the module **named like the bundle stem**.

- **CLAP** — `~/.clap/drumgizmo-rust.clap` loads into CLAP hosts
  (REAPER 7+, Bitwig, Reaper-style loaders, Carla 2.6+). The plugin
  passes the free-audio `clap-info` validator cleanly.

- **Carla** 2.5.x does not know the word "clap" yet — use 2.6+ for
  CLAP or just load the LV2/VST3 there.

---

## 5. Troubleshooting

| Symptom | Meaning / fix |
| --- | --- |
| GUI doesn't open | Wayland/X11 compositors need the client libs from §2.2; try `--scale 1.0` on 4K if it looks zoomed |
| `--list-midi` shows only `Midi Through` | That is fine — PipeWire exposes raw MIDI through it; connect your controller with `pw-dump`/`pw-midi`-ish tools, or in the setup your DAW owns the port |
| `Error` + 44100 silent fallback warning | No usable audio device (headless box). Tests, GUI and MIDI keep working; audio resumes on a machine with a device |
| DAW shows plugin but no sound | The engine needs a kit: set the kit path in plugin state (or let `--kit` seed it; VST3/CLAP state carries it) |
| Plugin not discovered | DAW 32-bit? We build x86_64 only. Path wrong? Re-run `scripts/install_local.sh` |

---

## 6. What "verified" means here

Everything promised in `README.md` is checked by tests that need no
audio hardware, plus third-party host validation recorded in
`docs/decisions.md`:

- `cargo test --workspace`: 116 tests, 0 failures (engine renders
  real sample arrays end-to-end against generated test kits).
- **clap-info** (free-audio reference validator, C): loads the CLAP
  binary, validates descriptor/params/note-ports/audio-ports, writes
  and reads state — exit 0, stderr empty.
- **carla-discovery 2.5.10**: LV2 and VST3 discovered with the
  expected bus/port/param layout.
- **pw-jack jalv**: real LV2 host instantiation, 15 s headless run.

If anything above breaks on your machine it is our bug — please open
an issue with your distro + logs.
