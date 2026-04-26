use std::time::{Duration, Instant};

use tracing::info;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
    SRCCOPY, SelectObject,
};

use crate::errors::{Result, RsnipError};
use crate::screen::monitor::{ScreenRect, VirtualScreen};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedImage {
    pub origin_x: i32,
    pub origin_y: i32,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureMetrics {
    pub elapsed: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenCapture {
    pub image: CapturedImage,
    pub metrics: CaptureMetrics,
}

struct WindowDc {
    hwnd: HWND,
    hdc: HDC,
}

struct MemoryDc(HDC);
struct Bitmap(HBITMAP);
struct SelectedObject {
    hdc: HDC,
    previous: HGDIOBJ,
}

impl CaptureRegion {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(RsnipError::Message(format!(
                "capture region must be non-empty, got {width}x{height} at {x},{y}"
            )));
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    pub fn rect(self) -> ScreenRect {
        ScreenRect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }

    pub fn right(self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }
}

impl CapturedImage {
    pub fn new(
        origin_x: i32,
        origin_y: i32,
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    ) -> Result<Self> {
        validate_dimensions(width, height)?;
        let expected_len = buffer_len(width, height)?;
        if bgra.len() != expected_len {
            return Err(RsnipError::Message(format!(
                "invalid BGRA buffer length: got {}, expected {expected_len} for {width}x{height}",
                bgra.len()
            )));
        }

        Ok(Self {
            origin_x,
            origin_y,
            width,
            height,
            bgra,
        })
    }

    pub fn full_region(&self) -> CaptureRegion {
        CaptureRegion {
            x: self.origin_x,
            y: self.origin_y,
            width: self.width,
            height: self.height,
        }
    }

    pub fn rect(&self) -> ScreenRect {
        self.full_region().rect()
    }

    pub fn offset_for_virtual_point(&self, x: i32, y: i32) -> Result<usize> {
        if x < self.origin_x || y < self.origin_y || x >= self.right() || y >= self.bottom() {
            return Err(RsnipError::Message(format!(
                "point {x},{y} is outside captured image {}x{} at {},{}",
                self.width, self.height, self.origin_x, self.origin_y
            )));
        }

        let local_x = u32::try_from(x - self.origin_x)
            .map_err(|_| RsnipError::Message("negative local x offset".to_owned()))?;
        let local_y = u32::try_from(y - self.origin_y)
            .map_err(|_| RsnipError::Message("negative local y offset".to_owned()))?;
        pixel_offset(self.width, local_x, local_y)
    }

    pub fn crop(&self, region: CaptureRegion) -> Result<Self> {
        if !self.rect().contains_rect(region.rect()) {
            return Err(RsnipError::Message(format!(
                "crop region {:?} is outside captured image {:?}",
                region,
                self.full_region()
            )));
        }

        let mut out = vec![0; buffer_len(region.width, region.height)?];
        let source_x = u32::try_from(region.x - self.origin_x)
            .map_err(|_| RsnipError::Message("negative crop x offset".to_owned()))?;
        let source_y = u32::try_from(region.y - self.origin_y)
            .map_err(|_| RsnipError::Message("negative crop y offset".to_owned()))?;
        let source_stride = row_len(self.width)?;
        let dest_stride = row_len(region.width)?;

        for row in 0..region.height {
            let source_offset = usize::try_from(source_y + row)
                .ok()
                .and_then(|y| y.checked_mul(source_stride))
                .and_then(|base| base.checked_add(usize::try_from(source_x).ok()?.checked_mul(4)?))
                .ok_or_else(|| RsnipError::Message("source crop offset overflow".to_owned()))?;
            let dest_offset = usize::try_from(row)
                .ok()
                .and_then(|y| y.checked_mul(dest_stride))
                .ok_or_else(|| {
                    RsnipError::Message("destination crop offset overflow".to_owned())
                })?;
            out[dest_offset..dest_offset + dest_stride]
                .copy_from_slice(&self.bgra[source_offset..source_offset + dest_stride]);
        }

        Self::new(region.x, region.y, region.width, region.height, out)
    }

    pub fn right(&self) -> i32 {
        self.origin_x.saturating_add(self.width as i32)
    }

    pub fn bottom(&self) -> i32 {
        self.origin_y.saturating_add(self.height as i32)
    }
}

pub fn capture_virtual_screen() -> Result<ScreenCapture> {
    let virtual_screen = VirtualScreen::current()?;
    capture_region(CaptureRegion::new(
        virtual_screen.x,
        virtual_screen.y,
        virtual_screen.width,
        virtual_screen.height,
    )?)
}

pub fn capture_region(region: CaptureRegion) -> Result<ScreenCapture> {
    let started = Instant::now();
    let image = capture_region_gdi(region)?;
    let elapsed = started.elapsed();
    info!(
        x = region.x,
        y = region.y,
        width = region.width,
        height = region.height,
        elapsed_ms = elapsed.as_millis(),
        "captured screen region"
    );
    Ok(ScreenCapture {
        image,
        metrics: CaptureMetrics { elapsed },
    })
}

fn capture_region_gdi(region: CaptureRegion) -> Result<CapturedImage> {
    let width_i32 = i32::try_from(region.width)
        .map_err(|_| RsnipError::Message(format!("capture width too large: {}", region.width)))?;
    let height_i32 = i32::try_from(region.height)
        .map_err(|_| RsnipError::Message(format!("capture height too large: {}", region.height)))?;

    let screen_dc = WindowDc::screen()?;
    let memory_dc = MemoryDc::compatible(screen_dc.hdc)?;
    let bitmap = Bitmap::compatible(screen_dc.hdc, width_i32, height_i32)?;
    let _selection = SelectedObject::select(memory_dc.0, bitmap.as_object())?;

    // SAFETY: DCs and bitmap are valid. Coordinates are virtual desktop coordinates.
    unsafe {
        BitBlt(
            memory_dc.0,
            0,
            0,
            width_i32,
            height_i32,
            screen_dc.hdc,
            region.x,
            region.y,
            SRCCOPY,
        )
    }
    .map_err(|error| RsnipError::Message(format!("BitBlt failed: {error}")))?;

    let mut bgra = vec![0u8; buffer_len(region.width, region.height)?];
    let mut bitmap_info = top_down_bgra_bitmap_info(width_i32, height_i32);
    // SAFETY: memory_dc and bitmap are valid; bgra has enough space for width*height*4 bytes.
    let scan_lines = unsafe {
        GetDIBits(
            memory_dc.0,
            bitmap.0,
            0,
            region.height,
            Some(bgra.as_mut_ptr().cast()),
            &mut bitmap_info,
            DIB_RGB_COLORS,
        )
    };

    if scan_lines == 0 || scan_lines != height_i32 {
        return Err(RsnipError::Message(format!(
            "GetDIBits failed or returned partial data: {scan_lines}/{height_i32}"
        )));
    }

    CapturedImage::new(region.x, region.y, region.width, region.height, bgra)
}

fn top_down_bgra_bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

impl WindowDc {
    fn screen() -> Result<Self> {
        let hwnd = HWND::default();
        // SAFETY: Passing a null HWND requests the full screen DC.
        let hdc = unsafe { GetDC(hwnd) };
        if hdc.is_invalid() {
            return Err(RsnipError::Message("GetDC failed".to_owned()));
        }
        Ok(Self { hwnd, hdc })
    }
}

impl Drop for WindowDc {
    fn drop(&mut self) {
        // SAFETY: The DC was acquired with GetDC for this HWND.
        let _ = unsafe { ReleaseDC(self.hwnd, self.hdc) };
    }
}

impl MemoryDc {
    fn compatible(source: HDC) -> Result<Self> {
        // SAFETY: source is a valid screen DC.
        let hdc = unsafe { CreateCompatibleDC(source) };
        if hdc.is_invalid() {
            return Err(RsnipError::Message("CreateCompatibleDC failed".to_owned()));
        }
        Ok(Self(hdc))
    }
}

impl Drop for MemoryDc {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: The memory DC is owned by this wrapper.
            let _ = unsafe { DeleteDC(self.0) };
        }
    }
}

