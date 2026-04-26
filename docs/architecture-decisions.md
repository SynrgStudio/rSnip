# RSnip Architecture Decisions

## ADR-001: Keep selection overlay separate from editor UI

Status: accepted  
Date: 2026-04-25

### Context

The selection overlay is the highest-priority UX/performance surface. It must appear quickly after hotkey activation and track mouse movement without perceptible lag while drawing selection rectangles or highlighting hovered windows.

The editor has more complex controls and tool state, but it is not in the same critical latency path.

### Decision

Implement the selection overlay and recording overlay as dedicated lightweight windows/rendering paths, separate from the editor.

Initial direction:

- Selection overlay: `winit` + `softbuffer` or direct WinAPI/GDI where needed.
- Recording overlay: lightweight custom rendering/windowing, optimized for low overhead.
- Editor: `egui` is allowed if it reduces complexity and improves maintainability.

### Consequences

- The overlay can be optimized independently from the editor.
- The editor can use higher-level UI abstractions without compromising snip/record/OCR selection latency.
- Shared data contracts are required for passing captured images/regions into the editor.

## ADR-002: Start with GDI screenshot capture, keep DXGI/WGC as future optimization

Status: accepted  
Date: 2026-04-25

### Context

The MVP needs fast, reliable snips before advanced recording optimization. GDI `BitBlt` is simpler to implement and sufficient for screenshots and initial region recording validation.

### Decision

Use GDI `BitBlt` for initial full virtual desktop screenshot capture and crop operations.

Keep DXGI Desktop Duplication or Windows Graphics Capture as future recording optimization if GDI cannot meet recording performance requirements.

### Consequences

- MVP can reach usable snip behavior sooner.
- Recording may need later backend replacement for high FPS or large regions.
- Capture abstractions should not expose GDI-specific details to the rest of the app.

## ADR-003: Use native Windows primitives for daemon-critical behavior

Status: accepted  
Date: 2026-04-25

### Context

The final tool must not depend on Python or AHK. Hotkeys, single instance, IPC, clipboard, window enumeration and capture are Windows-native concerns.

### Decision

Use the `windows` crate for:

- `RegisterHotKey` / `WM_HOTKEY`.
- Named mutex for single instance.
- Named pipe IPC.
- GDI/DWM capture/window bounds.
- Clipboard formats: `CF_DIB`, `CF_UNICODETEXT`, `CF_HDROP`.

### Consequences

- More explicit unsafe/WinAPI handling is required.
- Behavior can match Windows expectations with lower overhead.
- Wrappers must own handle cleanup and error reporting.

## ADR-004: Implement the editor as an isolated custom-rendered child process

Status: accepted  
Date: 2026-04-25

### Context

The editor must provide a frameless floating window, simple toolbar, image canvas, annotation tools, undo, shortcuts and clipboard copy. The selection overlay already uses `winit + softbuffer` successfully, but `winit` event loops cannot be recreated reliably inside the long-lived daemon process. The overlay solved this by launching a short-lived child process per selection.

`egui` remains acceptable for non-critical UI, but adding it now would increase dependency surface and still require isolating the event loop from the daemon. The first editor feature set is mostly custom drawing over a bitmap, which matches the existing `image`/`imageproc`/`softbuffer` stack.

### Decision

Implement the editor with the existing lightweight stack:

- Window/event loop: `winit` in a dedicated child process.
- Presentation: `softbuffer`.
- Annotation model and final composition: pure Rust state plus `image`/`imageproc` where useful.
- Entry point: hidden/internal command such as `__editor-once <input-image-path>`.
- Input image format: temporary PNG stored under the RSnip temp directory.
- Output path: editor copies the composed image to the Windows clipboard as `CF_DIB`; it may also emit a stable machine-readable outcome line for daemon/toast integration.

The daemon must not host the editor event loop directly. Snip with Shift and toast click should save the crop to a temp PNG and launch the editor child process with that path.

### Consequences

- No new UI dependency is required for the initial editor implementation.
- The daemon remains stable and avoids `winit` event-loop reuse issues.
- Temp-file lifecycle becomes part of the editor contract.
- The model/render code must stay separated enough that `egui` can still be introduced later if custom toolbar/canvas work becomes disproportionately costly.
