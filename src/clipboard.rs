use std::io::Cursor;
use std::mem::size_of;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use image::{ImageBuffer, ImageFormat, Rgba, RgbaImage};

use windows::Win32::Foundation::{BOOL, HANDLE, HGLOBAL, HWND, POINT};
use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFOHEADER};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::DROPFILES;
use windows::core::w;

use crate::errors::{Result, RsnipError};
use crate::screen::capture::CapturedImage;

const CLIPBOARD_OPEN_RETRIES: usize = 5;
const CLIPBOARD_RETRY_DELAY: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardFormat {
    Image,
    Text,
    FileDrop,
}

struct ClipboardGuard;

struct GlobalMem {
    handle: HGLOBAL,
    transferred: bool,
}

unsafe extern "system" {
    fn GlobalFree(hmem: HGLOBAL) -> HGLOBAL;
}

pub fn copy_text(text: &str) -> Result<()> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let bytes = unsafe {
        std::slice::from_raw_parts(utf16.as_ptr().cast::<u8>(), utf16.len() * size_of::<u16>())
    };
    set_clipboard_bytes(CF_UNICODETEXT.0.into(), bytes)
}

pub fn copy_image(image: &CapturedImage) -> Result<()> {
    let dib = image_to_dib(image)?;
    let png = image_to_png(image)?;
    let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    if png_format == 0 {
        return Err(RsnipError::Message(
            "RegisterClipboardFormatW failed for PNG".to_owned(),
        ));
    }
    set_clipboard_formats(&[
        (CF_DIB.0.into(), dib.as_slice()),
        (png_format, png.as_slice()),
    ])
}

pub fn copy_file(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let path_string = absolute.to_string_lossy();
    let mut wide_path: Vec<u16> = path_string.encode_utf16().collect();
    wide_path.push(0);
    wide_path.push(0);

    let header_size = size_of::<DROPFILES>();
    let path_bytes_len = wide_path.len() * size_of::<u16>();
    let total_len = header_size
        .checked_add(path_bytes_len)
        .ok_or_else(|| RsnipError::Message("CF_HDROP buffer length overflow".to_owned()))?;
    let mut bytes = vec![0u8; total_len];

    let dropfiles = DROPFILES {
        pFiles: header_size as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: BOOL(0),
        fWide: BOOL(1),
    };

    unsafe {
        std::ptr::copy_nonoverlapping(
            (&dropfiles as *const DROPFILES).cast::<u8>(),
            bytes.as_mut_ptr(),
            header_size,
        );
        std::ptr::copy_nonoverlapping(
            wide_path.as_ptr().cast::<u8>(),
            bytes.as_mut_ptr().add(header_size),
            path_bytes_len,
        );
    }

    set_clipboard_bytes(CF_HDROP.0.into(), &bytes)
}

pub fn image_to_dib(image: &CapturedImage) -> Result<Vec<u8>> {
    let header_size = size_of::<BITMAPINFOHEADER>();
    let pixel_bytes = image.bgra.len();
    let total_len = header_size
        .checked_add(pixel_bytes)
        .ok_or_else(|| RsnipError::Message("DIB buffer length overflow".to_owned()))?;
    let mut dib = vec![0u8; total_len];

    let width = i32::try_from(image.width)
        .map_err(|_| RsnipError::Message(format!("image width too large: {}", image.width)))?;
    let height = i32::try_from(image.height)
        .map_err(|_| RsnipError::Message(format!("image height too large: {}", image.height)))?;

    let header = BITMAPINFOHEADER {
        biSize: header_size as u32,
        biWidth: width,
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: u32::try_from(pixel_bytes).unwrap_or(0),
        ..Default::default()
    };

    unsafe {
        std::ptr::copy_nonoverlapping(
            (&header as *const BITMAPINFOHEADER).cast::<u8>(),
            dib.as_mut_ptr(),
            header_size,
        );
        std::ptr::copy_nonoverlapping(
            image.bgra.as_ptr(),
            dib.as_mut_ptr().add(header_size),
            pixel_bytes,
        );
    }

    Ok(dib)
}

