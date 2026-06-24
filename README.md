# rsll 🎹

**rsll** is a live MIDI looper with a terminal UI, written in Rust. It connects to your MIDI controller over JACK, records what you play into looping tracks, and plays everything back through a software synth such as FluidSynth or sfizz. Everything is driven from the controller itself — arm a track, punch in, layer loops, switch scenes — so you can keep your hands on the keys while the TUI gives you live feedback on tracks, recording state, and the master clock.

## Features ✨

- 🔁 **Multi-track looping** — record and overdub up to 8 independent tracks, each with its own MIDI channel, program (instrument), volume, and loop length
- 🥁 **Drum pads** — pad bindings mapped to drum notes on channel 10, with per-scene kits and velocity scaling
- 🎬 **Scenes** — multiple song setups in `scenes/*.toml` (BPM, time signature, track programs, drum kits); switch between them live from the controller
- ⏱️ **Master clock & metronome** — configurable BPM, PPQN, and time signature, with a toggleable metronome (downbeat + subdivision clicks)
- 🎯 **Quantize** — optional snapping of recorded notes to the nearest sixteenth
- ↩️ **Undo / clear** — per-track undo history, clear-track, and clear-all controls
- 🔇 **Track mute** and per-track volume via a mapped knob
- 💡 **LED feedback** — pad LEDs reflect track state (on / off / blinking while armed)
- ⏲️ **Arm time** — configurable minimum delay between arming record and punch-in
- 🔄 **Live config reload** — `hardware.toml` and scene files are watched and reloaded while running, no restart needed
- 💾 **MIDI export** — press `s` in the TUI to export your loops to a standard MIDI file

## Requirements

- A Linux system with [JACK](https://jackaudio.org/) (rsll uses `midir` with the JACK backend)
- A MIDI controller (the default `hardware.toml` is set up for an Akai MPK mini IV — adjust the port patterns and key bindings for your hardware)
- A synth to make sound: [FluidSynth](https://www.fluidsynth.org/) or [sfizz](https://sfz.tools/sfizz/) must be running so rsll has an output port to connect to

## Building 🔨

```sh
cargo build --release
```

The binary lands at `target/release/rsll`.

## Running 🚀

1. Start your synth first, e.g. FluidSynth with a General MIDI soundfont:

   ```sh
   fluidsynth -a jack -m jack /path/to/GeneralUser.sf2
   ```

   (or launch sfizz instead)

2. Plug in your MIDI controller.

3. Launch rsll from the project directory:

   ```sh
   ./target/release/rsll
   ```

rsll will scan for MIDI ports matching the regex patterns in `hardware.toml`, connect inputs/outputs automatically (and keep retrying if a device appears later), load every scene from `scenes/`, and start the TUI.

### Command-line options

| Flag | Description |
|------|-------------|
| `-s <scene.toml>` | Load a single scene file instead of the `scenes/` directory |
| `-c <hardware.toml>` | Use an alternate hardware config file |
| `-h`, `--help` | Show usage |

### TUI keys

| Key | Action |
|-----|--------|
| `q` | Quit |
| `s` | Export loops to a MIDI file |

## Configuration

- **`hardware.toml`** — MIDI port regex patterns, controller key bindings (record, clear, undo, mute, quantize, track pads, drum pads, scene up/down, BPM/volume knobs), LED channels, and record arm time. Live-reloaded while running.
- **`scenes/*.toml`** — one file per scene: BPM, metronome default, time signature, PPQN, per-track channel/length/program, and the drum kit (notes, channel, velocity multiplier). Loop `length` accepts either a raw tick count (`length = 96`) or a musical fraction string relative to a whole note (`length = "1/8"`, `length = "1/12"`); at PPQN 24 a whole note is 96 ticks, so `"1/1"` = 96, `"1/4"` = 24, `"1/8"` = 12.
- **`gm.toml`** — General MIDI program number → instrument name table used for display.
