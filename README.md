# RSnip

RSnip is a native Windows screen capture tool for fast screenshots, region recording, OCR, lightweight annotation, and clipboard workflows.

It is a Rust reimplementation of the original SnipTool prototype. The final tool does not depend on Python or AutoHotkey.

## Features

- Global hotkeys.
- Single-instance daemon.
- Multi-monitor virtual desktop capture.
- Rectangle snip selection with dimmed overlay.
- Window hover selection and taskbar selection.
- Image clipboard support compatible with apps that prefer DIB or PNG clipboard formats.
- Floating editor for quick annotations.
- Region recording to MP4 with recording overlay and cursor rendering.
- OCR region selection using Tesseract.
- Non-modal toast notifications.

## Default hotkeys

| Hotkey | Action |
| --- | --- |
| `Ctrl+Shift+S` | Snip region/window and copy image to clipboard |
| `Ctrl+Shift+R` | Start region recording, or stop active recording |
| `Ctrl+Shift+E` | OCR region/window and copy text to clipboard |

Hotkeys are configurable in `rsnip.toml`.

## Commands

```powershell
rsnip.exe daemon   # Start daemon and global hotkeys
rsnip.exe snip     # Trigger snip through the daemon
rsnip.exe record   # Start/stop recording through the daemon
rsnip.exe ocr      # Trigger OCR through the daemon
rsnip.exe stop     # Stop the daemon
rsnip.exe config   # Print the config file path
```

Normal usage is to start the daemon once:

```powershell
rsnip.exe daemon
```

Then use the global hotkeys.

## Snipping

Press `Ctrl+Shift+S`, then:

- drag to select a rectangle;
- click a highlighted window to capture that window;
- press `Escape` to cancel.

After selection, the image is copied to the clipboard.

If you hold `Shift` while selecting, RSnip opens the editor instead of copying directly.

A normal snip also shows a toast. Clicking that toast opens the captured image in the editor.

## Editor

The editor is a small floating frameless window.

Supported tools:

| Shortcut | Tool |
| --- | --- |
| `1` | Pen/freehand |
| `2` | Arrow |
| `3` | Line |
| `4` | Rectangle |
| `5` | Redact/black box |
| `6` | Numbered step marker |

Color shortcuts:

| Shortcut | Color |
| --- | --- |
| `Q` | Red |
| `W` | Blue |
| `E` | Green |
| `R` | Yellow |

Editor shortcuts:

| Shortcut | Action |
| --- | --- |
| `Ctrl+C` | Copy edited image and close editor |
| `Ctrl+Z` | Undo last annotation |
| `Escape` | Close editor |

The top-bar copy button copies without closing the editor.

## Recording

Press `Ctrl+Shift+R`, select a region, and recording starts.

Press `Ctrl+Shift+R` again to stop.

Recording behavior:

- records only the selected region;
- saves MP4 files to the configured save folder;
- filename format: `Recording_YYYYMMDD_HHMMSS.mp4`;
- copies the MP4 file to the clipboard as a file drop;
- shows a toast that can open Explorer at the saved file;
- shows a red recording overlay with elapsed time and FPS;
- renders the cursor into the video;
- uses wall-clock based timing to avoid video duration drift.

## OCR

Press `Ctrl+Shift+E`, select a region or highlighted window, and RSnip runs Tesseract OCR.

If text is found, it is copied to the clipboard as Unicode text.

OCR supports Spanish and English by default:

```toml
[ocr]
languages = "spa+eng"
```

## Configuration

RSnip creates a config file on first daemon run.

Print the config path:

```powershell
rsnip.exe config
```

Default location on Windows:

```text
%APPDATA%\rsnip\rsnip.toml
```

Example config:

```toml
[hotkeys]
snip = "ctrl+shift+s"
record = "ctrl+shift+r"
ocr = "ctrl+shift+e"

[recording]
fps = 30
save_folder = "C:\\Users\\<user>\\Videos"
codec = "libx264"
crf = 26
preset = "veryfast"
# ffmpeg_path = "C:\\Tools\\ffmpeg.exe"

[ocr]
tesseract_path = "C:\\Program Files\\Tesseract-OCR\\tesseract.exe"
languages = "spa+eng"

[ui]
toasts = true
editor = true
```

## Runtime dependencies

### FFmpeg

Recording requires `ffmpeg.exe`.

RSnip uses `ffmpeg.exe` from `PATH` by default. You can also configure an explicit path:

```toml
[recording]
ffmpeg_path = "C:\\Tools\\ffmpeg.exe"
```

### Tesseract OCR

OCR requires Tesseract.

Default expected path:

```text
C:\Program Files\Tesseract-OCR\tesseract.exe
```

Recommended Windows installer:

```text
https://github.com/UB-Mannheim/tesseract/releases/
```

If Tesseract is installed elsewhere, update:

```toml
[ocr]
tesseract_path = "C:\\Path\\To\\tesseract.exe"
```

## Packaging

Local packaging is handled by:

```powershell
.\release-local.ps1 -Version 0.1.0 -PackageOnly
```

The script builds a Windows x64 package under `dist/`:

```text
dist/rsnip-v<version>-windows-x64.zip
```

Package contents include:

- `rsnip.exe`
- `README.txt`
- `rsnip.example.toml`
- selected docs
- checksums

For a dry run:

```powershell
.\release-local.ps1 -Version 0.1.0 -PackageOnly -DryRun
```

## Development

Useful commands:

```powershell
cargo fmt --check
cargo check
cargo test
cargo build --release
```

Run package-only release without validation when a release binary already exists:

```powershell
.\release-local.ps1 -Version 0.1.0 -PackageOnly -SkipValidation
```

## Notes

- RSnip targets Windows.
- No Python runtime is required.
- No AutoHotkey runtime is required.
- Audio recording is not part of the current MVP. System audio recording via WASAPI loopback is tracked as a future nice-to-have.
