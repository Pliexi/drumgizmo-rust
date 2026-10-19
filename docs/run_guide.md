# Run guide — drumgizmo-rust on any machine

This is the practical guide: get the app running on your machine, load
a DrumGizmo kit, play it from a keyboard, hear it through the speakers
or drop it into a DAW as a plugin.

Supported today: **Linux x86_64** (tested on Arch Linux with PipeWire;
CI builds on Ubuntu 22.04) and **Windows x86_64** (CI builds with the
MSVC toolchain; the standalone and all three plugin formats are
compiled, but no Windows DAW session could be run in this project
environment — see docs/decisions.md). macOS is not built yet; the Rust
code itself is portable, but the plugin bundles and build scripts here
do not handle it yet.

---

## 1. Fastest path: prebuilt binaries (GitHub Releases)

Go to <https://github.com/Pliexi/drumgizmo-rust/releases>: every tag
publishes, without any local toolchain:

| Asset | What you get |
| --- | --- |
| `drumgizmo-rust-v0.1.1-linux-x86_64.zip` | standalone + LV2/VST3/CLAP bundles + README + RUN_GUIDE |
| `drumgizmo-rust-v0.1.1-windows-x86_64.zip` | same for Windows (MSVC build) |
| `drumgizmo-rust_0.1.1_amd64.deb` | Debian/Ubuntu/Mint system package (CLI into PATH, plugins into host search paths) |
| `drumgizmo-rust-0.1.1-x86_64.rpm` | Fedora/RHEL/openSUSE package |
| `drumgizmo-rust-0.1.1-x86_64.pkg.tar.zst` | Arch Linux pacman package (`sudo pacman -U …`) |

Linux zip quick start:

```sh
unzip drumgizmo-rust-v0.1.1-linux-x86_64.zip
cd release   # the archive holds one folder with everything
./drumgizmo-rust --help
```

Windows: unzip, start `drumgizmo-rust.exe`, and copy
`drumgizmo-rust.lv2/`, `drumgizmo-rust.vst3/` and
`drumgizmo-rust.clap` into your DAW%27s plugin folders (or
`%COMMONPROGRAMFILES%/VST3`, `%COMMONPROGRAMFILES%/CLAP`...). The
Windows plugin bundles carry `.dll` modules with the same stem-naming
rule hosts expect.

The packages for Debian/Fedora/Arch already install everything to the
standard paths — install and just look up `drumgizmo-rust` in your DAW
after a plugin rescan.

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
| `--list-audio` | print every detected audio host/device and the rated config the engine would pick, then exit (v0.1.3 — the "no sound?" self-diagnosis) |
| `--version` / `--help` | what they say |

`scripts/run_standalone.sh` builds if needed and runs with
`DG_PROFILE=${DG_PROFILE:-release}`.

### 3.1 Loading a kit is easy now (v0.1.1)

The Kit tab accepts any of these forms in the path field:
- a kit **directory** (…/mugen/),
- a **drumkit.xml file path**,
- with `~/` expansion, trimmed quotes and stray slashes removed.

Loading options, pick whichever feels natural:
1. type/paste the path and press **Load Kit**;
2. press **Paste** — reads the clipboard directly (arboard, works
   through Wayland portals even where Ctrl+V fails in apps);
3. expand **Browse directories** and click your way to the kit;
4. **drag a kit.xml / kit folder from your file manager** onto the
   window — instant load on drop.

The global shortcuts (Space, 1–9, Ctrl+Z/Y) stay out of the way while
you type in any text field.

### 3.2 Audio output

The app plays through the **default output device** (ALSA; with
PipeWire/PulseAudio installed it appears as the default). If there is
no usable audio device at launch — e.g. a headless box — the engine
still starts at 44100 Hz as a silent fallback and prints a loud
warning to stderr, so tests, MIDI routing and GUI checks keep working.

### 3.3 MIDI input

```sh
./target/release/drumgizmo-rust --list-midi
# 0: Midi Through:Midi Through Port-0 14:0

./target/release/drumgizmo-rust --midi-in "Keystation"
```

MIDI channel numbers are ignored (upstream DrumGizmo behaviour): a
note-on routes through `midimap.xml`, note-off stops the voice, and a
polyphonic aftertouch (or `0xA0` status) event with pressure > 0
chokes the instrument. Velocity is the byte scale 1..=127.

### 3.4 Loading real DrumGizmo kits

Kits are **100% original-format compatible** (velocity layers,
round-robins, choke groups, `drumkit.xml` + `midimap.xml`):

1. Download a kit from <https://drumgizmo.org/wiki/doku.php?id=kits>
   (e.g. the Mugen drums, PowerDrumKit, MRDrummer...).
