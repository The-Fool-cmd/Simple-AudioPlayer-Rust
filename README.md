# Simple-AudioPlayer-Rust

A simple terminal audio player with a Ratatui UI.

## What it does

- Browse a directory, pick a file, and play it.
- Highlights common audio formats (mp3, mp1, wav, aiff/aif, flac, ogg/vorbis, isomp4/m4a/mp4, aac, alac).
- Seek ±5s, pause/resume, and stop at end-of-track.
- Visualizer:
  - Wide terminals: vertical sliding bars.
  - Narrow terminals: single horizontal peak bar.
- Timeline shows progress (uses total duration when available).

## Build

```
cargo build
```

## Run

No arg (current directory):
```
cargo run
```

With a directory:
```
cargo run -- /path/to/music
```

Note: If an argument is provided, it must be a valid directory.

## Controls

- Up/Down: navigate files
- Enter: open dir / play file
- h: parent directory
- Space: pause/resume
- Left/Right: seek -/+ 5s
- q: quit

## Notes

- No resampling: output device must support the file’s sample rate at 2 channels.
- Duration may be missing for some formats; timeline falls back gracefully.
- Errors (e.g., unsupported file) are shown in the status header.

## Layout

- Status (top)
- Files (left) + Visualizer (right) or stacked on small terminals
- Timeline (separate block)
- Controls (bottom)
