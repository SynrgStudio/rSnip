pub mod history;
pub mod render;
pub mod tools;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use image::{ImageBuffer, Rgba, RgbaImage};

use crate::errors::{Result, RsnipError};
use crate::screen::capture::CapturedImage;

use history::EditHistory;
pub use tools::{EditorColor, EditorTool};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorImageSize {
    pub width: u32,
    pub height: u32,
}

impl EditorImageSize {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorPoint {
    pub x: i32,
    pub y: i32,
}

impl EditorPoint {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl EditorRect {
    pub fn from_points(start: EditorPoint, end: EditorPoint) -> Self {
        let left = start.x.min(end.x);
        let top = start.y.min(end.y);
        let right = start.x.max(end.x);
        let bottom = start.y.max(end.y);
        Self {
            x: left,
            y: top,
            width: (right - left) as u32,
            height: (bottom - top) as u32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorAnnotation {
    Pen {
        points: Vec<EditorPoint>,
        color: EditorColor,
    },
    Arrow {
        start: EditorPoint,
        end: EditorPoint,
        color: EditorColor,
    },
    Line {
        start: EditorPoint,
        end: EditorPoint,
        color: EditorColor,
    },
    Rectangle {
        rect: EditorRect,
        color: EditorColor,
    },
    Redact {
        rect: EditorRect,
    },
    Step {
        center: EditorPoint,
        number: u32,
        color: EditorColor,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorDocument {
    image_size: EditorImageSize,
    annotations: Vec<EditorAnnotation>,
    active_tool: EditorTool,
    active_color: EditorColor,
    next_step_number: u32,
    history: EditHistory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorSessionImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl EditorSessionImage {
    pub fn image_size(&self) -> EditorImageSize {
        EditorImageSize::new(self.width, self.height)
    }

    pub fn to_captured_image(&self) -> Result<CapturedImage> {
        if !self.rgba.len().is_multiple_of(4) {
            return Err(RsnipError::Message(format!(
                "RGBA buffer length must be divisible by 4, got {}",
                self.rgba.len()
            )));
        }
        let mut bgra = Vec::with_capacity(self.rgba.len());
        for pixel in self.rgba.chunks_exact(4) {
            bgra.push(pixel[2]);
            bgra.push(pixel[1]);
            bgra.push(pixel[0]);
            bgra.push(pixel[3]);
        }
        CapturedImage::new(0, 0, self.width, self.height, bgra)
    }
}

impl EditorDocument {
    pub fn new(image_size: EditorImageSize) -> Self {
        Self {
            image_size,
            annotations: Vec::new(),
            active_tool: EditorTool::default(),
            active_color: EditorColor::default(),
            next_step_number: 1,
            history: EditHistory::default(),
        }
    }

    pub fn image_size(&self) -> EditorImageSize {
        self.image_size
    }

    pub fn annotations(&self) -> &[EditorAnnotation] {
        &self.annotations
    }

    pub fn active_tool(&self) -> EditorTool {
        self.active_tool
    }

    pub fn set_active_tool(&mut self, tool: EditorTool) {
        self.active_tool = tool;
    }

    pub fn active_color(&self) -> EditorColor {
        self.active_color
    }

    pub fn set_active_color(&mut self, color: EditorColor) {
        self.active_color = color;
    }

    pub fn next_step_number(&self) -> u32 {
        self.next_step_number
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn push_annotation(&mut self, annotation: EditorAnnotation) {
        self.history.push(&self.annotations, self.next_step_number);
        if let EditorAnnotation::Step { number, .. } = annotation {
            self.next_step_number = self.next_step_number.max(number.saturating_add(1));
        }
        self.annotations.push(annotation);
    }

    pub fn push_next_step(&mut self, center: EditorPoint) {
        let number = self.next_step_number;
        self.push_annotation(EditorAnnotation::Step {
            center,
            number,
            color: self.active_color,
        });
    }

    pub fn undo(&mut self) -> bool {
        if let Some((annotations, next_step_number)) = self.history.undo() {
            self.annotations = annotations;
            self.next_step_number = next_step_number;
            return true;
        }
        false
    }
}

pub fn save_editor_input_png(image: &CapturedImage) -> Result<PathBuf> {
    let path = next_editor_temp_path()?;
    crate::paths::ensure_parent_dir(&path)?;
    let rgba = bgra_to_rgba(&image.bgra)?;
    let buffer: RgbaImage =
        ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(image.width, image.height, rgba).ok_or_else(
            || {
                RsnipError::Message(format!(
                    "failed to build editor PNG buffer for {}x{} image",
                    image.width, image.height
                ))
            },
        )?;
    buffer
        .save(&path)
        .map_err(|error| RsnipError::Message(format!("failed to save editor PNG: {error}")))?;
    Ok(path)
}

pub fn load_editor_input_png(path: &Path) -> Result<EditorSessionImage> {
    let image = image::open(path)
        .map_err(|error| RsnipError::Message(format!("failed to load editor PNG: {error}")))?
        .into_rgba8();
    Ok(EditorSessionImage {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    })
}

fn next_editor_temp_path() -> Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| RsnipError::Message(format!("system time before UNIX_EPOCH: {error}")))?
        .as_nanos();
    Ok(crate::paths::temp_dir().join(format!(
        "rsnip-editor-{}-{timestamp}.png",
        std::process::id()
    )))
}

fn bgra_to_rgba(bgra: &[u8]) -> Result<Vec<u8>> {
    if !bgra.len().is_multiple_of(4) {
        return Err(RsnipError::Message(format!(
            "BGRA buffer length must be divisible by 4, got {}",
            bgra.len()
        )));
    }

    let mut rgba = Vec::with_capacity(bgra.len());
    for pixel in bgra.chunks_exact(4) {
        rgba.push(pixel[2]);
        rgba.push(pixel[1]);
        rgba.push(pixel[0]);
        rgba.push(pixel[3]);
    }
    Ok(rgba)
}

#[cfg(test)]
mod tests {
    use super::{
        EditorAnnotation, EditorColor, EditorDocument, EditorImageSize, EditorPoint, EditorRect,
        EditorTool,
    };

    #[test]
    fn document_defaults_to_pen_and_red() {
        let document = EditorDocument::new(EditorImageSize::new(320, 240));
        assert_eq!(document.image_size(), EditorImageSize::new(320, 240));
        assert_eq!(document.active_tool(), EditorTool::Pen);
        assert_eq!(document.active_color(), EditorColor::Red);
        assert_eq!(document.next_step_number(), 1);
        assert!(document.annotations().is_empty());
    }

    #[test]
    fn rect_normalizes_points() {
        let rect = EditorRect::from_points(EditorPoint::new(10, 30), EditorPoint::new(-5, 12));
        assert_eq!(rect.x, -5);
        assert_eq!(rect.y, 12);
        assert_eq!(rect.width, 15);
        assert_eq!(rect.height, 18);
    }

    #[test]
    fn undo_restores_annotations() {
        let mut document = EditorDocument::new(EditorImageSize::new(100, 100));
        document.push_annotation(EditorAnnotation::Line {
            start: EditorPoint::new(0, 0),
            end: EditorPoint::new(10, 10),
            color: EditorColor::Blue,
        });
        assert_eq!(document.annotations().len(), 1);
        assert!(document.can_undo());

        assert!(document.undo());
        assert!(document.annotations().is_empty());
        assert!(!document.can_undo());
        assert!(!document.undo());
    }

    #[test]
    fn undo_restores_step_counter() {
        let mut document = EditorDocument::new(EditorImageSize::new(100, 100));
        document.push_next_step(EditorPoint::new(10, 10));
        document.push_next_step(EditorPoint::new(20, 20));

        assert_eq!(document.next_step_number(), 3);
        assert_eq!(document.annotations().len(), 2);

        assert!(document.undo());
        assert_eq!(document.next_step_number(), 2);
        assert_eq!(document.annotations().len(), 1);

        assert!(document.undo());
        assert_eq!(document.next_step_number(), 1);
        assert!(document.annotations().is_empty());
    }
}