impl Bitmap {
    fn compatible(source: HDC, width: i32, height: i32) -> Result<Self> {
        // SAFETY: source is a valid screen DC and dimensions are positive.
        let bitmap = unsafe { CreateCompatibleBitmap(source, width, height) };
        if bitmap.is_invalid() {
            return Err(RsnipError::Message(
                "CreateCompatibleBitmap failed".to_owned(),
            ));
        }
        Ok(Self(bitmap))
    }

    fn as_object(&self) -> HGDIOBJ {
        HGDIOBJ(self.0.0)
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: The bitmap is owned by this wrapper.
            let _ = unsafe { DeleteObject(self.0) };
        }
    }
}

impl SelectedObject {
    fn select(hdc: HDC, object: HGDIOBJ) -> Result<Self> {
        // SAFETY: hdc and object are valid GDI handles.
        let previous = unsafe { SelectObject(hdc, object) };
        if previous.is_invalid() {
            return Err(RsnipError::Message("SelectObject failed".to_owned()));
        }
        Ok(Self { hdc, previous })
    }
}

impl Drop for SelectedObject {
    fn drop(&mut self) {
        if !self.previous.is_invalid() {
            // SAFETY: Restores the previous object into the same DC.
            let _ = unsafe { SelectObject(self.hdc, self.previous) };
        }
    }
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(RsnipError::Message(format!(
            "image dimensions must be non-empty, got {width}x{height}"
        )));
    }
    let _ = buffer_len(width, height)?;
    Ok(())
}

