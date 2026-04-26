use std::fs;
use std::num::NonZeroU32;
use std::path::Path;
use std::rc::Rc;
use std::sync::OnceLock;

use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};
use softbuffer::{Context as SoftbufferContext, Surface as SoftbufferSurface};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize, Size};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, OwnedDisplayHandle};
use winit::keyboard::{Key, ModifiersState, NamedKey};
#[cfg(target_os = "windows")]
use winit::platform::windows::{EventLoopBuilderExtWindows, WindowAttributesExtWindows};
#[cfg(target_os = "windows")]
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowAttributes, WindowId, WindowLevel};

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{COLORREF, HWND};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, LWA_ALPHA, SetLayeredWindowAttributes, SetWindowLongPtrW,
    WS_EX_LAYERED,
};

use crate::editor::{
    EditorAnnotation, EditorColor, EditorDocument, EditorPoint, EditorSessionImage, EditorTool,
};
use crate::errors::{Result, RsnipError};
use crate::screen::capture::CapturedImage;

const TITLE_BAR_HEIGHT: f64 = 36.0;
const CLOSE_BUTTON_WIDTH: f64 = 44.0;
const COPY_BUTTON_WIDTH: f64 = 92.0;
const TOOLBAR_WIDTH: u32 = 56;
const CONTENT_PADDING: u32 = 16;
const TOOL_BUTTON_SIZE: u32 = 34;
const TOOL_BUTTON_GAP: u32 = 8;
const MIN_WINDOW_WIDTH: u32 = 360;
const MIN_WINDOW_HEIGHT: u32 = 240;
const MAX_INITIAL_WINDOW_WIDTH: u32 = 1200;
const MAX_INITIAL_WINDOW_HEIGHT: u32 = 900;
const STROKE_THICKNESS: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorCanvasLayout {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub image_width: u32,
    pub image_height: u32,
}

impl EditorCanvasLayout {
    pub fn image_point_from_window_position(
        self,
        position: PhysicalPosition<f64>,
    ) -> Option<EditorPoint> {
        let x = position.x.round() as i32;
        let y = position.y.round() as i32;
        let left = i32::try_from(self.x).ok()?;
        let top = i32::try_from(self.y).ok()?;
        let right = left.checked_add(i32::try_from(self.width).ok()?)?;
        let bottom = top.checked_add(i32::try_from(self.height).ok()?)?;
        if x < left || y < top || x >= right || y >= bottom {
            return None;
        }
        Some(EditorPoint::new(x - left, y - top))
    }
}

struct EditorShellApp {
    image: EditorSessionImage,
    context: SoftbufferContext<OwnedDisplayHandle>,
    surface: Option<SoftbufferSurface<OwnedDisplayHandle, Rc<Window>>>,
    window: Option<Rc<Window>>,
    cursor_position: Option<PhysicalPosition<f64>>,
    modifiers: ModifiersState,
    document: EditorDocument,
    active_tool: EditorTool,
    active_color: EditorColor,
    current_stroke: Option<Vec<EditorPoint>>,
    current_drag: Option<(EditorTool, EditorPoint, EditorPoint)>,
}

impl EditorShellApp {
    fn new(image: EditorSessionImage, context: SoftbufferContext<OwnedDisplayHandle>) -> Self {
        let document = EditorDocument::new(image.image_size());
        Self {
            image,
            context,
            surface: None,
            window: None,
            cursor_position: None,
            modifiers: ModifiersState::default(),
            document,
            active_tool: EditorTool::default(),
            active_color: EditorColor::default(),
            current_stroke: None,
            current_drag: None,
        }
    }

    fn finish(&mut self, event_loop: &ActiveEventLoop) {
        self.surface = None;
        if let Some(window) = &self.window {
            window.set_visible(false);
        }
        event_loop.exit();
    }

    fn handle_left_press(&mut self, event_loop: &ActiveEventLoop) {
        let Some(position) = self.cursor_position else {
            return;
        };
        let Some(window) = &self.window else {
            return;
        };
        let size = window.inner_size();
        if position.y <= TITLE_BAR_HEIGHT {
            let close_left = f64::from(size.width).saturating_sub_f64(CLOSE_BUTTON_WIDTH);
            let copy_left = close_left.saturating_sub_f64(COPY_BUTTON_WIDTH);
            if position.x >= close_left {
                self.finish(event_loop);
                return;
            }
            if position.x >= copy_left && position.x < close_left {
                self.copy_current_image_to_clipboard();
                return;
            }
            if let Err(error) = window.drag_window() {
                eprintln!("editor drag failed: {error}");
            }
            return;
        }

        let layout = canvas_layout(size.width, size.height, self.image.width, self.image.height);
        if let Some(point) = layout.image_point_from_window_position(position) {
            match self.active_tool {
                EditorTool::Pen => self.current_stroke = Some(vec![point]),
                EditorTool::Arrow
                | EditorTool::Line
                | EditorTool::Rectangle
                | EditorTool::Redact => {
                    self.current_drag = Some((self.active_tool, point, point));
                }
                EditorTool::Step => {
                    self.document.push_next_step(point);
                    window.request_redraw();
                }
            }
        }
    }

    fn handle_left_release(&mut self) {
        if let Some(stroke) = self.current_stroke.take()
            && stroke.len() > 1
        {
            self.document.push_annotation(EditorAnnotation::Pen {
                points: stroke,
                color: self.active_color,
            });
        }
        if let Some((tool, start, end)) = self.current_drag.take() {
            let end = constrain_point_if_shift(start, end, self.modifiers.shift_key());
            if start != end {
                match tool {
                    EditorTool::Line => self.document.push_annotation(EditorAnnotation::Line {
                        start,
                        end,
                        color: self.active_color,
                    }),
                    EditorTool::Arrow => self.document.push_annotation(EditorAnnotation::Arrow {
                        start,
                        end,
                        color: self.active_color,
                    }),
                    EditorTool::Rectangle => {
                        self.document.push_annotation(EditorAnnotation::Rectangle {
                            rect: crate::editor::EditorRect::from_points(start, end),
                            color: self.active_color,
                        })
                    }
                    EditorTool::Redact => self.document.push_annotation(EditorAnnotation::Redact {
                        rect: crate::editor::EditorRect::from_points(start, end),
                    }),
                    _ => {}
                }
            }
        }
    }

