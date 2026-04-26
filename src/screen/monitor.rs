use std::sync::OnceLock;

use tracing::{debug, warn};
use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, PROCESS_PER_MONITOR_DPI_AWARE,
    SetProcessDpiAwareness, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::errors::{Result, RsnipError};

static DPI_AWARENESS_INITIALIZED: OnceLock<bool> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualScreen {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorInfo {
    pub bounds: ScreenRect,
    pub work_area: ScreenRect,
    pub primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl VirtualScreen {
    pub fn current() -> Result<Self> {
        let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };

        let width = u32::try_from(width).map_err(|_| {
            RsnipError::Message(format!("invalid virtual screen width from WinAPI: {width}"))
        })?;
        let height = u32::try_from(height).map_err(|_| {
            RsnipError::Message(format!(
                "invalid virtual screen height from WinAPI: {height}"
            ))
        })?;

        if width == 0 || height == 0 {
            return Err(RsnipError::Message(format!(
                "invalid virtual screen dimensions: {width}x{height} at {x},{y}"
            )));
        }

        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    pub fn right(self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }

    pub fn rect(self) -> ScreenRect {
        ScreenRect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

impl ScreenRect {
    pub fn right(self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }

    pub fn contains_rect(self, other: ScreenRect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    pub fn contains_point(self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }
}

pub fn initialize_dpi_awareness() -> Result<()> {
    let initialized = *DPI_AWARENESS_INITIALIZED.get_or_init(|| {
        // SAFETY: Process-wide DPI awareness is set before creating windows/capture resources.
        let modern_result =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        match modern_result {
            Ok(()) => {
                debug!("DPI awareness set to per-monitor v2");
                return true;
            }
            Err(error) => {
                debug!(%error, "failed to set per-monitor v2 DPI awareness; trying fallback");
            }
        }

        // SAFETY: Fallback process-wide DPI awareness for older Windows versions.
        match unsafe { SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE) } {
            Ok(()) => {
                debug!("DPI awareness set to per-monitor fallback");
                true
            }
            Err(error) => {
                warn!(%error, "failed to set process DPI awareness");
                false
            }
        }
    });

    if initialized {
        Ok(())
    } else {
        Err(RsnipError::Message(
            "failed to set process DPI awareness".to_owned(),
        ))
    }
}

pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    let mut monitors = Vec::new();
    let monitors_ptr = &mut monitors as *mut Vec<MonitorInfo>;

    // SAFETY: The callback only uses the LPARAM as the Vec pointer created above,
    // and EnumDisplayMonitors runs synchronously before this function returns.
    let ok = unsafe {
        EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(enum_monitor_callback),
            LPARAM(monitors_ptr as isize),
        )
    };

    if !ok.as_bool() {
        return Err(RsnipError::Message("EnumDisplayMonitors failed".to_owned()));
    }

    Ok(monitors)
}

unsafe extern "system" fn enum_monitor_callback(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let monitors = unsafe { &mut *(data.0 as *mut Vec<MonitorInfo>) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };

    // SAFETY: monitor is provided by EnumDisplayMonitors and info points to a valid MONITORINFO.
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        monitors.push(MonitorInfo {
            bounds: rect_to_screen_rect(info.rcMonitor),
            work_area: rect_to_screen_rect(info.rcWork),
            primary: info.dwFlags & 1 == 1,
        });
    }

    true.into()
}

fn rect_to_screen_rect(rect: RECT) -> ScreenRect {
    let width = rect.right.saturating_sub(rect.left).max(0) as u32;
    let height = rect.bottom.saturating_sub(rect.top).max(0) as u32;
    ScreenRect {
        x: rect.left,
        y: rect.top,
        width,
        height,
    }
}
