# RSnip Behavior Reference

This document captures the intended user-visible behavior for the Rust implementation. The Python implementation in `sniptool/` is a functional reference only; do not port it line by line.

## Product goal

RSnip is a native Windows tool for fast screen snipping, region recording, OCR, and lightweight image annotation.

The UX must feel immediate. The selection overlay for snip, recording, and OCR is a critical latency path and must not show perceptible lag while tracking the mouse or drawing the selection rectangle.

## Commands

Expected executable: `rsnip.exe`.

Commands:

```text
rsnip daemon      # start daemon and register hotkeys
rsnip snip        # trigger snip via daemon/IPC when available
rsnip record      # start/stop recording via daemon/IPC when available
rsnip ocr         # trigger OCR selection via daemon/IPC when available
rsnip stop        # stop daemon
rsnip config      # print config path
```

## Default hotkeys

```text
Ctrl+Shift+S -> snip
Ctrl+Shift+R -> recording start/stop
Ctrl+Shift+E -> OCR region
```

Hotkeys must be configurable from `rsnip.toml`.

## Snip flow

1. User presses `Ctrl+Shift+S` or runs `rsnip snip`.
2. RSnip captures the full virtual desktop across all monitors.
3. RSnip opens a topmost borderless overlay covering the full virtual desktop.
4. The overlay shows the current desktop dimmed.
5. User drags a rectangle.
6. `Escape` cancels.
7. Mouse release crops the selected region.
8. Default behavior: copy cropped image to clipboard as Windows-compatible DIB.
9. Show toast: `✔  Copiado al portapapeles (Click para editar)`.
10. Clicking the toast opens the editor with the captured image.
11. If `Shift` is held during selection, open the editor directly instead of copying immediately.

Invalid or tiny selections should be ignored or cancelled without crashing.

## Selection by window

The selection overlay should also support window selection:

- Hover visible windows and highlight the window under the cursor.
- Click without dragging selects the highlighted window.
- Include the Windows taskbar as selectable.
- Use DWM extended frame bounds when available.
- Fall back to regular window rects when DWM bounds are unavailable.
- Do not select minimized windows.
- Attempt to bring the target window to foreground before capture.

Hover and highlight must remain responsive.

## Clipboard behavior

Image clipboard:

- Copy as `CF_DIB` / bitmap-compatible data.
- Must paste into common Windows apps: Paint, Telegram, Discord, browsers, Word/OneNote.
- Retry briefly if clipboard is temporarily busy.
- Always close the clipboard after use, including error paths.

Text clipboard:

- Copy OCR output as Unicode text.

File clipboard:

- Copy completed recordings as `CF_HDROP`.

## Editor behavior

The editor may use `egui` if it makes implementation faster and more maintainable. The editor is not the same latency-critical path as the selection overlay.

Expected behavior:

- Floating frameless window.
- Draggable top bar.
- Selected image displayed on canvas.
- Copy button.
- Close button.
- `Escape` closes.
- `Ctrl+C` copies edited image.
- `Ctrl+Z` undo.

Tools:

```text
1 -> pen/freehand
2 -> arrow
3 -> line
4 -> rectangle
5 -> redact/black censor block
6 -> numbered steps
```

Colors:

```text
q -> red    #ff3333
w -> blue   #3388ff
e -> green  #33cc33
r -> yellow #ffcc00
```

Additional behavior:

- Holding `Shift` while drawing line/arrow constrains to 45-degree angles.
- Undo must restore both image state and numbered-step counter.
- Copying from the editor copies the rendered edited image to clipboard.
- Successful editor copy toast: `✔  Editado y copiado al portapapeles`.

## Recording flow

1. User presses `Ctrl+Shift+R` or runs `rsnip record`.
2. If no recording is active, RSnip opens the same fast selection overlay in recording mode.
3. User selects a region.
4. RSnip starts recording only that region.
5. Pressing `Ctrl+Shift+R` again stops the active recording.
6. Save file in configured recording folder.
7. Filename format: `Recording_YYYYMMDD_HHMMSS.mp4`.
8. Copy saved MP4 to clipboard as `CF_HDROP`.
9. Show video toast.

Recording requirements:

- FPS configurable.
- Include cursor if cursor is inside the recording region.
- Maintain real duration aligned with wall-clock time.
- Duplicate last frame if capture loop falls behind.
- Initial encoder path may use external `ffmpeg.exe` receiving raw video on stdin.

Recording overlay:

- Red dotted border around selected region.
- Red/dark top bar.
- Text: `00:00 | N FPS`.
- Overlay should not add severe overhead or appear in captured frames if avoidable.

Video toast:

- Message equivalent: `✔ Video Guardado y Copiado (Click para abrir)` when copied.
- Click opens Explorer selecting the saved file and/or exposes path behavior equivalent to Python reference.

## OCR flow

1. User presses `Ctrl+Shift+E` or runs `rsnip ocr`.
2. RSnip opens the same fast selection overlay in OCR mode.
3. User selects a region.
4. Show progress toast: `⚙ Extrayendo texto...`.
5. Run Tesseract on the selected image.
6. Default languages: `spa+eng`.
7. Copy extracted text to clipboard as Unicode text.
8. If text exists, show `✔ Texto extraído y copiado!`.
9. If no clear text is found, show `⚠ No se encontró texto claro`.
10. If Tesseract is missing or fails, show actionable error: `✘ Error: ¿Tienes instalado Tesseract-OCR?`.

## Toast behavior

- Bottom-right position.
- Topmost temporarily.
- Fade in/out.
- Auto-destroy.
- Click handlers for snip/editor and video actions.
- Must not block the main UI loop or steal focus unnecessarily.

## Configuration behavior

Recommended config: `rsnip.toml`.

Minimum expected fields:

```toml
[hotkeys]
snip = "ctrl+shift+s"
record = "ctrl+shift+r"
ocr = "ctrl+shift+e"

[recording]
fps = 30
save_folder = "C:/Users/<user>/Videos"
codec = "libx264"
crf = 26
preset = "veryfast"

[ocr]
tesseract_path = "C:/Program Files/Tesseract-OCR/tesseract.exe"
languages = "spa+eng"

[ui]
toasts = true
editor = true
```

The Python `sniptool/config.json` may be imported or used as reference, but Rust should own its final config format.

## Performance requirements

Critical latency paths:

- Hotkey to overlay visible.
- Overlay mouse tracking.
- Selection rectangle redraw.
- Window hover detection/highlight.
- Recording overlay updates.

These must prioritize performance over convenience. Use a simple renderer and minimize allocations/repaints in these paths.

## Non-goals

- Do not preserve Python/AHK dependencies.
- Do not copy the Python implementation line by line.
- Do not add tray/autostart as MVP blockers.
- Do not optimize recording with DXGI/WGC before the MVP proves the full flow unless GDI is clearly insufficient.