    fn update_current_drag(&mut self, position: PhysicalPosition<f64>) {
        let Some(window) = &self.window else {
            return;
        };
        let size = window.inner_size();
        let layout = canvas_layout(size.width, size.height, self.image.width, self.image.height);
        let Some(point) = layout.image_point_from_window_position(position) else {
            return;
        };
        if let Some(stroke) = &mut self.current_stroke
            && stroke.last().copied() != Some(point)
        {
            stroke.push(point);
            window.request_redraw();
        }
        if let Some((_, _, end)) = &mut self.current_drag
            && *end != point
        {
            *end = point;
            window.request_redraw();
        }
    }

    fn copy_current_image_to_clipboard(&self) -> bool {
        match compose_editor_image(
            &self.image,
            self.document.annotations(),
            self.current_stroke.as_deref(),
            self.current_drag,
            self.active_color,
            self.modifiers.shift_key(),
        )
        .and_then(|image| crate::clipboard::copy_image(&image))
        {
            Ok(()) => {
                println!(
                    "editor copied image to clipboard: {}x{}",
                    self.image.width, self.image.height
                );
                true
            }
            Err(error) => {
                eprintln!("editor copy failed: {error}");
                false
            }
        }
    }

    fn render(&mut self) -> Result<()> {
        let Some(window) = &self.window else {
            return Ok(());
        };
        let Some(surface) = &mut self.surface else {
            return Ok(());
        };
        let size = window.inner_size();
        let width = NonZeroU32::new(size.width.max(1))
            .ok_or_else(|| RsnipError::Message("editor window width is zero".to_owned()))?;
        let height = NonZeroU32::new(size.height.max(1))
            .ok_or_else(|| RsnipError::Message("editor window height is zero".to_owned()))?;
        surface.resize(width, height).map_err(|error| {
            RsnipError::Message(format!("failed to resize editor surface: {error}"))
        })?;

        let width = size.width.max(1);
        let height = size.height.max(1);
        let mut buffer = surface.buffer_mut().map_err(|error| {
            RsnipError::Message(format!("failed to get editor frame buffer: {error}"))
        })?;
        draw_editor_shell(
            &mut buffer,
            width,
            height,
            &self.image,
            self.active_tool,
            self.active_color,
            self.document.annotations(),
            self.current_stroke.as_deref(),
            self.current_drag,
            self.modifiers.shift_key(),
        );
        buffer.present().map_err(|error| {
            RsnipError::Message(format!("failed to present editor frame: {error}"))
        })?;
        Ok(())
    }
}

impl ApplicationHandler for EditorShellApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attributes = editor_window_attributes(&self.image);
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Rc::new(window),
            Err(error) => {
                eprintln!("failed to create editor window: {error}");
                event_loop.exit();
                return;
            }
        };
        set_editor_window_alpha(&window, 0);
        let surface = match SoftbufferSurface::new(&self.context, window.clone()) {
            Ok(surface) => surface,
            Err(error) => {
                eprintln!("failed to create editor surface: {error}");
                event_loop.exit();
                return;
            }
        };
        self.surface = Some(surface);
        self.window = Some(window.clone());
        if let Err(error) = self.render() {
            eprintln!("editor initial render failed: {error}");
            self.finish(event_loop);
            return;
        }
        window.set_visible(true);
        if let Err(error) = self.render() {
            eprintln!("editor visible render failed: {error}");
            self.finish(event_loop);
            return;
        }
        set_editor_window_alpha(&window, 255);
        window.focus_window();
        window.request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => self.finish(event_loop),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position = Some(position);
                self.update_current_drag(position);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => self.handle_left_press(event_loop),
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                self.handle_left_release();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed
                    && matches!(event.logical_key, Key::Named(NamedKey::Escape))
                {
                    self.finish(event_loop);
                } else if event.state == ElementState::Pressed
                    && self.modifiers.control_key()
                    && matches!(event.logical_key, Key::Character(ref key) if key.eq_ignore_ascii_case("c"))
                {
                    if self.copy_current_image_to_clipboard() {
                        self.finish(event_loop);
                    }
                } else if event.state == ElementState::Pressed
                    && self.modifiers.control_key()
                    && matches!(event.logical_key, Key::Character(ref key) if key.eq_ignore_ascii_case("z"))
                {
                    if self.document.undo() {
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                } else if event.state == ElementState::Pressed
                    && let Key::Character(ref key) = event.logical_key
                    && let Some(key) = key.chars().next()
                {
                    if let Some(tool) = EditorTool::from_shortcut(key) {
                        self.active_tool = tool;
                        self.document.set_active_tool(tool);
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    } else if let Some(color) = EditorColor::from_shortcut(key) {
                        self.active_color = color;
                        self.document.set_active_color(color);
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.render() {
                    eprintln!("editor render failed: {error}");
                    self.finish(event_loop);
                }
            }
            WindowEvent::Resized(_) => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }
}

pub fn run_editor_shell(input_path: &Path) -> Result<()> {
    let image = crate::editor::load_editor_input_png(input_path)?;
    let mut builder = EventLoop::builder();
    #[cfg(target_os = "windows")]
    {
        builder.with_any_thread(true);
    }
    let event_loop = builder.build().map_err(|error| {
        RsnipError::Message(format!("failed to build editor event loop: {error}"))
    })?;
    let context = SoftbufferContext::new(event_loop.owned_display_handle()).map_err(|error| {
        RsnipError::Message(format!("failed to create editor render context: {error}"))
    })?;
    let mut app = EditorShellApp::new(image, context);
    event_loop
        .run_app(&mut app)
        .map_err(|error| RsnipError::Message(format!("editor event loop failed: {error}")))?;
    Ok(())
}

