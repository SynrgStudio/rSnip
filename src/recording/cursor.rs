use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::errors::{Result, RsnipError};
use crate::screen::capture::CaptureRegion;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorScreenPosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorFramePosition {
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorVisibility {
    Inside(CursorFramePosition),
    Outside,
}

const CURSOR_PATTERN: &[&str] = &[
    "X...........",
    "XX..........",
    "XWX.........",
    "XWWX........",
    "XWWWX.......",
    "XWWWWX......",
    "XWWWWWX.....",
    "XWWWWWWX....",
    "XWWWWWWWX...",
    "XWWWWWWWWX..",
    "XWWWWXXXXX..",
    "XWWXWWX.....",
    "XWX.XWWX....",
    "XX..XWWX....",
    "X....XWWX...",
    ".....XWWX...",
    "......XX....",
];

impl CursorScreenPosition {
    pub fn resolve_in_region(self, region: CaptureRegion) -> CursorVisibility {
        if self.x < region.x
            || self.y < region.y
            || self.x >= region.right()
            || self.y >= region.bottom()
        {
            return CursorVisibility::Outside;
        }

        CursorVisibility::Inside(CursorFramePosition {
            x: u32::try_from(self.x - region.x).expect("cursor local x is non-negative"),
            y: u32::try_from(self.y - region.y).expect("cursor local y is non-negative"),
        })
    }
}

pub fn current_cursor_position() -> Result<CursorScreenPosition> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }
        .map_err(|error| RsnipError::Message(format!("failed to read cursor position: {error}")))?;
    Ok(CursorScreenPosition {
        x: point.x,
        y: point.y,
    })
}

pub fn draw_cursor_on_bgra_frame(
    frame: &mut [u8],
    frame_width: u32,
    frame_height: u32,
    position: CursorFramePosition,
) -> Result<()> {
    let expected_len = usize::try_from(frame_width)
        .ok()
        .and_then(|width| {
            usize::try_from(frame_height)
                .ok()?
                .checked_mul(width)?
                .checked_mul(4)
        })
        .ok_or_else(|| RsnipError::Message("cursor frame dimensions overflow".to_owned()))?;
    if frame.len() != expected_len {
        return Err(RsnipError::Message(format!(
            "invalid frame length for cursor drawing: got {}, expected {expected_len}",
            frame.len()
        )));
    }

    for (pattern_y, row) in CURSOR_PATTERN.iter().enumerate() {
        for (pattern_x, pixel) in row.bytes().enumerate() {
            let color = match pixel {
                b'X' => Some([0, 0, 0, 255]),
                b'W' => Some([255, 255, 255, 255]),
                _ => None,
            };
            let Some([b, g, r, a]) = color else {
                continue;
            };

            let Some(x) = position.x.checked_add(pattern_x as u32) else {
                continue;
            };
            let Some(y) = position.y.checked_add(pattern_y as u32) else {
                continue;
            };
            if x >= frame_width || y >= frame_height {
                continue;
            }

            let offset = usize::try_from(y)
                .ok()
                .and_then(|row| row.checked_mul(usize::try_from(frame_width).ok()?))
                .and_then(|base| base.checked_add(usize::try_from(x).ok()?))
                .and_then(|pixel| pixel.checked_mul(4))
                .ok_or_else(|| RsnipError::Message("cursor pixel offset overflow".to_owned()))?;
            frame[offset] = b;
            frame[offset + 1] = g;
            frame[offset + 2] = r;
            frame[offset + 3] = a;
        }
    }

    Ok(())
}

pub fn draw_current_cursor_on_bgra_frame(
    frame: &mut [u8],
    frame_width: u32,
    frame_height: u32,
    region: CaptureRegion,
) -> Result<CursorVisibility> {
    let position = current_cursor_position()?;
    let visibility = position.resolve_in_region(region);
    if let CursorVisibility::Inside(frame_position) = visibility {
        draw_cursor_on_bgra_frame(frame, frame_width, frame_height, frame_position)?;
    }
    Ok(visibility)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_cursor_inside_region_to_local_coordinates() {
        let region = CaptureRegion::new(100, 200, 300, 400).expect("valid region");
        let position = CursorScreenPosition { x: 125, y: 240 };
        assert_eq!(
            position.resolve_in_region(region),
            CursorVisibility::Inside(CursorFramePosition { x: 25, y: 40 })
        );
    }

    #[test]
    fn resolves_negative_region_coordinates() {
        let region = CaptureRegion::new(-500, -100, 300, 200).expect("valid region");
        let position = CursorScreenPosition { x: -450, y: -50 };
        assert_eq!(
            position.resolve_in_region(region),
            CursorVisibility::Inside(CursorFramePosition { x: 50, y: 50 })
        );
    }

    #[test]
    fn right_and_bottom_edges_are_outside() {
        let region = CaptureRegion::new(10, 20, 30, 40).expect("valid region");
        assert_eq!(
            CursorScreenPosition { x: 10, y: 20 }.resolve_in_region(region),
            CursorVisibility::Inside(CursorFramePosition { x: 0, y: 0 })
        );
        assert_eq!(
            CursorScreenPosition { x: 39, y: 59 }.resolve_in_region(region),
            CursorVisibility::Inside(CursorFramePosition { x: 29, y: 39 })
        );
        assert_eq!(
            CursorScreenPosition { x: 40, y: 59 }.resolve_in_region(region),
            CursorVisibility::Outside
        );
        assert_eq!(
            CursorScreenPosition { x: 39, y: 60 }.resolve_in_region(region),
            CursorVisibility::Outside
        );
    }

    #[test]
    fn cursor_draw_clips_at_frame_edges() {
        let mut frame = vec![128; 4 * 4 * 4];
        draw_cursor_on_bgra_frame(&mut frame, 4, 4, CursorFramePosition { x: 2, y: 2 })
            .expect("cursor draws");
        assert!(frame.chunks_exact(4).any(|pixel| pixel == [0, 0, 0, 255]));
        assert_eq!(frame.len(), 4 * 4 * 4);
    }

    #[test]
    fn cursor_draw_rejects_invalid_buffer_length() {
        let mut frame = vec![0; 3];
        assert!(
            draw_cursor_on_bgra_frame(&mut frame, 10, 10, CursorFramePosition { x: 0, y: 0 })
                .is_err()
        );
    }
}