pub fn image_to_png(image: &CapturedImage) -> Result<Vec<u8>> {
    let rgba = bgra_to_rgba(&image.bgra)?;
    let buffer: RgbaImage =
        ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(image.width, image.height, rgba).ok_or_else(
            || {
                RsnipError::Message(format!(
                    "failed to build clipboard PNG buffer for {}x{} image",
                    image.width, image.height
                ))
            },
        )?;
    let mut png = Cursor::new(Vec::new());
    buffer
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|error| RsnipError::Message(format!("failed to encode clipboard PNG: {error}")))?;
    Ok(png.into_inner())
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

fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> Result<()> {
    set_clipboard_formats(&[(format, bytes)])
}

fn set_clipboard_formats(formats: &[(u32, &[u8])]) -> Result<()> {
    let _clipboard = ClipboardGuard::open_with_retries()?;
    unsafe { EmptyClipboard() }
        .map_err(|error| RsnipError::Message(format!("EmptyClipboard failed: {error}")))?;

    let mut memories = Vec::with_capacity(formats.len());
    for (format, bytes) in formats {
        let mut memory = GlobalMem::new(bytes.len())?;
        memory.write(bytes)?;
        unsafe { SetClipboardData(*format, HANDLE(memory.handle.0)) }
            .map_err(|error| RsnipError::Message(format!("SetClipboardData failed: {error}")))?;
        memory.transferred = true;
        memories.push(memory);
    }
    Ok(())
}

impl ClipboardGuard {
    fn open_with_retries() -> Result<Self> {
        let mut last_error = None;
        for _ in 0..CLIPBOARD_OPEN_RETRIES {
            match unsafe { OpenClipboard(HWND::default()) } {
                Ok(()) => return Ok(Self),
                Err(error) => {
                    last_error = Some(error);
                    sleep(CLIPBOARD_RETRY_DELAY);
                }
            }
        }
        Err(RsnipError::Message(format!(
            "OpenClipboard failed after {CLIPBOARD_OPEN_RETRIES} attempts: {}",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "unknown error".to_owned())
        )))
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

impl GlobalMem {
    fn new(size: usize) -> Result<Self> {
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, size) }
            .map_err(|error| RsnipError::Message(format!("GlobalAlloc failed: {error}")))?;
        Ok(Self {
            handle,
            transferred: false,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let ptr = unsafe { GlobalLock(self.handle) };
        if ptr.is_null() {
            return Err(RsnipError::Message("GlobalLock failed".to_owned()));
        }

        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        }

        unsafe { GlobalUnlock(self.handle) }
            .or_else(|_| Ok::<(), windows::core::Error>(()))
            .map_err(|error| RsnipError::Message(format!("GlobalUnlock failed: {error}")))?;
        Ok(())
    }
}

impl Drop for GlobalMem {
    fn drop(&mut self) {
        if !self.transferred {
            let _ = unsafe { GlobalFree(self.handle) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{image_to_dib, image_to_png};
    use crate::screen::capture::CapturedImage;

    #[test]
    fn dib_contains_top_down_32bit_header_and_pixels() {
        let image = CapturedImage::new(0, 0, 2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let dib = image_to_dib(&image).unwrap();

        assert_eq!(dib.len(), 40 + 8);
        assert_eq!(u32::from_le_bytes(dib[0..4].try_into().unwrap()), 40);
        assert_eq!(i32::from_le_bytes(dib[4..8].try_into().unwrap()), 2);
        assert_eq!(i32::from_le_bytes(dib[8..12].try_into().unwrap()), -1);
        assert_eq!(u16::from_le_bytes(dib[14..16].try_into().unwrap()), 32);
        assert_eq!(&dib[40..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn png_contains_png_signature() {
        let image = CapturedImage::new(0, 0, 1, 1, vec![1, 2, 3, 255]).unwrap();
        let png = image_to_png(&image).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
