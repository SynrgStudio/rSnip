use std::num::NonZeroU32;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use softbuffer::{Context as SoftbufferContext, Surface as SoftbufferSurface};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize, Position, Size};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, OwnedDisplayHandle};
use winit::keyboard::{Key, NamedKey};
#[cfg(target_os = "windows")]
use winit::platform::windows::{
    EventLoopBuilderExtWindows, WindowAttributesExtWindows, WindowExtWindows,
};
#[cfg(target_os = "windows")]
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowAttributes, WindowId, WindowLevel};

use crate::errors::{Result, RsnipError};
use crate::screen::capture::CaptureRegion;
use crate::screen::monitor::VirtualScreen;
use crate::screen::windows::{SelectableWindow, SelectableWindowHandle};

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{HWND, POINT, RECT};
#[cfg(target_os = "windows")]
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetThreadDpiAwarenessContext,
};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetCursorPos, GetWindowRect, HWND_TOPMOST, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SetWindowPos,
};

pub const DEFAULT_SELECTION_THRESHOLD_PX: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Snip,
    Record,
    Ocr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRequest {
    pub mode: SelectionMode,
    pub threshold_px: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionFlags {
    pub shift_pressed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub mode: SelectionMode,
    pub region: CaptureRegion,
    pub flags: SelectionFlags,
    pub window_handle: Option<SelectableWindowHandle>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionOutcome {
    Selected(Selection),
    Cancelled(SelectionCancelReason),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionCancelReason {
    Escape,
    WindowClosed,
    BelowThreshold,
}

struct SelectionOverlayApp {
    request: SelectionRequest,
    virtual_screen: VirtualScreen,
    frame: OverlayFrame,
    context: SoftbufferContext<OwnedDisplayHandle>,
    surface: Option<SoftbufferSurface<OwnedDisplayHandle, Rc<Window>>>,
    window: Option<Rc<Window>>,
    outcome: SelectionOutcome,
    last_cursor_position: Option<PhysicalPosition<f64>>,
    cursor_move_events: u32,
    drag_state: MouseDragState,
    shift_pressed: bool,
    selectable_windows: Vec<SelectableWindow>,
    hovered_window: Option<SelectableWindow>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MouseDragState {
    start: Option<SelectionPoint>,
    current: Option<SelectionPoint>,
}

impl MouseDragState {
    fn begin(&mut self, point: SelectionPoint) {
        self.start = Some(point);
        self.current = Some(point);
    }

    fn update(&mut self, point: SelectionPoint) {
        if self.start.is_some() {
            self.current = Some(point);
        }
    }

    fn finish(&mut self, point: SelectionPoint) -> Option<(SelectionPoint, SelectionPoint)> {
        let start = self.start.take()?;
        self.current = None;
        Some((start, point))
    }

    fn cancel(&mut self) {
        self.start = None;
        self.current = None;
    }

    fn active_selection(
        &self,
        request: SelectionRequest,
        flags: SelectionFlags,
    ) -> Result<Option<Selection>> {
        let Some(start) = self.start else {
            return Ok(None);
        };
        let Some(current) = self.current else {
            return Ok(None);
        };
        Selection::from_drag(request.mode, start, current, flags, request.threshold_px)
    }
}

struct OverlayFrame {
    width: u32,
    height: u32,
    dimmed_pixels: Vec<u32>,
    clear_pixels: Vec<u32>,
    capture_elapsed: Duration,
    prepare_elapsed: Duration,
}

impl SelectionRequest {
    pub fn new(mode: SelectionMode) -> Self {
        Self {
            mode,
            threshold_px: DEFAULT_SELECTION_THRESHOLD_PX,
        }
    }

    pub fn with_threshold(mode: SelectionMode, threshold_px: u32) -> Self {
        Self { mode, threshold_px }
    }
}

impl SelectionPoint {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

impl SelectionFlags {
    pub fn new(shift_pressed: bool) -> Self {
        Self { shift_pressed }
    }
}

impl Selection {
    pub fn from_drag(
        mode: SelectionMode,
        start: SelectionPoint,
        end: SelectionPoint,
        flags: SelectionFlags,
        threshold_px: u32,
    ) -> Result<Option<Self>> {
        let Some(region) = normalize_drag_region(start, end, threshold_px)? else {
            return Ok(None);
        };

        Ok(Some(Self {
            mode,
            region,
            flags,
            window_handle: None,
        }))
    }
}

impl SelectionOverlayApp {
    fn new(
        request: SelectionRequest,
        virtual_screen: VirtualScreen,
        frame: OverlayFrame,
        context: SoftbufferContext<OwnedDisplayHandle>,
    ) -> Self {
        Self {
            request,
            virtual_screen,
            frame,
            context,
            surface: None,
            window: None,
            outcome: SelectionOutcome::Cancelled(SelectionCancelReason::WindowClosed),
            last_cursor_position: None,
            cursor_move_events: 0,
            drag_state: MouseDragState::default(),
            shift_pressed: false,
            selectable_windows: Vec::new(),
            hovered_window: None,
        }
    }

    fn current_cursor_selection_point(&self) -> Option<SelectionPoint> {
        current_platform_cursor_selection_point(self.virtual_screen).or_else(|| {
            self.last_cursor_position
                .map(|position| selection_point_from_window_position(position, self.virtual_screen))
        })
    }

    fn finish_drag(&mut self, event_loop: &ActiveEventLoop, point: SelectionPoint) {
        let Some((start, end)) = self.drag_state.finish(point) else {
            return;
        };
        let outcome = outcome_from_drag(
            self.request,
            start,
            end,
            SelectionFlags::new(self.shift_pressed),
        );
        let outcome = match outcome {
            SelectionOutcome::Cancelled(SelectionCancelReason::BelowThreshold) => self
                .hovered_window
                .as_ref()
                .map(|window| {
                    SelectionOutcome::Selected(Selection {
                        mode: self.request.mode,
                        region: window.bounds,
                        flags: SelectionFlags::new(self.shift_pressed),
                        window_handle: Some(window.handle),
                    })
                })
                .unwrap_or(SelectionOutcome::Cancelled(
                    SelectionCancelReason::BelowThreshold,
                )),
            other => other,
        };
        println!("overlay-debug selection outcome: {outcome:?}");
        self.finish_with_outcome(event_loop, outcome);
    }

    fn finish_with_outcome(&mut self, event_loop: &ActiveEventLoop, outcome: SelectionOutcome) {
        self.drag_state.cancel();
        self.outcome = outcome;
        if let Some(window) = self.window.as_ref() {
            window.set_visible(false);
        }
        self.surface = None;
        event_loop.exit();
    }

    fn render_frame(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let active_selection = match self
            .drag_state
            .active_selection(self.request, SelectionFlags::new(self.shift_pressed))
        {
            Ok(selection) => selection,
            Err(error) => {
                self.outcome = SelectionOutcome::Failed(error.to_string());
                return;
            }
        };
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let size = window.inner_size();
        let width = size.width.min(self.frame.width).max(1);
        let height = size.height.min(self.frame.height).max(1);
        let Some(non_zero_width) = NonZeroU32::new(width) else {
            return;
        };
        let Some(non_zero_height) = NonZeroU32::new(height) else {
            return;
        };

        let started = Instant::now();
        if let Err(error) = surface.resize(non_zero_width, non_zero_height) {
            self.outcome =
                SelectionOutcome::Failed(format!("failed to resize overlay surface: {error}"));
            return;
        }
        let mut buffer = match surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(error) => {
                self.outcome =
                    SelectionOutcome::Failed(format!("failed to acquire overlay buffer: {error}"));
                return;
            }
        };
        let source_width = self.frame.width as usize;
        let copy_width = width as usize;
        let copy_height = height as usize;
        for row in 0..copy_height {
            let source_offset = row * source_width;
            let dest_offset = row * copy_width;
            buffer[dest_offset..dest_offset + copy_width].copy_from_slice(
                &self.frame.dimmed_pixels[source_offset..source_offset + copy_width],
            );
        }
        if let Some(selection) = active_selection {
            draw_selection_region(
                &mut buffer,
                width,
                height,
                self.frame.width,
                &self.frame.clear_pixels,
                selection.region,
                self.virtual_screen,
            );
        } else if let Some(window) = self.hovered_window.as_ref() {
            draw_hover_region(
                &mut buffer,
                width,
                height,
                window.bounds,
                self.virtual_screen,
            );
        }
        draw_mode_chip(
            &mut buffer,
            width,
            height,
            self.request.mode,
            self.last_cursor_position,
        );
        if let Err(error) = buffer.present() {
            self.outcome =
                SelectionOutcome::Failed(format!("failed to present overlay frame: {error}"));
            return;
        }
        println!(
            "overlay-debug render: frame={}x{} surface={}x{} capture_ms={} prepare_ms={} present_ms={}",
            self.frame.width,
            self.frame.height,
            width,
            height,
            self.frame.capture_elapsed.as_millis(),
            self.frame.prepare_elapsed.as_millis(),
            started.elapsed().as_millis()
        );
    }
}

impl ApplicationHandler for SelectionOverlayApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        match event_loop.create_window(selection_window_attributes_for_request(
            self.virtual_screen,
            self.request,
        )) {
            Ok(window) => {
                let window = Rc::new(window);
                print_window_debug("created", &window, self.virtual_screen);
                window.set_outer_position(PhysicalPosition::new(
                    self.virtual_screen.x,
                    self.virtual_screen.y,
                ));
                let _ = window.request_inner_size(PhysicalSize::new(
                    self.virtual_screen.width,
                    self.virtual_screen.height,
                ));
                print_window_debug("after request_inner_size", &window, self.virtual_screen);
                force_window_to_virtual_screen(&window, self.virtual_screen);
                print_window_debug(
                    "after force_window_to_virtual_screen",
                    &window,
                    self.virtual_screen,
                );
                window.set_window_level(WindowLevel::AlwaysOnTop);
                #[cfg(target_os = "windows")]
                window.set_skip_taskbar(true);
                window.focus_window();
                match SoftbufferSurface::new(&self.context, window.clone()) {
                    Ok(surface) => self.surface = Some(surface),
                    Err(error) => {
                        self.outcome = SelectionOutcome::Failed(format!(
                            "failed to create overlay render surface: {error}"
                        ));
                        event_loop.exit();
                        return;
                    }
                }
                self.window = Some(window.clone());
                self.render_frame();
                if matches!(self.outcome, SelectionOutcome::Failed(_)) {
                    event_loop.exit();
                    return;
                }
                window.set_visible(true);
                window.focus_window();
                println!(
                    "overlay-debug: rendered dimmed screenshot; click/move in white and black areas; press Escape to exit"
                );
                window.request_redraw();
            }
            Err(error) => {
                self.outcome = SelectionOutcome::Failed(format!(
                    "failed to create selection overlay window: {error}"
                ));
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if window.id() != window_id {
            return;
        }

        match event {
            WindowEvent::CloseRequested => {
                self.finish_with_outcome(
                    event_loop,
                    SelectionOutcome::Cancelled(SelectionCancelReason::WindowClosed),
                );
            }
            WindowEvent::RedrawRequested => {
                self.render_frame();
                if matches!(self.outcome, SelectionOutcome::Failed(_)) {
                    self.finish_with_outcome(event_loop, self.outcome.clone());
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                self.finish_with_outcome(
                    event_loop,
                    SelectionOutcome::Cancelled(SelectionCancelReason::Escape),
                );
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.last_cursor_position = Some(position);
                let point = selection_point_from_window_position(position, self.virtual_screen);
                self.drag_state.update(point);
                if self.drag_state.start.is_none() {
                    let previous = self.hovered_window.as_ref().map(|window| window.handle);
                    self.hovered_window = crate::screen::windows::window_at_point(
                        &self.selectable_windows,
                        point.x,
                        point.y,
                    )
                    .cloned();
                    if previous != self.hovered_window.as_ref().map(|window| window.handle) {
                        window.request_redraw();
                    }
                }
                window.request_redraw();
                self.cursor_move_events = self.cursor_move_events.saturating_add(1);
                if self.cursor_move_events <= 5 || self.cursor_move_events % 60 == 0 {
                    println!(
                        "overlay-debug cursor: local=({:.1},{:.1}) virtual=({},{}) moves={}",
                        position.x, position.y, point.x, point.y, self.cursor_move_events
                    );
                }
                if self.drag_state.start.is_some() {
                    window.request_redraw();
                    match self
                        .drag_state
                        .active_selection(self.request, SelectionFlags::new(self.shift_pressed))
                    {
                        Ok(Some(selection))
                            if self.cursor_move_events <= 5
                                || self.cursor_move_events % 30 == 0 =>
                        {
                            println!(
                                "overlay-debug drag: region=({},{} {}x{}) shift={}",
                                selection.region.x,
                                selection.region.y,
                                selection.region.width,
                                selection.region.height,
                                selection.flags.shift_pressed
                            );
                        }
                        Ok(_) => {}
                        Err(error) => {
                            self.finish_with_outcome(
                                event_loop,
                                SelectionOutcome::Failed(error.to_string()),
                            );
                        }
                    }
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.shift_pressed = modifiers.state().shift_key();
            }
            WindowEvent::MouseInput { state, button, .. }
                if state == ElementState::Pressed && button == MouseButton::Left =>
            {
                let Some(point) = self.current_cursor_selection_point() else {
                    println!("overlay-debug mouse down: no cursor position recorded");
                    return;
                };
                self.hovered_window = crate::screen::windows::window_at_point(
                    &self.selectable_windows,
                    point.x,
                    point.y,
                )
                .cloned();
                self.drag_state.begin(point);
                println!(
                    "overlay-debug mouse down: virtual=({},{}) shift={}",
                    point.x, point.y, self.shift_pressed
                );
            }
            WindowEvent::MouseInput { state, button, .. }
                if state == ElementState::Released && button == MouseButton::Left =>
            {
                let Some(point) = self.current_cursor_selection_point() else {
                    println!("overlay-debug mouse up: no cursor position recorded");
                    self.finish_with_outcome(
                        event_loop,
                        SelectionOutcome::Cancelled(SelectionCancelReason::BelowThreshold),
                    );
                    return;
                };
                self.finish_drag(event_loop, point);
            }
            _ => {}
        }
    }
}

pub fn run_selection_overlay_shell(request: SelectionRequest) -> Result<SelectionOutcome> {
    let join_handle = thread::spawn(move || run_selection_overlay_shell_on_current_thread(request));
    join_handle
        .join()
        .map_err(|_| RsnipError::Message("selection overlay thread panicked".to_owned()))?
}

fn run_selection_overlay_shell_on_current_thread(
    request: SelectionRequest,
) -> Result<SelectionOutcome> {
    set_overlay_thread_dpi_awareness();
    let virtual_screen = VirtualScreen::current()?;
    let frame = prepare_overlay_frame()?;
    print_virtual_screen_debug(virtual_screen, &frame);
    let selectable_windows = if request.mode == SelectionMode::Snip {
        crate::screen::windows::enumerate_selectable_windows().unwrap_or_else(|error| {
            println!("overlay-debug selectable windows unavailable: {error}");
            Vec::new()
        })
    } else {
        Vec::new()
    };
    let mut builder = EventLoop::builder();
    #[cfg(target_os = "windows")]
    {
        builder.with_any_thread(true);
    }
    let event_loop = builder.build().map_err(|error| {
        RsnipError::Message(format!("failed to build overlay event loop: {error}"))
    })?;
    let context = SoftbufferContext::new(event_loop.owned_display_handle()).map_err(|error| {
        RsnipError::Message(format!("failed to create overlay render context: {error}"))
    })?;
    let mut app = SelectionOverlayApp::new(request, virtual_screen, frame, context);
    app.selectable_windows = selectable_windows;
    event_loop.run_app(&mut app).map_err(|error| {
        RsnipError::Message(format!("selection overlay event loop failed: {error}"))
    })?;
    Ok(app.outcome)
}

pub fn selection_window_attributes(virtual_screen: VirtualScreen) -> WindowAttributes {
    selection_window_attributes_for_request(
        virtual_screen,
        SelectionRequest::new(SelectionMode::Snip),
    )
}

fn selection_window_attributes_for_request(
    virtual_screen: VirtualScreen,
    request: SelectionRequest,
) -> WindowAttributes {
    let attributes = Window::default_attributes()
        .with_title(selection_window_title(request.mode))
        .with_decorations(false)
        .with_resizable(false)
        .with_window_level(WindowLevel::AlwaysOnTop)
        .with_position(Position::Physical(PhysicalPosition::new(
            virtual_screen.x,
            virtual_screen.y,
        )))
        .with_inner_size(Size::Physical(PhysicalSize::new(
            virtual_screen.width,
            virtual_screen.height,
        )))
        .with_visible(false);

    #[cfg(target_os = "windows")]
    let attributes = attributes
        .with_skip_taskbar(true)
        .with_drag_and_drop(false)
        .with_undecorated_shadow(false)
        .with_class_name("RSnipSelectionOverlay");

    attributes
}

fn selection_window_title(mode: SelectionMode) -> &'static str {
    match mode {
        SelectionMode::Snip => "RSnip Snip Selection Overlay",
        SelectionMode::Record => "RSnip Record Selection Overlay",
        SelectionMode::Ocr => "RSnip OCR Selection Overlay",
    }
}

fn prepare_overlay_frame() -> Result<OverlayFrame> {
    let started = Instant::now();
    let capture = crate::screen::capture::capture_virtual_screen()?;
    let capture_elapsed = capture.metrics.elapsed;
    let image = capture.image;
    let (dimmed_pixels, clear_pixels) = bgra_to_overlay_pixels(&image.bgra);
    Ok(OverlayFrame {
        width: image.width,
        height: image.height,
        dimmed_pixels,
        clear_pixels,
        capture_elapsed,
        prepare_elapsed: started.elapsed().saturating_sub(capture_elapsed),
    })
}

fn bgra_to_overlay_pixels(bgra: &[u8]) -> (Vec<u32>, Vec<u32>) {
    const NUMERATOR: u16 = 55;
    const DENOMINATOR: u16 = 100;
    let mut dimmed_pixels = Vec::with_capacity(bgra.len() / 4);
    let mut clear_pixels = Vec::with_capacity(bgra.len() / 4);
    for pixel in bgra.chunks_exact(4) {
        let b = u32::from(pixel[0]);
        let g = u32::from(pixel[1]);
        let r = u32::from(pixel[2]);
        clear_pixels.push(b | (g << 8) | (r << 16));
        dimmed_pixels.push(
            (b * u32::from(NUMERATOR) / u32::from(DENOMINATOR))
                | ((g * u32::from(NUMERATOR) / u32::from(DENOMINATOR)) << 8)
                | ((r * u32::from(NUMERATOR) / u32::from(DENOMINATOR)) << 16),
        );
    }
    (dimmed_pixels, clear_pixels)
}

fn draw_selection_region(
    buffer: &mut [u32],
    surface_width: u32,
    surface_height: u32,
    frame_width: u32,
    clear_pixels: &[u32],
    region: CaptureRegion,
    virtual_screen: VirtualScreen,
) {
    let left = (region.x - virtual_screen.x).max(0) as u32;
    let top = (region.y - virtual_screen.y).max(0) as u32;
    let right = left.saturating_add(region.width).min(surface_width);
    let bottom = top.saturating_add(region.height).min(surface_height);
    if left >= right || top >= bottom {
        return;
    }

    let surface_width_usize = surface_width as usize;
    let frame_width_usize = frame_width as usize;
    let left_usize = left as usize;
    let right_usize = right as usize;
    for y in top as usize..bottom as usize {
        let dest_offset = y * surface_width_usize;
        let source_offset = y * frame_width_usize;
        buffer[dest_offset + left_usize..dest_offset + right_usize].copy_from_slice(
            &clear_pixels[source_offset + left_usize..source_offset + right_usize],
        );
    }

    draw_rect_border(
        buffer,
        surface_width,
        surface_height,
        left,
        top,
        right,
        bottom,
    );
}

fn draw_hover_region(
    buffer: &mut [u32],
    surface_width: u32,
    surface_height: u32,
    region: CaptureRegion,
    virtual_screen: VirtualScreen,
) {
    let left = (region.x - virtual_screen.x).max(0) as u32;
    let top = (region.y - virtual_screen.y).max(0) as u32;
    let right = left.saturating_add(region.width).min(surface_width);
    let bottom = top.saturating_add(region.height).min(surface_height);
    if left >= right || top >= bottom {
        return;
    }

    draw_rect_border_color(
        buffer,
        surface_width,
        surface_height,
        left,
        top,
        right,
        bottom,
        0x0000_ffff,
        3,
    );
}

fn draw_rect_border(
    buffer: &mut [u32],
    surface_width: u32,
    surface_height: u32,
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
) {
    const WHITE: u32 = 0x00ff_ffff;
    const BLACK: u32 = 0x0000_0000;
    draw_rect_border_color(
        buffer,
        surface_width,
        surface_height,
        left,
        top,
        right,
        bottom,
        BLACK,
        2,
    );
    draw_rect_border_color(
        buffer,
        surface_width,
        surface_height,
        left.saturating_add(1),
        top.saturating_add(1),
        right.saturating_sub(1),
        bottom.saturating_sub(1),
        WHITE,
        1,
    );
}

fn draw_rect_border_color(
    buffer: &mut [u32],
    surface_width: u32,
    surface_height: u32,
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
    color: u32,
    thickness: u32,
) {
    if left >= right || top >= bottom || surface_width == 0 || surface_height == 0 {
        return;
    }
    let width = surface_width as usize;
    for inset in 0..thickness {
        let x0 = left
            .saturating_add(inset)
            .min(surface_width.saturating_sub(1));
        let y0 = top
            .saturating_add(inset)
            .min(surface_height.saturating_sub(1));
        let x1 = right
            .saturating_sub(1 + inset)
            .min(surface_width.saturating_sub(1));
        let y1 = bottom
            .saturating_sub(1 + inset)
            .min(surface_height.saturating_sub(1));
        if x0 > x1 || y0 > y1 {
            break;
        }
        for x in x0..=x1 {
            buffer[y0 as usize * width + x as usize] = color;
            buffer[y1 as usize * width + x as usize] = color;
        }
        for y in y0..=y1 {
            buffer[y as usize * width + x0 as usize] = color;
            buffer[y as usize * width + x1 as usize] = color;
        }
    }
}

fn draw_mode_chip(
    buffer: &mut [u32],
    surface_width: u32,
    surface_height: u32,
    mode: SelectionMode,
    cursor_position: Option<PhysicalPosition<f64>>,
) {
    let (label, accent) = match mode {
        SelectionMode::Snip => ("SNIP", 0x0033_88ff),
        SelectionMode::Record => ("REC", 0x0000_3333),
        SelectionMode::Ocr => ("OCR", 0x0033_cc33),
    };
    let scale = 3;
    let text_width = text_5x7_width(label, scale);
    let chip_width = text_width + 24;
    let chip_height = 27;
    let (mut x, mut y) = cursor_position
        .map(|position| {
            (
                position.x.round() as i32 + 18,
                position.y.round() as i32 + 18,
            )
        })
        .unwrap_or((18, 18));
    x = x.clamp(0, surface_width.saturating_sub(chip_width) as i32);
    y = y.clamp(0, surface_height.saturating_sub(chip_height) as i32);

    draw_filled_rect(
        buffer,
        surface_width,
        surface_height,
        x,
        y,
        chip_width,
        chip_height,
        0x0020_2020,
    );
    draw_filled_rect(
        buffer,
        surface_width,
        surface_height,
        x,
        y,
        6,
        chip_height,
        accent,
    );
    draw_rect_border_color(
        buffer,
        surface_width,
        surface_height,
        x as u32,
        y as u32,
        (x as u32).saturating_add(chip_width),
        (y as u32).saturating_add(chip_height),
        0x00ff_ffff,
        1,
    );
    draw_text_5x7(
        buffer,
        surface_width,
        surface_height,
        x + 14,
        y + 5,
        label,
        0x00ff_ffff,
        scale,
    );
}

fn draw_filled_rect(
    buffer: &mut [u32],
    surface_width: u32,
    surface_height: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    color: u32,
) {
    if surface_width == 0 || surface_height == 0 || width == 0 || height == 0 {
        return;
    }
    let left = x.max(0) as u32;
    let top = y.max(0) as u32;
    let right = (x.saturating_add(width as i32)).max(0) as u32;
    let bottom = (y.saturating_add(height as i32)).max(0) as u32;
    let right = right.min(surface_width);
    let bottom = bottom.min(surface_height);
    if left >= right || top >= bottom {
        return;
    }
    let stride = surface_width as usize;
    for row in top as usize..bottom as usize {
        let offset = row * stride;
        for column in left as usize..right as usize {
            buffer[offset + column] = color;
        }
    }
}

fn draw_text_5x7(
    buffer: &mut [u32],
    surface_width: u32,
    surface_height: u32,
    x: i32,
    y: i32,
    text: &str,
    color: u32,
    scale: u32,
) {
    let mut cursor_x = x;
    for character in text.chars() {
        draw_char_5x7(
            buffer,
            surface_width,
            surface_height,
            cursor_x,
            y,
            character,
            color,
            scale,
        );
        cursor_x += (5 * scale + scale) as i32;
    }
}

fn text_5x7_width(text: &str, scale: u32) -> u32 {
    let count = text.chars().count() as u32;
    if count == 0 {
        return 0;
    }
    count * 5 * scale + count.saturating_sub(1) * scale
}

fn draw_char_5x7(
    buffer: &mut [u32],
    surface_width: u32,
    surface_height: u32,
    x: i32,
    y: i32,
    character: char,
    color: u32,
    scale: u32,
) {
    let Some(pattern) = font_5x7(character) else {
        return;
    };
    for (row, bits) in pattern.iter().enumerate() {
        for col in 0..5 {
            if bits & (1 << (4 - col)) == 0 {
                continue;
            }
            draw_filled_rect(
                buffer,
                surface_width,
                surface_height,
                x + col * scale as i32,
                y + row as i32 * scale as i32,
                scale,
                scale,
                color,
            );
        }
    }
}

fn font_5x7(character: char) -> Option<[u8; 7]> {
    match character.to_ascii_uppercase() {
        'C' => Some([
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ]),
        'E' => Some([
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ]),
        'I' => Some([
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111,
        ]),
        'N' => Some([
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ]),
        'O' => Some([
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ]),
        'P' => Some([
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ]),
        'R' => Some([
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ]),
        'S' => Some([
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ]),
        _ => None,
    }
}

fn print_virtual_screen_debug(virtual_screen: VirtualScreen, frame: &OverlayFrame) {
    println!(
        "overlay-debug virtual_screen: x={} y={} width={} height={} right={} bottom={}",
        virtual_screen.x,
        virtual_screen.y,
        virtual_screen.width,
        virtual_screen.height,
        virtual_screen.right(),
        virtual_screen.bottom()
    );
    match crate::screen::monitor::enumerate_monitors() {
        Ok(monitors) => {
            println!("overlay-debug monitors: {}", monitors.len());
            for (index, monitor) in monitors.iter().enumerate() {
                println!(
                    "overlay-debug monitor[{index}]: bounds=({},{} {}x{}) work=({},{} {}x{}) primary={}",
                    monitor.bounds.x,
                    monitor.bounds.y,
                    monitor.bounds.width,
                    monitor.bounds.height,
                    monitor.work_area.x,
                    monitor.work_area.y,
                    monitor.work_area.width,
                    monitor.work_area.height,
                    monitor.primary
                );
            }
        }
        Err(error) => println!("overlay-debug monitors: failed: {error}"),
    }
    println!(
        "overlay-debug capture: size={}x{} capture_ms={} prepare_ms={} pixels={}",
        frame.width,
        frame.height,
        frame.capture_elapsed.as_millis(),
        frame.prepare_elapsed.as_millis(),
        frame.dimmed_pixels.len()
    );
}

fn print_window_debug(label: &str, window: &Window, virtual_screen: VirtualScreen) {
    let inner = window.inner_size();
    let outer = window.outer_size();
    let outer_pos = window.outer_position();
    let scale_factor = window.scale_factor();
    println!(
        "overlay-debug window[{label}]: scale_factor={scale_factor:.4} inner={}x{} outer={}x{} outer_pos={outer_pos:?} requested_virtual={}x{} at {},{}",
        inner.width,
        inner.height,
        outer.width,
        outer.height,
        virtual_screen.width,
        virtual_screen.height,
        virtual_screen.x,
        virtual_screen.y
    );
    print_platform_window_debug(label, window);
}

#[cfg(target_os = "windows")]
fn print_platform_window_debug(label: &str, window: &Window) {
    let Ok(handle) = window.window_handle() else {
        println!("overlay-debug win32[{label}]: window_handle unavailable");
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        println!("overlay-debug win32[{label}]: non-Win32 handle");
        return;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let mut window_rect = RECT::default();
    let mut client_rect = RECT::default();
    let window_ok = unsafe { GetWindowRect(hwnd, &mut window_rect) }.is_ok();
    let client_ok = unsafe { GetClientRect(hwnd, &mut client_rect) }.is_ok();
    println!(
        "overlay-debug win32[{label}]: hwnd={:?} dpi={} window_ok={} window_rect=({},{} {}x{}) client_ok={} client_rect=({},{} {}x{})",
        hwnd.0,
        dpi,
        window_ok,
        window_rect.left,
        window_rect.top,
        window_rect.right.saturating_sub(window_rect.left),
        window_rect.bottom.saturating_sub(window_rect.top),
        client_ok,
        client_rect.left,
        client_rect.top,
        client_rect.right.saturating_sub(client_rect.left),
        client_rect.bottom.saturating_sub(client_rect.top)
    );
}

#[cfg(not(target_os = "windows"))]
fn print_platform_window_debug(_label: &str, _window: &Window) {}

#[cfg(target_os = "windows")]
fn set_overlay_thread_dpi_awareness() {
    // SAFETY: This only affects the current thread before creating the overlay window or reading
    // virtual-screen metrics for it. The previous context is intentionally not restored because
    // the overlay event loop owns this short-lived thread path until it exits.
    let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

#[cfg(not(target_os = "windows"))]
fn set_overlay_thread_dpi_awareness() {}

#[cfg(target_os = "windows")]
fn force_window_to_virtual_screen(window: &Window, virtual_screen: VirtualScreen) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };

    let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
    // SAFETY: The HWND comes from the live winit window on the current thread.
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let width = scale_dimension_for_dpi(virtual_screen.width, dpi, 96);
    let height = scale_dimension_for_dpi(virtual_screen.height, dpi, 96);

    // SAFETY: The HWND comes from the live winit window on the current thread. SetWindowPos is
    // used only to force exact bounds after translating DPI-virtualized metrics to physical size.
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            virtual_screen.x,
            virtual_screen.y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    };
}

#[cfg(target_os = "windows")]
fn scale_dimension_for_dpi(value: u32, dpi: u32, base_dpi: u32) -> i32 {
    let scaled = u64::from(value)
        .saturating_mul(u64::from(dpi))
        .saturating_add(u64::from(base_dpi / 2))
        / u64::from(base_dpi);
    i32::try_from(scaled).unwrap_or(i32::MAX)
}

#[cfg(not(target_os = "windows"))]
fn force_window_to_virtual_screen(window: &Window, virtual_screen: VirtualScreen) {
    let _ = window.request_inner_size(PhysicalSize::new(
        virtual_screen.width,
        virtual_screen.height,
    ));
    window.set_outer_position(PhysicalPosition::new(virtual_screen.x, virtual_screen.y));
}

#[cfg(target_os = "windows")]
fn current_platform_cursor_selection_point(
    virtual_screen: VirtualScreen,
) -> Option<SelectionPoint> {
    let mut point = POINT::default();
    // SAFETY: GetCursorPos writes to a valid POINT on success and has no aliasing requirements.
    unsafe { GetCursorPos(&mut point) }
        .is_ok()
        .then(|| clamp_virtual_point(point.x, point.y, virtual_screen))
}

#[cfg(not(target_os = "windows"))]
fn current_platform_cursor_selection_point(
    _virtual_screen: VirtualScreen,
) -> Option<SelectionPoint> {
    None
}

fn selection_point_from_window_position(
    position: PhysicalPosition<f64>,
    virtual_screen: VirtualScreen,
) -> SelectionPoint {
    let max_x = f64::from(virtual_screen.width);
    let max_y = f64::from(virtual_screen.height);
    let local_x = position.x.clamp(0.0, max_x);
    let local_y = position.y.clamp(0.0, max_y);
    clamp_virtual_point(
        virtual_screen.x.saturating_add(local_x.round() as i32),
        virtual_screen.y.saturating_add(local_y.round() as i32),
        virtual_screen,
    )
}

fn clamp_virtual_point(x: i32, y: i32, virtual_screen: VirtualScreen) -> SelectionPoint {
    SelectionPoint::new(
        x.clamp(virtual_screen.x, virtual_screen.right()),
        y.clamp(virtual_screen.y, virtual_screen.bottom()),
    )
}

pub fn outcome_from_drag(
    request: SelectionRequest,
    start: SelectionPoint,
    end: SelectionPoint,
    flags: SelectionFlags,
) -> SelectionOutcome {
    match Selection::from_drag(request.mode, start, end, flags, request.threshold_px) {
        Ok(Some(selection)) => SelectionOutcome::Selected(selection),
        Ok(None) => SelectionOutcome::Cancelled(SelectionCancelReason::BelowThreshold),
        Err(error) => SelectionOutcome::Failed(error.to_string()),
    }
}

pub fn normalize_drag_region(
    start: SelectionPoint,
    end: SelectionPoint,
    threshold_px: u32,
) -> Result<Option<CaptureRegion>> {
    let left = start.x.min(end.x);
    let top = start.y.min(end.y);
    let right = start.x.max(end.x);
    let bottom = start.y.max(end.y);
    let width = u32::try_from(right - left)
        .map_err(|_| RsnipError::Message("selection width overflow".to_owned()))?;
    let height = u32::try_from(bottom - top)
        .map_err(|_| RsnipError::Message("selection height overflow".to_owned()))?;

    if width < threshold_px || height < threshold_px {
        return Ok(None);
    }

    CaptureRegion::new(left, top, width, height).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_drag_in_any_direction() {
        let region = normalize_drag_region(
            SelectionPoint::new(100, 80),
            SelectionPoint::new(10, 20),
            DEFAULT_SELECTION_THRESHOLD_PX,
        )
        .unwrap()
        .unwrap();

        assert_eq!(region.x, 10);
        assert_eq!(region.y, 20);
        assert_eq!(region.width, 90);
        assert_eq!(region.height, 60);
    }

    #[test]
    fn supports_negative_virtual_coordinates() {
        let region = normalize_drag_region(
            SelectionPoint::new(-200, -100),
            SelectionPoint::new(-50, 25),
            DEFAULT_SELECTION_THRESHOLD_PX,
        )
        .unwrap()
        .unwrap();

        assert_eq!(region.x, -200);
        assert_eq!(region.y, -100);
        assert_eq!(region.width, 150);
        assert_eq!(region.height, 125);
    }

    #[test]
    fn rejects_drags_below_threshold() {
        let region = normalize_drag_region(
            SelectionPoint::new(10, 10),
            SelectionPoint::new(12, 100),
            DEFAULT_SELECTION_THRESHOLD_PX,
        )
        .unwrap();

        assert_eq!(region, None);
    }

    #[test]
    fn mouse_drag_state_tracks_active_selection() {
        let request = SelectionRequest::with_threshold(SelectionMode::Snip, 3);
        let mut drag = MouseDragState::default();

        drag.begin(SelectionPoint::new(10, 10));
        drag.update(SelectionPoint::new(50, 40));

        let selection = drag
            .active_selection(request, SelectionFlags::new(true))
            .unwrap()
            .unwrap();
        assert_eq!(
            selection.region,
            CaptureRegion::new(10, 10, 40, 30).unwrap()
        );
        assert!(selection.flags.shift_pressed);
        assert_eq!(
            drag.finish(SelectionPoint::new(50, 40)),
            Some((SelectionPoint::new(10, 10), SelectionPoint::new(50, 40)))
        );
        assert_eq!(
            drag.active_selection(request, SelectionFlags::new(false))
                .unwrap(),
            None
        );
    }

    #[test]
    fn draws_selection_region_with_clear_interior_and_border() {
        let virtual_screen = VirtualScreen {
            x: 10,
            y: 20,
            width: 8,
            height: 8,
        };
        let mut buffer = vec![0x0001_0101; 64];
        let clear_pixels = vec![0x0002_0202; 64];

        draw_selection_region(
            &mut buffer,
            8,
            8,
            8,
            &clear_pixels,
            CaptureRegion::new(11, 21, 6, 6).unwrap(),
            virtual_screen,
        );

        assert_eq!(buffer[9], 0x0000_0000);
        assert_eq!(buffer[18], 0x00ff_ffff);
        assert_eq!(buffer[27], 0x0002_0202);
    }

    #[test]
    fn clamps_virtual_points_to_virtual_screen() {
        let virtual_screen = VirtualScreen {
            x: -100,
            y: 50,
            width: 400,
            height: 300,
        };

        assert_eq!(
            clamp_virtual_point(-200, 999, virtual_screen),
            SelectionPoint::new(-100, 350)
        );
    }

    #[test]
    fn converts_window_position_to_virtual_point_with_clamp() {
        let virtual_screen = VirtualScreen {
            x: -100,
            y: 50,
            width: 400,
            height: 300,
        };

        assert_eq!(
            selection_point_from_window_position(PhysicalPosition::new(25.4, 10.5), virtual_screen),
            SelectionPoint::new(-75, 61)
        );
        assert_eq!(
            selection_point_from_window_position(
                PhysicalPosition::new(999.0, -5.0),
                virtual_screen
            ),
            SelectionPoint::new(300, 50)
        );
    }

    #[test]
    fn outcome_preserves_mode_region_and_shift_flag() {
        let outcome = outcome_from_drag(
            SelectionRequest::new(SelectionMode::Ocr),
            SelectionPoint::new(10, 10),
            SelectionPoint::new(30, 40),
            SelectionFlags::new(true),
        );

        assert_eq!(
            outcome,
            SelectionOutcome::Selected(Selection {
                mode: SelectionMode::Ocr,
                region: CaptureRegion::new(10, 10, 20, 30).unwrap(),
                flags: SelectionFlags {
                    shift_pressed: true,
                },
                window_handle: None,
            })
        );
    }
}