fn buffer_len(width: u32, height: u32) -> Result<usize> {
    row_len(width)?
        .checked_mul(usize::try_from(height).map_err(|_| {
            RsnipError::Message(format!("image height does not fit usize: {height}"))
        })?)
        .ok_or_else(|| {
            RsnipError::Message(format!("image buffer length overflow: {width}x{height}"))
        })
}

fn row_len(width: u32) -> Result<usize> {
    usize::try_from(width)
        .map_err(|_| RsnipError::Message(format!("image width does not fit usize: {width}")))?
        .checked_mul(4)
        .ok_or_else(|| RsnipError::Message(format!("image row length overflow: {width}")))
}

fn pixel_offset(width: u32, x: u32, y: u32) -> Result<usize> {
    let stride = row_len(width)?;
    usize::try_from(y)
        .ok()
        .and_then(|row| row.checked_mul(stride))
        .and_then(|base| base.checked_add(usize::try_from(x).ok()?.checked_mul(4)?))
        .ok_or_else(|| RsnipError::Message("pixel offset overflow".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{CaptureRegion, CapturedImage};

    #[test]
    fn crop_supports_negative_virtual_origin() {
        let mut bgra = Vec::new();
        for i in 0..16u8 {
            bgra.extend_from_slice(&[i, i, i, 255]);
        }
        let image = CapturedImage::new(-2, -2, 4, 4, bgra).unwrap();
        let crop = image
            .crop(CaptureRegion::new(-1, -1, 2, 2).unwrap())
            .unwrap();

        assert_eq!(crop.origin_x, -1);
        assert_eq!(crop.origin_y, -1);
        assert_eq!(crop.width, 2);
        assert_eq!(crop.height, 2);
        assert_eq!(crop.bgra.len(), 16);
    }

    #[test]
    fn rejects_out_of_bounds_crop() {
        let image = CapturedImage::new(0, 0, 2, 2, vec![0; 16]).unwrap();
        assert!(image.crop(CaptureRegion::new(1, 1, 2, 2).unwrap()).is_err());
    }
}