fn editor_window_attributes(image: &EditorSessionImage) -> WindowAttributes {
    let width = image
        .width
        .saturating_add(TOOLBAR_WIDTH)
        .saturating_add(CONTENT_PADDING * 3)
        .clamp(MIN_WINDOW_WIDTH, MAX_INITIAL_WINDOW_WIDTH);
    let height = image
        .height
        .saturating_add(TITLE_BAR_HEIGHT as u32)
        .saturating_add(CONTENT_PADDING * 2)
        .clamp(MIN_WINDOW_HEIGHT, MAX_INITIAL_WINDOW_HEIGHT);
    let attributes = Window::default_attributes()
        .with_title("RSnip Editor")
        .with_decorations(false)
        .with_resizable(true)
        .with_window_level(WindowLevel::AlwaysOnTop)
        .with_inner_size(Size::Physical(PhysicalSize::new(width, height)))
        .with_visible(false);

    #[cfg(target_os = "windows")]
    let attributes = attributes
        .with_drag_and_drop(false)
        .with_undecorated_shadow(true)
        .with_class_name("RSnipEditor");

    attributes
}

#[cfg(target_os = "windows")]
fn set_editor_window_alpha(window: &Window, alpha: u8) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
    // SAFETY: The HWND belongs to the live winit window on this thread. We only add the layered
    // style and adjust whole-window alpha to hide default OS paint before the first editor frame.
    unsafe {
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_style | WS_EX_LAYERED.0 as isize);
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
    }
}

#[cfg(not(target_os = "windows"))]
fn set_editor_window_alpha(_window: &Window, _alpha: u8) {}

fn draw_editor_shell(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    image: &EditorSessionImage,
    active_tool: EditorTool,
    active_color: EditorColor,
    annotations: &[EditorAnnotation],
    current_stroke: Option<&[EditorPoint]>,
    current_drag: Option<(EditorTool, EditorPoint, EditorPoint)>,
    shift_pressed: bool,
) {
    fill(buffer, width, height, 0x00303030);
    draw_title_bar(buffer, width, height);
    draw_toolbar(buffer, width, height, active_tool, active_color);
    let layout = canvas_layout(width, height, image.width, image.height);
    draw_rect(
        buffer,
        width,
        layout.x,
        layout.y,
        layout.width,
        layout.height,
        0x00444444,
    );
    draw_image(buffer, width, height, image, layout);
    draw_annotations(buffer, width, height, layout, annotations);
    if let Some(points) = current_stroke {
        draw_pen_points(buffer, width, height, layout, points, active_color);
    }
    if let Some((tool, start, end)) = current_drag {
        let end = constrain_point_if_shift(start, end, shift_pressed);
        match tool {
            EditorTool::Line => {
                draw_editor_line(buffer, width, height, layout, start, end, active_color)
            }
            EditorTool::Arrow => {
                draw_editor_arrow(buffer, width, height, layout, start, end, active_color)
            }
            EditorTool::Rectangle => draw_editor_rect_outline(
                buffer,
                width,
                height,
                layout,
                crate::editor::EditorRect::from_points(start, end),
                active_color,
            ),
            EditorTool::Redact => draw_editor_redact(
                buffer,
                width,
                height,
                layout,
                crate::editor::EditorRect::from_points(start, end),
            ),
            _ => {}
        }
    }
    draw_rect_outline(
        buffer,
        width,
        height,
        layout.x.saturating_sub(1),
        layout.y.saturating_sub(1),
        layout.width.saturating_add(2),
        layout.height.saturating_add(2),
        0x00888888,
    );
}

fn fill(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    color: u32,
) {
    let len = width as usize * height as usize;
    for index in 0..len {
        buffer[index] = color;
    }
}

fn draw_title_bar(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
) {
    let title_height = (TITLE_BAR_HEIGHT as u32).min(height);
    draw_rect(buffer, width, 0, 0, width, title_height, 0x00222222);
    let close_x = width.saturating_sub(CLOSE_BUTTON_WIDTH as u32);
    let copy_x = close_x.saturating_sub(COPY_BUTTON_WIDTH as u32);
    draw_rect(
        buffer,
        width,
        copy_x,
        0,
        COPY_BUTTON_WIDTH as u32,
        title_height,
        0x00385f88,
    );
    draw_rect(
        buffer,
        width,
        close_x,
        0,
        CLOSE_BUTTON_WIDTH as u32,
        title_height,
        0x00883333,
    );
    draw_text(
        buffer,
        width,
        height,
        14,
        10,
        "RSNIP EDITOR",
        0x00e8e8e8,
        16.0,
        FontFace::Ui,
    );
    draw_text(
        buffer,
        width,
        height,
        copy_x + 14,
        9,
        "Copiar",
        0x00ffffff,
        16.0,
        FontFace::Ui,
    );
    draw_text(
        buffer,
        width,
        height,
        close_x + 15,
        8,
        "×",
        0x00ffffff,
        22.0,
        FontFace::Ui,
    );
}

