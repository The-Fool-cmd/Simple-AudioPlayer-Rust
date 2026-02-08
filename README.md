# Simple-AudioPlayer-Rust

A minimal terminal audio player with a Ratatui UI. Built to be simple, fast, and easy to hack on.

## Features

- File explorer:
  - Starts in a directory (current directory by default).
  - Highlights supported audio files (mp3, mp1, wav, aiff/aif, flac, ogg, vorbis, isomp4/m4a/mp4, aac, alac).
  - Non-supported files remain visible but are not highlighted.
- Playback:
  - Uses Symphonia for format probing/decoding.
  - Real seek support (±5 seconds).
  - End-of-track stops playback (no autoplay).
- UI:
  - Status header showing current time, total duration when available, sample rate, channels, and underruns.
  - Permanent file explorer block (left).
  - Visualizer block:
    - Vertical sliding-window bars on wide terminals (side-by-side layout).
    - Horizontal single-bar peak when stacked below files on narrow terminals.
  - Timeline block maps progress to full duration (when available), or shows placeholder while idle.
  - Controls footer.
- Lower-latency ring buffer for snappier response.

## Limitations

- The output device must support the input sample rate and 2 channels. There is no resampling.
- Duration may be approximate or unavailable for some formats; timeline falls back gracefully.
- Visualizer uses output peak level, not a full spectrum.

## Build and Run

Prerequisites:
- Rust toolchain (stable) and Cargo.
- A working system audio backend supported by CPAL.

Build:
```
cargo build
```

Run (no argument: start in current directory):
```
cargo run
```

Run (explicit directory; must be valid or it errors):
```
cargo run -- /path/to/music
```

Notes:
- If your terminal shows relative paths like `../../..`, they’ll be canonicalized and displayed cleanly.
- If argument is present and not a directory, the program exits with an error.

## Controls

- Up / Down: Move selection in the file list
- Enter:
  - On a directory: open it
  - On a file: start playback (stops current and switches if already playing)
- h: Go to parent directory
- Space: Pause / Resume playback
- Left / Right: Seek -/+ 5 seconds (real seek)
- q: Quit

## Supported Formats

Enabled Symphonia features include:
- mp3, mp1
- wav, aiff/aif
- flac
- ogg, vorbis
- isomp4/m4a/mp4
- aac, alac

If you need more formats, add the corresponding Symphonia feature flags in `Cargo.toml`.

## Project Structure

- `src/main.rs`: CLI argument handling and UI entry point.
- `src/ui.rs`: Ratatui UI (file explorer, visualizer, timeline, controls).
- `src/player.rs`: Playback (probe, decode, output, seek, state).

## Troubleshooting

- “Argument must be a valid directory”:
  - Pass a directory or omit the argument to use the current directory.
- “No default output device found”:
  - Check your OS sound configuration; ensure a default output device exists.
- “No supported output config matches sample rate…”:
  - Your device may not support the file’s sample rate at 2 channels. This build does not resample.
- Silent playback or underruns:
  - Lower system load or try different files/devices; the buffer is tuned for lower latency.
