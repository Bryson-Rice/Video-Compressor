# Video Compressor

A small Windows desktop app for shrinking videos and images down to a target file size 

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

Requires Rust and, for Windows builds made on macOS/Linux, `mingw-w64` (for `windres`):

```
cargo build --release --target x86_64-pc-windows-gnu
```

`ffmpeg.exe` and `ffprobe.exe` are bundled in `bin/` and used at runtime.

## Notes

- Settings (target size, mute, output folder, etc.) are saved to the registry under `HKCU\Software\VideoCompressor` and persist across launches.
- Windows shell integration ("Open with") also registers itself under `HKCU`, so no admin rights are needed.