fn draw_toolbar(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    active_tool: EditorTool,
    active_color: EditorColor,
) {
    let x = CONTENT_PADDING;
    let y = TITLE_BAR_HEIGHT as u32 + CONTENT_PADDING;
    draw_rect(
        buffer,
        width,
        x,
        y,
        TOOLBAR_WIDTH,
        toolbar_height(height),
        0x00282828,
    );

    for (index, tool) in [
        EditorTool::Pen,
        EditorTool::Arrow,
        EditorTool::Line,
        EditorTool::Rectangle,
        EditorTool::Redact,
        EditorTool::Step,
    ]
    .iter()
    .enumerate()
    {
        let button_y = y + TOOL_BUTTON_GAP + index as u32 * (TOOL_BUTTON_SIZE + TOOL_BUTTON_GAP);
        let color = if *tool == active_tool {
            0x00606060
        } else {
            0x003c3c3c
        };
        draw_rect(
            buffer,
            width,
            x + 11,
            button_y,
            TOOL_BUTTON_SIZE,
            TOOL_BUTTON_SIZE,
            color,
        );
        draw_rect_outline(
            buffer,
            width,
            height,
            x + 11,
            button_y,
            TOOL_BUTTON_SIZE,
            TOOL_BUTTON_SIZE,
            0x00aaaaaa,
        );
        draw_tool_icon(buffer, width, height, *tool, x + 11, button_y);
    }

    let colors_y = height.saturating_sub(CONTENT_PADDING + 4 * 18 + 3 * 6 + 2);
    for (index, color) in [
        EditorColor::Red,
        EditorColor::Blue,
        EditorColor::Green,
        EditorColor::Yellow,
    ]
    .iter()
    .enumerate()
    {
        let swatch_y = colors_y + index as u32 * 24;
        let rgb = rgba_to_softbuffer_color(color.rgba());
        draw_rect(buffer, width, x + 17, swatch_y, 22, 18, rgb);
        if *color == active_color {
            draw_rect_outline(
                buffer,
                width,
                height,
                x + 15,
                swatch_y - 2,
                26,
                22,
                0x00ffffff,
            );
        }
    }
}

fn draw_tool_icon(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    tool: EditorTool,
    x: u32,
    y: u32,
) {
    let (icon, size, offset_x, offset_y) = match tool {
        EditorTool::Pen => ("✎", 24.0, 8, 5),
        EditorTool::Arrow => ("↗", 24.0, 8, 5),
        EditorTool::Line => ("╱", 24.0, 9, 5),
        EditorTool::Rectangle => ("□", 25.0, 7, 4),
        EditorTool::Redact => ("▰", 23.0, 8, 6),
        EditorTool::Step => ("①", 22.0, 7, 6),
    };
    draw_text(
        buffer,
        width,
        height,
        x + offset_x,
        y + offset_y,
        icon,
        0x00f0f0f0,
        size,
        FontFace::Symbol,
    );
}

#[derive(Debug, Clone, Copy)]
enum FontFace {
    Ui,
    Symbol,
}

static UI_FONT: OnceLock<Option<FontArc>> = OnceLock::new();
static SYMBOL_FONT: OnceLock<Option<FontArc>> = OnceLock::new();

fn draw_text(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    text: &str,
    color: u32,
    size: f32,
    face: FontFace,
) {
    let Some(font) = font(face) else {
        draw_text_5x7(buffer, width, x, y, text, color, 2);
        return;
    };
    let scaled = font.as_scaled(PxScale::from(size));
    let mut caret = point(x as f32, y as f32 + scaled.ascent());
    let rgba = softbuffer_color_to_rgba(color);
    for character in text.chars() {
        let glyph_id = scaled.glyph_id(character);
        let glyph = glyph_id.with_scale_and_position(PxScale::from(size), caret);
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, alpha| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
                    return;
                }
                blend_pixel(buffer, width, px, py, rgba, alpha);
            });
        }
        caret.x += scaled.h_advance(glyph_id);
    }
}

fn font(face: FontFace) -> Option<&'static FontArc> {
    match face {
        FontFace::Ui => UI_FONT
            .get_or_init(|| load_font("C:/Windows/Fonts/segoeui.ttf"))
            .as_ref(),
        FontFace::Symbol => SYMBOL_FONT
            .get_or_init(|| load_font("C:/Windows/Fonts/seguisym.ttf"))
            .as_ref()
            .or_else(|| {
                UI_FONT
                    .get_or_init(|| load_font("C:/Windows/Fonts/segoeui.ttf"))
                    .as_ref()
            }),
    }
}

fn load_font(path: &str) -> Option<FontArc> {
    let bytes = fs::read(path).ok()?;
    FontArc::try_from_vec(bytes).ok()
}

fn softbuffer_color_to_rgba(color: u32) -> [u8; 4] {
    [
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
        0xff,
    ]
}

fn blend_pixel(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    x: i32,
    y: i32,
    rgba: [u8; 4],
    alpha: f32,
) {
    let offset = y as usize * width as usize + x as usize;
    let existing = buffer[offset];
    let inv_alpha = 1.0 - alpha;
    let old_r = ((existing >> 16) & 0xff) as f32;
    let old_g = ((existing >> 8) & 0xff) as f32;
    let old_b = (existing & 0xff) as f32;
    let r = old_r * inv_alpha + f32::from(rgba[0]) * alpha;
    let g = old_g * inv_alpha + f32::from(rgba[1]) * alpha;
    let b = old_b * inv_alpha + f32::from(rgba[2]) * alpha;
    buffer[offset] = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
}

fn draw_text_5x7(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    x: u32,
    y: u32,
    text: &str,
    color: u32,
    scale: u32,
) {
    let mut cursor_x = x;
    for character in text.chars() {
        if character == ' ' {
            cursor_x = cursor_x.saturating_add(4 * scale);
            continue;
        }
        draw_char_5x7(buffer, width, cursor_x, y, character, color, scale);
        cursor_x = cursor_x.saturating_add(6 * scale);
    }
}

fn draw_char_5x7(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    x: u32,
    y: u32,
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
            draw_rect(
                buffer,
                width,
                x + col * scale,
                y + row as u32 * scale,
                scale,
                scale,
                color,
            );
        }
    }
}

