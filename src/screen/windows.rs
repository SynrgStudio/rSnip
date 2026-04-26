use std::ffi::c_void;

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, IsIconic,
    IsWindowVisible, SetForegroundWindow,
};
use windows::core::w;

use crate::errors::{Result, RsnipError};
use crate::screen::capture::CaptureRegion;
use crate::screen::monitor::{ScreenRect, VirtualScreen};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SelectableWindowHandle(pub isize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectableWindow {
    pub handle: SelectableWindowHandle,
    pub title: String,
    pub bounds: CaptureRegion,
    pub kind: SelectableWindowKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectableWindowKind {
    Window,
    Taskbar,
}

pub fn enumerate_selectable_windows() -> Result<Vec<SelectableWindow>> {
    let virtual_screen = VirtualScreen::current()?;
    let mut windows = Vec::new();
    let windows_ptr = &mut windows as *mut Vec<SelectableWindow>;

    // SAFETY: The callback only uses the LPARAM as the Vec pointer created above,
    // and EnumWindows runs synchronously before this function returns.
    unsafe { EnumWindows(Some(enum_window_callback), LPARAM(windows_ptr as isize)) }
        .map_err(|error| RsnipError::Message(format!("EnumWindows failed: {error}")))?;

    windows.retain(|window| is_useful_window(window, virtual_screen));
    add_taskbar_window(&mut windows)?;
    Ok(windows)
}

pub fn foreground_window(handle: SelectableWindowHandle) -> bool {
    // SAFETY: HWND may be stale; SetForegroundWindow reports failure without ownership transfer.
    unsafe { SetForegroundWindow(hwnd_from_handle(handle)).as_bool() }
}

pub fn window_at_point(windows: &[SelectableWindow], x: i32, y: i32) -> Option<&SelectableWindow> {
    windows
        .iter()
        .find(|window| window.bounds.rect().contains_point(x, y))
}

unsafe extern "system" fn enum_window_callback(hwnd: HWND, data: LPARAM) -> BOOL {
    if !is_selectable_hwnd(hwnd) {
        return true.into();
    }

    let Some(bounds) = window_bounds(hwnd) else {
        return true.into();
    };

    let windows = unsafe { &mut *(data.0 as *mut Vec<SelectableWindow>) };
    windows.push(SelectableWindow {
        handle: handle_from_hwnd(hwnd),
        title: window_title(hwnd).unwrap_or_default(),
        bounds,
        kind: SelectableWindowKind::Window,
    });

    true.into()
}

fn is_useful_window(window: &SelectableWindow, virtual_screen: VirtualScreen) -> bool {
    if window.title.trim().is_empty()
        || window.title == "Program Manager"
        || window.title == "Windows Input Experience"
    {
        return false;
    }
    if window.bounds.width < 8 || window.bounds.height < 8 {
        return false;
    }
    virtual_screen.rect().contains_rect(window.bounds.rect())
}

fn add_taskbar_window(windows: &mut Vec<SelectableWindow>) -> Result<()> {
    // SAFETY: Static class name is a valid null-terminated UTF-16 string; no window name filter.
    let hwnd = match unsafe { FindWindowW(w!("Shell_TrayWnd"), None) } {
        Ok(hwnd) => hwnd,
        Err(_) => return Ok(()),
    };

    let handle = handle_from_hwnd(hwnd);
    if windows.iter().any(|window| window.handle == handle) {
        return Ok(());
    }

    let Some(bounds) = window_bounds(hwnd) else {
        return Ok(());
    };

    windows.push(SelectableWindow {
        handle,
        title: "Taskbar".to_owned(),
        bounds,
        kind: SelectableWindowKind::Taskbar,
    });
    Ok(())
}

fn is_selectable_hwnd(hwnd: HWND) -> bool {
    // SAFETY: Visibility/iconic checks do not transfer ownership and tolerate regular top-level HWNDs.
    unsafe { IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() }
}

fn window_title(hwnd: HWND) -> Option<String> {
    // SAFETY: hwnd is provided by EnumWindows/FindWindowW.
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return None;
    }

    let mut buffer = vec![0u16; len as usize + 1];
    // SAFETY: buffer is valid mutable UTF-16 storage including trailing NUL slot.
    let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if copied <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..copied as usize]))
}

fn window_bounds(hwnd: HWND) -> Option<CaptureRegion> {
    extended_frame_bounds(hwnd)
        .or_else(|| window_rect_bounds(hwnd))
        .and_then(capture_region_from_rect)
}

fn extended_frame_bounds(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    // SAFETY: rect points to valid RECT storage and size matches RECT.
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    Some(rect)
}

fn window_rect_bounds(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    // SAFETY: rect points to valid RECT storage.
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(rect)
}

fn capture_region_from_rect(rect: RECT) -> Option<CaptureRegion> {
    let width = rect.right.saturating_sub(rect.left);
    let height = rect.bottom.saturating_sub(rect.top);
    if width <= 0 || height <= 0 {
        return None;
    }
    CaptureRegion::new(rect.left, rect.top, width as u32, height as u32).ok()
}

fn handle_from_hwnd(hwnd: HWND) -> SelectableWindowHandle {
    SelectableWindowHandle(hwnd.0 as isize)
}

fn hwnd_from_handle(handle: SelectableWindowHandle) -> HWND {
    HWND(handle.0 as *mut c_void)
}

pub fn rect_from_region(region: CaptureRegion) -> ScreenRect {
    ScreenRect {
        x: region.x,
        y: region.y,
        width: region.width,
        height: region.height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_first_z_order_window_containing_point() {
        let windows = vec![
            SelectableWindow {
                handle: SelectableWindowHandle(1),
                title: "top".to_owned(),
                bounds: CaptureRegion::new(10, 10, 20, 20).unwrap(),
                kind: SelectableWindowKind::Window,
            },
            SelectableWindow {
                handle: SelectableWindowHandle(2),
                title: "bottom".to_owned(),
                bounds: CaptureRegion::new(0, 0, 100, 100).unwrap(),
                kind: SelectableWindowKind::Window,
            },
        ];

        assert_eq!(
            window_at_point(&windows, 15, 15).map(|window| window.handle),
            Some(SelectableWindowHandle(1))
        );
        assert_eq!(
            window_at_point(&windows, 5, 5).map(|window| window.handle),
            Some(SelectableWindowHandle(2))
        );
        assert_eq!(window_at_point(&windows, 500, 500), None);
    }
}