2. Unpack it somewhere (`~/kits/mugen/`).
3. Run: `./target/release/drumgizmo-rust ~/kits/mugen/`
   or `--kit ~/kits/mugen` — the folder with `drumkit.xml` inside.

### 3.5 Sequencer (Channel Rack)

The **Sequencer** tab is a FL-Studio-style drum rack: a fixed instrument
column on the left, a bar-numbered ruler locked to the step grid and a
large beat-grouped cell grid (groups of four steps, brighter bar starts).

| Input | Action |
| --- | --- |
| **▶ / ⏹** button or `Space` | start / stop the transport |
| left click / left-drag | add / paint steps |
| right click / right-drag | erase steps (FL semantics) |
| middle click | toggle the *choke* articulation on a step |
| mouse wheel on a step | velocity trim (Inspector edits longer tails) |
| instrument name click | one preview hit at velocity 114 |
| `1`–`9` | select lane |
| `Ctrl+Z` / `Ctrl+Y` | undo / redo |

While the transport runs, the **playhead column glows amber** in the
grid and lies exactly on the step the engine is sounding (the GUI polls
the engine's realtime watch — no drift, no guessing).

A freshly loaded kit **seeds a demo beat** (kick 0/10 · snare 4/12 ·
closed hats offbeat across all bars): press ▶ and hear it immediately.
Every lane name shows the instrument and its MIDI note; empty kits with
unusual names fall back to the first three lanes.

The **status bar** is honest about sound: 🎧 shows the live output
device (`48000 Hz / 2 ch`) or the exact silence reason (`no device: …`,
`stream failed: …`), and the kit state comes straight from the engine:
`🥁 kit vN` or `⚠ no kit loaded` with the load error flagged on failure.

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
- **MIDI in**: the same note/choke semantics as the standalone app
  (velocity-zero note-ons count as note-offs; out-of-range keys are
  dropped).
- **Output**: stereo (mono hosts get a mixdown; extra channels silent).
- **State** persists (kit path, pattern, master gain, tempo) and
  survives project reload.
- **GUI**: CLAP and VST3 embed the **full egui editor** — the Kit tab
  with a native file chooser button (`📂 Open file chooser…`), the
  Sequencer and the Inspector, exactly like the standalone window.
  Master gain and tempo are edited in that GUI; there are no
  host-automatable parameters (see `docs/decisions.md` for why).
- **LV2** keeps the two automatable parameters (`Master gain`,
  `Sequencer tempo`) and no GUI yet — the LV2 wrapper was left as-is
  in v0.1.3 and the editor work went to CLAP/VST3 first.

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

### 4.2 Upgrading from v0.1.2 sessions

The VST3 class IDs changed between v0.1.2 (hand-written wrapper) and
v0.1.3 (nice-plug wrapper): v0.1.2 exported **two** VST3 classes
(processor + edit controller), v0.1.3 exports the modern **single**
combined class. Hosts cannot map the old class to the new one, so
projects saved with the v0.1.2 VST3 will show it as missing after
upgrading; re-add "DrumGizmo Rust" once and the state (kit/pattern/
gain) is re-settable from the (now built-in) UI. CLAP projects keep
the same `com.pliexi.drumgizmo-rust` id and restore normally; LV2 is
unchanged.

---

## 5. Troubleshooting

| Symptom | Meaning / fix |
| --- | --- |
| GUI doesn't open | Wayland/X11 compositors need the client libs from §2.2; try `--scale 1.0` on 4K if it looks zoomed |
| Ctrl+V into the kit path does nothing | Your compositor/clipboard bridge is silent — use the **Paste** button (reads the clipboard directly, works under Wayland) or drag&drop the kit.xml in |
| `--list-midi` shows only `Midi Through` | That is fine — PipeWire exposes raw MIDI through it; connect your controller with `pw-dump`/`pw-midi`-ish tools, or in the setup your DAW owns the port |
| `Error` + 44100 silent fallback warning | No usable audio device (headless box). Tests, GUI and MIDI keep working; audio resumes on a machine with a device |
| DAW shows plugin but no sound | The engine needs a kit: open the plugin UI (CLAP/VST3) and use `📂 Open file chooser…`; state carries the kit across reloads |
| Standalone seems silent | Run `drumgizmo-rust --list-audio` — it prints every detected host/device and the config it chose; if the device appears with a non-float sample format the new tiered `choose()` should still have matched it, otherwise file an issue with that output |
| Plugin not discovered | DAW 32-bit? We build x86_64 only. Path wrong? Re-run `scripts/install_local.sh`. Windows hosts: check the module file is named `drumgizmo-rust.vst3` inside `Contents\x86_64-win\` |
| Typing digits/Space in a field does something else | Fixed in v0.1.1 — fields now own the keyboard; update your build |

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