fn font_5x7(character: char) -> Option<[u8; 7]> {
    match character.to_ascii_uppercase() {
        'A' => Some([
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ]),
        'C' => Some([
            0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111,
        ]),
        'D' => Some([
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
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
        'T' => Some([
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ]),
        'X' => Some([
            0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b01010, 0b10001,
        ]),
        _ => None,
    }
}

fn canvas_layout(
    window_width: u32,
    window_height: u32,
    image_width: u32,
    image_height: u32,
) -> EditorCanvasLayout {
    let x = CONTENT_PADDING * 2 + TOOLBAR_WIDTH;
    let y = TITLE_BAR_HEIGHT as u32 + CONTENT_PADDING;
    let max_width = window_width.saturating_sub(x + CONTENT_PADDING);
    let max_height = window_height.saturating_sub(y + CONTENT_PADDING);
    EditorCanvasLayout {
        x,
        y,
        width: image_width.min(max_width),
        height: image_height.min(max_height),
        image_width,
        image_height,
    }
}

fn toolbar_height(window_height: u32) -> u32 {
    window_height.saturating_sub(TITLE_BAR_HEIGHT as u32 + CONTENT_PADDING * 2)
}

fn draw_image(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    surface_width: u32,
    surface_height: u32,
    image: &EditorSessionImage,
    layout: EditorCanvasLayout,
) {
    let width_usize = surface_width as usize;
    for row in 0..layout.height.min(surface_height.saturating_sub(layout.y)) {
        for col in 0..layout.width.min(surface_width.saturating_sub(layout.x)) {
            let image_offset = (row as usize * image.width as usize + col as usize) * 4;
            if image_offset + 3 >= image.rgba.len() {
                continue;
            }
            let rgba = [
                image.rgba[image_offset],
                image.rgba[image_offset + 1],
                image.rgba[image_offset + 2],
                image.rgba[image_offset + 3],
            ];
            let dest = (layout.y + row) as usize * width_usize + (layout.x + col) as usize;
            buffer[dest] = rgba_to_softbuffer_color(rgba);
        }
    }
}

fn draw_annotations(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    layout: EditorCanvasLayout,
    annotations: &[EditorAnnotation],
) {
    for annotation in annotations {
        match annotation {
            EditorAnnotation::Pen { points, color } => {
                draw_pen_points(buffer, width, height, layout, points, *color);
            }
            EditorAnnotation::Line { start, end, color } => {
                draw_editor_line(buffer, width, height, layout, *start, *end, *color);
            }
            EditorAnnotation::Arrow { start, end, color } => {
                draw_editor_arrow(buffer, width, height, layout, *start, *end, *color);
            }
            EditorAnnotation::Rectangle { rect, color } => {
                draw_editor_rect_outline(buffer, width, height, layout, *rect, *color);
            }
            EditorAnnotation::Redact { rect } => {
                draw_editor_redact(buffer, width, height, layout, *rect);
            }
            EditorAnnotation::Step {
                center,
                number,
                color,
            } => {
                draw_editor_step(buffer, width, height, layout, *center, *number, *color);
            }
        }
    }
}

fn draw_editor_line(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    layout: EditorCanvasLayout,
    start: EditorPoint,
    end: EditorPoint,
    color: EditorColor,
) {
    draw_line(
        buffer,
        width,
        height,
        translate_point(start, layout),
        translate_point(end, layout),
        rgba_to_softbuffer_color(color.rgba()),
    );
}

fn draw_editor_arrow(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    layout: EditorCanvasLayout,
    start: EditorPoint,
    end: EditorPoint,
    color: EditorColor,
) {
    draw_editor_line(buffer, width, height, layout, start, end, color);
    let angle = ((start.y - end.y) as f64).atan2((start.x - end.x) as f64);
    let head_len = 14.0;
    for offset in [0.55_f64, -0.55_f64] {
        let head = EditorPoint::new(
            end.x + (head_len * (angle + offset).cos()).round() as i32,
            end.y + (head_len * (angle + offset).sin()).round() as i32,
        );
        draw_editor_line(buffer, width, height, layout, end, head, color);
    }
}

fn draw_editor_step(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    layout: EditorCanvasLayout,
    center: EditorPoint,
    number: u32,
    color: EditorColor,
) {
    let center = translate_point(center, layout);
    fill_circle(
        buffer,
        width,
        height,
        center,
        13,
        rgba_to_softbuffer_color(color.rgba()),
    );
    draw_circle_outline(buffer, width, height, center, 13, 0x00ffffff);
    draw_number(buffer, width, height, center, number, 0x00ffffff);
}

fn draw_editor_rect_outline(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    layout: EditorCanvasLayout,
    rect: crate::editor::EditorRect,
    color: EditorColor,
) {
    let x = layout.x.saturating_add(rect.x.max(0) as u32);
    let y = layout.y.saturating_add(rect.y.max(0) as u32);
    draw_thick_rect_outline(
        buffer,
        width,
        height,
        x,
        y,
        rect.width,
        rect.height,
        rgba_to_softbuffer_color(color.rgba()),
        STROKE_THICKNESS,
    );
}

fn draw_editor_redact(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    layout: EditorCanvasLayout,
    rect: crate::editor::EditorRect,
) {
    let x = layout.x.saturating_add(rect.x.max(0) as u32);
    let y = layout.y.saturating_add(rect.y.max(0) as u32);
    let rect_width = rect.width.min(width.saturating_sub(x));
    let rect_height = rect.height.min(height.saturating_sub(y));
    draw_rect(buffer, width, x, y, rect_width, rect_height, 0x00000000);
}

fn draw_pen_points(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    layout: EditorCanvasLayout,
    points: &[EditorPoint],
    color: EditorColor,
) {
    for segment in points.windows(2) {
        let start = segment[0];
        let end = segment[1];
        draw_line(
            buffer,
            width,
            height,
            translate_point(start, layout),
            translate_point(end, layout),
            rgba_to_softbuffer_color(color.rgba()),
        );
    }
}

fn translate_point(point: EditorPoint, layout: EditorCanvasLayout) -> EditorPoint {
    EditorPoint::new(
        point.x.saturating_add(layout.x as i32),
        point.y.saturating_add(layout.y as i32),
    )
}

fn rgba_to_softbuffer_color(rgba: [u8; 4]) -> u32 {
    (u32::from(rgba[0]) << 16) | (u32::from(rgba[1]) << 8) | u32::from(rgba[2])
}

fn compose_editor_image(
    image: &EditorSessionImage,
    annotations: &[EditorAnnotation],
    current_stroke: Option<&[EditorPoint]>,
    current_drag: Option<(EditorTool, EditorPoint, EditorPoint)>,
    active_color: EditorColor,
    shift_pressed: bool,
) -> Result<CapturedImage> {
    let mut rgba = image.rgba.clone();
    for annotation in annotations {
        match annotation {
            EditorAnnotation::Pen { points, color } => {
                draw_pen_points_rgba(&mut rgba, image.width, image.height, points, color.rgba());
            }
            EditorAnnotation::Line { start, end, color } => {
                draw_line_rgba(
                    &mut rgba,
                    image.width,
                    image.height,
                    *start,
                    *end,
                    color.rgba(),
                );
            }
            EditorAnnotation::Arrow { start, end, color } => {
                draw_arrow_rgba(
                    &mut rgba,
                    image.width,
                    image.height,
                    *start,
                    *end,
                    color.rgba(),
                );
            }
            EditorAnnotation::Rectangle { rect, color } => {
                draw_rect_outline_rgba(&mut rgba, image.width, image.height, *rect, color.rgba());
            }
            EditorAnnotation::Redact { rect } => {
                fill_rect_rgba(&mut rgba, image.width, image.height, *rect, [0, 0, 0, 255]);
            }
            EditorAnnotation::Step {
                center,
                number,
                color,
            } => {
                draw_step_rgba(
                    &mut rgba,
                    image.width,
                    image.height,
                    *center,
                    *number,
                    color.rgba(),
                );
            }
        }
    }
    if let Some(points) = current_stroke {
        draw_pen_points_rgba(
            &mut rgba,
            image.width,
            image.height,
            points,
            active_color.rgba(),
        );
    }
    if let Some((tool, start, end)) = current_drag {
        let end = constrain_point_if_shift(start, end, shift_pressed);
        match tool {
            EditorTool::Line => draw_line_rgba(
                &mut rgba,
                image.width,
                image.height,
                start,
                end,
                active_color.rgba(),
            ),
            EditorTool::Arrow => draw_arrow_rgba(
                &mut rgba,
                image.width,
                image.height,
                start,
                end,
                active_color.rgba(),
            ),
            EditorTool::Rectangle => draw_rect_outline_rgba(
                &mut rgba,
                image.width,
                image.height,
                crate::editor::EditorRect::from_points(start, end),
                active_color.rgba(),
            ),
            EditorTool::Redact => fill_rect_rgba(
                &mut rgba,
                image.width,
                image.height,
                crate::editor::EditorRect::from_points(start, end),
                [0, 0, 0, 255],
            ),
            _ => {}
        }
    }
    let mut bgra = Vec::with_capacity(rgba.len());
    for pixel in rgba.chunks_exact(4) {
        bgra.push(pixel[2]);
        bgra.push(pixel[1]);
        bgra.push(pixel[0]);
        bgra.push(pixel[3]);
    }
    CapturedImage::new(0, 0, image.width, image.height, bgra)
}

fn draw_pen_points_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    points: &[EditorPoint],
    color: [u8; 4],
) {
    for segment in points.windows(2) {
        draw_line_rgba(rgba, width, height, segment[0], segment[1], color);
    }
}

fn draw_step_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    center: EditorPoint,
    number: u32,
    color: [u8; 4],
) {
    fill_circle_rgba(rgba, width, height, center, 13, color);
    draw_number_rgba(rgba, width, height, center, number, [255, 255, 255, 255]);
}

