# Video Compressor

A small Windows and Linux desktop app for shrinking videos and images down to a target file size 

## Features

- Compress videos and images to a target size: 8 MB, 25 MB, 50 MB, 100 MB, or a custom value
- Two-pass encoding for more accurate results
- Optional audio muting
- Trim video start/end before compressing
- Drag-and-drop, or batch-process a whole folder

## Supported file types

- **Video:** mp4, mkv, avi, mov, wmv, flv, webm, m4v, ts, mts
- **Image:** jpg, jpeg, png, bmp, gif

## Building

Requires Rust.

**Windows** (cross-compiling from macOS/Linux needs `mingw-w64` for `windres`):

```
cargo build --release --target x86_64-pc-windows-gnu
```

**Linux** (build on Linux; needs GTK3 and windowing headers, e.g. on Debian/Ubuntu
`sudo apt install build-essential pkg-config libgtk-3-dev libxkbcommon-dev libwayland-dev libx11-dev`):

```
cargo build --release --target x86_64-unknown-linux-gnu
```

## Notes

- Settings (target size, mute, output folder, etc.) persist across launches: on Windows in the registry under `HKCU\Software\VideoCompressor`, on Linux in `~/.config/video-compressor/settings.json`.
- Windows shell integration ("Open with") registers itself under `HKCU`
- FFmpeg failure logs: `%LOCALAPPDATA%\VideoCompressor\logs` on Windows, `~/.local/state/video-compressor/logs` on Linux.