fn fill_circle_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    center: EditorPoint,
    radius: i32,
    color: [u8; 4],
) {
    for y in center.y - radius..=center.y + radius {
        for x in center.x - radius..=center.x + radius {
            let dx = x - center.x;
            let dy = y - center.y;
            if dx * dx + dy * dy <= radius * radius {
                put_pixel_rgba(rgba, width, height, x, y, color);
            }
        }
    }
}

fn draw_number_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    center: EditorPoint,
    number: u32,
    color: [u8; 4],
) {
    let text = number.to_string();
    let digit_width = 5;
    let gap = 1;
    let total_width = text.len() as i32 * digit_width + (text.len().saturating_sub(1)) as i32 * gap;
    let start_x = center.x - total_width / 2;
    for (index, digit) in text.chars().enumerate() {
        if let Some(value) = digit.to_digit(10) {
            draw_digit_rgba(
                rgba,
                width,
                height,
                start_x + index as i32 * (digit_width + gap),
                center.y - 5,
                value as u8,
                color,
            );
        }
    }
}

fn draw_digit_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    digit: u8,
    color: [u8; 4],
) {
    const SEGMENTS: [u8; 10] = [
        0b1111110, 0b0110000, 0b1101101, 0b1111001, 0b0110011, 0b1011011, 0b1011111, 0b1110000,
        0b1111111, 0b1111011,
    ];
    let Some(mask) = SEGMENTS.get(digit as usize).copied() else {
        return;
    };
    let segments = [
        (0b1000000, (0, 0), (4, 0)),
        (0b0100000, (4, 0), (4, 5)),
        (0b0010000, (4, 5), (4, 10)),
        (0b0001000, (0, 10), (4, 10)),
        (0b0000100, (0, 5), (0, 10)),
        (0b0000010, (0, 0), (0, 5)),
        (0b0000001, (0, 5), (4, 5)),
    ];
    for (bit, start, end) in segments {
        if mask & bit != 0 {
            draw_line_rgba(
                rgba,
                width,
                height,
                EditorPoint::new(x + start.0, y + start.1),
                EditorPoint::new(x + end.0, y + end.1),
                color,
            );
        }
    }
}

fn draw_rect_outline_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    rect: crate::editor::EditorRect,
    color: [u8; 4],
) {
    let x = rect.x.max(0);
    let y = rect.y.max(0);
    let right = x.saturating_add(rect.width as i32).min(width as i32);
    let bottom = y.saturating_add(rect.height as i32).min(height as i32);
    if x >= right || y >= bottom {
        return;
    }
    draw_line_rgba(
        rgba,
        width,
        height,
        EditorPoint::new(x, y),
        EditorPoint::new(right - 1, y),
        color,
    );
    draw_line_rgba(
        rgba,
        width,
        height,
        EditorPoint::new(x, bottom - 1),
        EditorPoint::new(right - 1, bottom - 1),
        color,
    );
    draw_line_rgba(
        rgba,
        width,
        height,
        EditorPoint::new(x, y),
        EditorPoint::new(x, bottom - 1),
        color,
    );
    draw_line_rgba(
        rgba,
        width,
        height,
        EditorPoint::new(right - 1, y),
        EditorPoint::new(right - 1, bottom - 1),
        color,
    );
}

fn fill_rect_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    rect: crate::editor::EditorRect,
    color: [u8; 4],
) {
    let x = rect.x.max(0) as u32;
    let y = rect.y.max(0) as u32;
    let rect_width = rect.width.min(width.saturating_sub(x));
    let rect_height = rect.height.min(height.saturating_sub(y));
    for row in y..y.saturating_add(rect_height) {
        for col in x..x.saturating_add(rect_width) {
            put_pixel_rgba(rgba, width, height, col as i32, row as i32, color);
        }
    }
}

fn draw_arrow_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    start: EditorPoint,
    end: EditorPoint,
    color: [u8; 4],
) {
    draw_line_rgba(rgba, width, height, start, end, color);
    let angle = ((start.y - end.y) as f64).atan2((start.x - end.x) as f64);
    let head_len = 14.0;
    for offset in [0.55_f64, -0.55_f64] {
        let head = EditorPoint::new(
            end.x + (head_len * (angle + offset).cos()).round() as i32,
            end.y + (head_len * (angle + offset).sin()).round() as i32,
        );
        draw_line_rgba(rgba, width, height, end, head, color);
    }
}

fn constrain_point_if_shift(
    start: EditorPoint,
    end: EditorPoint,
    shift_pressed: bool,
) -> EditorPoint {
    if !shift_pressed {
        return end;
    }
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    if dx == 0 && dy == 0 {
        return end;
    }
    let distance = dx.abs().max(dy.abs());
    if dx.abs().saturating_mul(2) < dy.abs() {
        return EditorPoint::new(start.x, start.y + dy.signum() * distance);
    }
    if dy.abs().saturating_mul(2) < dx.abs() {
        return EditorPoint::new(start.x + dx.signum() * distance, start.y);
    }
    EditorPoint::new(
        start.x + dx.signum() * distance,
        start.y + dy.signum() * distance,
    )
}

fn draw_line(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    start: EditorPoint,
    end: EditorPoint,
    color: u32,
) {
    let mut x0 = start.x;
    let mut y0 = start.y;
    let x1 = end.x;
    let y1 = end.y;
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        put_brush(buffer, width, height, x0, y0, color, STROKE_THICKNESS);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x0 += sx;
        }
        if doubled <= dx {
            error += dx;
            y0 += sy;
        }
    }
}

fn put_brush(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    color: u32,
    thickness: i32,
) {
    let radius = (thickness / 2).max(0);
    for offset_y in -radius..=radius {
        for offset_x in -radius..=radius {
            put_pixel(buffer, width, height, x + offset_x, y + offset_y, color);
        }
    }
}

fn put_pixel(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    color: u32,
) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    buffer[y as usize * width as usize + x as usize] = color;
}

fn draw_line_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    start: EditorPoint,
    end: EditorPoint,
    color: [u8; 4],
) {
    let mut x0 = start.x;
    let mut y0 = start.y;
    let x1 = end.x;
    let y1 = end.y;
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        put_brush_rgba(rgba, width, height, x0, y0, color, STROKE_THICKNESS);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x0 += sx;
        }
        if doubled <= dx {
            error += dx;
            y0 += sy;
        }
    }
}

fn put_brush_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    color: [u8; 4],
    thickness: i32,
) {
    let radius = (thickness / 2).max(0);
    for offset_y in -radius..=radius {
        for offset_x in -radius..=radius {
            put_pixel_rgba(rgba, width, height, x + offset_x, y + offset_y, color);
        }
    }
}

fn put_pixel_rgba(rgba: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let offset = (y as usize * width as usize + x as usize) * 4;
    if offset + 3 >= rgba.len() {
        return;
    }
    rgba[offset..offset + 4].copy_from_slice(&color);
}

fn fill_circle(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    center: EditorPoint,
    radius: i32,
    color: u32,
) {
    for y in center.y - radius..=center.y + radius {
        for x in center.x - radius..=center.x + radius {
            let dx = x - center.x;
            let dy = y - center.y;
            if dx * dx + dy * dy <= radius * radius {
                put_pixel(buffer, width, height, x, y, color);
            }
        }
    }
}

fn draw_circle_outline(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    center: EditorPoint,
    radius: i32,
    color: u32,
) {
    for y in center.y - radius..=center.y + radius {
        for x in center.x - radius..=center.x + radius {
            let dx = x - center.x;
            let dy = y - center.y;
            let distance = dx * dx + dy * dy;
            if distance >= (radius - 1) * (radius - 1) && distance <= radius * radius {
                put_pixel(buffer, width, height, x, y, color);
            }
        }
    }
}

fn draw_number(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    center: EditorPoint,
    number: u32,
    color: u32,
) {
    let text = number.to_string();
    let digit_width = 5;
    let gap = 1;
    let total_width = text.len() as i32 * digit_width + (text.len().saturating_sub(1)) as i32 * gap;
    let start_x = center.x - total_width / 2;
    for (index, digit) in text.chars().enumerate() {
        if let Some(value) = digit.to_digit(10) {
            draw_digit(
                buffer,
                width,
                height,
                start_x + index as i32 * (digit_width + gap),
                center.y - 5,
                value as u8,
                color,
            );
        }
    }
}

fn draw_digit(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    digit: u8,
    color: u32,
) {
    const SEGMENTS: [u8; 10] = [
        0b1111110, 0b0110000, 0b1101101, 0b1111001, 0b0110011, 0b1011011, 0b1011111, 0b1110000,
        0b1111111, 0b1111011,
    ];
    let Some(mask) = SEGMENTS.get(digit as usize).copied() else {
        return;
    };
    let segments = [
        (0b1000000, (0, 0), (4, 0)),
        (0b0100000, (4, 0), (4, 5)),
        (0b0010000, (4, 5), (4, 10)),
        (0b0001000, (0, 10), (4, 10)),
        (0b0000100, (0, 5), (0, 10)),
        (0b0000010, (0, 0), (0, 5)),
        (0b0000001, (0, 5), (4, 5)),
    ];
    for (bit, start, end) in segments {
        if mask & bit != 0 {
            draw_line(
                buffer,
                width,
                height,
                EditorPoint::new(x + start.0, y + start.1),
                EditorPoint::new(x + end.0, y + end.1),
                color,
            );
        }
    }
}

fn draw_rect(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    width: u32,
    x: u32,
    y: u32,
    rect_width: u32,
    rect_height: u32,
    color: u32,
) {
    let width_usize = width as usize;
    for row in y..y.saturating_add(rect_height) {
        for col in x..x.saturating_add(rect_width) {
            buffer[row as usize * width_usize + col as usize] = color;
        }
    }
}

fn draw_rect_outline(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    surface_width: u32,
    surface_height: u32,
    x: u32,
    y: u32,
    rect_width: u32,
    rect_height: u32,
    color: u32,
) {
    draw_thick_rect_outline(
        buffer,
        surface_width,
        surface_height,
        x,
        y,
        rect_width,
        rect_height,
        color,
        1,
    );
}

fn draw_thick_rect_outline(
    buffer: &mut softbuffer::Buffer<'_, OwnedDisplayHandle, Rc<Window>>,
    surface_width: u32,
    surface_height: u32,
    x: u32,
    y: u32,
    rect_width: u32,
    rect_height: u32,
    color: u32,
    thickness: i32,
) {
    if rect_width == 0 || rect_height == 0 || x >= surface_width || y >= surface_height {
        return;
    }
    let right = x.saturating_add(rect_width).min(surface_width);
    let bottom = y.saturating_add(rect_height).min(surface_height);
    for col in x..right {
        put_brush(
            buffer,
            surface_width,
            surface_height,
            col as i32,
            y as i32,
            color,
            thickness,
        );
        put_brush(
            buffer,
            surface_width,
            surface_height,
            col as i32,
            (bottom - 1) as i32,
            color,
            thickness,
        );
    }
    for row in y..bottom {
        put_brush(
            buffer,
            surface_width,
            surface_height,
            x as i32,
            row as i32,
            color,
            thickness,
        );
        put_brush(
            buffer,
            surface_width,
            surface_height,
            (right - 1) as i32,
            row as i32,
            color,
            thickness,
        );
    }
}

trait SaturatingSubF64 {
    fn saturating_sub_f64(self, rhs: f64) -> f64;
}

impl SaturatingSubF64 for f64 {
    fn saturating_sub_f64(self, rhs: f64) -> f64 {
        (self - rhs).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{EditorCanvasLayout, canvas_layout};
    use winit::dpi::PhysicalPosition;

    #[test]
    fn canvas_layout_maps_window_points_to_image_points() {
        let layout = EditorCanvasLayout {
            x: 72,
            y: 52,
            width: 320,
            height: 200,
            image_width: 320,
            image_height: 200,
        };

        assert_eq!(
            layout.image_point_from_window_position(PhysicalPosition::new(80.0, 60.0)),
            Some(crate::editor::EditorPoint::new(8, 8))
        );
        assert_eq!(
            layout.image_point_from_window_position(PhysicalPosition::new(10.0, 10.0)),
            None
        );
    }

    #[test]
    fn canvas_layout_reserves_toolbar_and_padding() {
        let layout = canvas_layout(800, 600, 640, 480);
        assert_eq!(layout.x, 88);
        assert_eq!(layout.y, 52);
        assert_eq!(layout.width, 640);
        assert_eq!(layout.height, 480);
    }

    #[test]
    fn compose_editor_image_includes_pen_annotation() {
        let image = crate::editor::EditorSessionImage {
            width: 3,
            height: 1,
            rgba: vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255],
        };
        let annotations = [crate::editor::EditorAnnotation::Pen {
            points: vec![
                crate::editor::EditorPoint::new(0, 0),
                crate::editor::EditorPoint::new(2, 0),
            ],
            color: crate::editor::EditorColor::Red,
        }];

        let composed = super::compose_editor_image(
            &image,
            &annotations,
            None,
            None,
            crate::editor::EditorColor::Red,
            false,
        )
        .unwrap();

        assert_eq!(
            composed.bgra,
            vec![
                0x33, 0x33, 0xff, 0xff, 0x33, 0x33, 0xff, 0xff, 0x33, 0x33, 0xff, 0xff
            ]
        );
    }
}
