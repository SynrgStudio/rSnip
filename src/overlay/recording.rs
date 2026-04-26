use std::ptr::null_mut;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CombineRgn, CreatePen, CreateRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, FillRect, GetStockObject, InvalidateRect, NULL_BRUSH, PAINTSTRUCT, PS_DOT, RGN_OR,
    Rectangle, SelectObject, SetBkMode, SetTextColor, SetWindowRgn, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, IDC_ARROW, KillTimer, LWA_ALPHA, LoadCursorW,
    MSG, PostQuitMessage, RegisterClassW, SW_SHOWNA, SetLayeredWindowAttributes, SetTimer,
    SetWindowDisplayAffinity, SetWindowLongPtrW, ShowWindow, WDA_EXCLUDEFROMCAPTURE, WM_CLOSE,
    WM_DESTROY, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::errors::{Result, RsnipError};
use crate::screen::capture::CaptureRegion;

const RECORDING_OVERLAY_CLASS_NAME: &str = "RSnipRecordingOverlayWindow";
const COMMAND_TIMER_ID: usize = 1;
const CLOCK_TIMER_ID: usize = 2;
const COMMAND_TIMER_MS: u32 = 50;
const CLOCK_TIMER_MS: u32 = 250;
const BAR_HEIGHT: i32 = 28;
const BORDER_INSET: i32 = 1;
const BORDER_THICKNESS: i32 = 2;

#[derive(Debug)]
pub struct RecordingOverlayHandle {
    command_sender: Sender<RecordingOverlayCommand>,
    join_handle: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingOverlayStatus {
    pub elapsed: Duration,
    pub fps: u32,
}

#[derive(Debug)]
enum RecordingOverlayCommand {
    Update(RecordingOverlayStatus),
    Stop,
}

#[derive(Debug)]
struct RecordingOverlayWindowState {
    command_receiver: Receiver<RecordingOverlayCommand>,
    region: CaptureRegion,
    started_at: Instant,
    status: RecordingOverlayStatus,
}

impl RecordingOverlayHandle {
    pub fn start(region: CaptureRegion, fps: u32) -> Result<Self> {
        let (command_sender, command_receiver) = mpsc::channel();
        let join_handle = thread::Builder::new()
            .name("rsnip-recording-overlay".to_owned())
            .spawn(move || run_recording_overlay_thread(region, fps, command_receiver))
            .map_err(|error| {
                RsnipError::Message(format!("failed to start recording overlay thread: {error}"))
            })?;

        Ok(Self {
            command_sender,
            join_handle: Some(join_handle),
        })
    }

    pub fn update(&self, status: RecordingOverlayStatus) -> Result<()> {
        self.command_sender
            .send(RecordingOverlayCommand::Update(status))
            .map_err(|_| RsnipError::Message("recording overlay is not running".to_owned()))
    }

    pub fn stop(mut self) {
        let _ = self.command_sender.send(RecordingOverlayCommand::Stop);
        if let Some(join_handle) = self.join_handle.take() {
            if join_handle.join().is_err() {
                warn!("recording overlay thread panicked during stop");
            }
        }
    }
}

impl Drop for RecordingOverlayHandle {
    fn drop(&mut self) {
        let _ = self.command_sender.send(RecordingOverlayCommand::Stop);
        if let Some(join_handle) = self.join_handle.take() {
            if join_handle.join().is_err() {
                warn!("recording overlay thread panicked during drop");
            }
        }
    }
}

fn run_recording_overlay_thread(
    region: CaptureRegion,
    fps: u32,
    command_receiver: Receiver<RecordingOverlayCommand>,
) {
    if let Err(error) = show_recording_overlay_window(region, fps, command_receiver) {
        warn!(%error, "recording overlay failed");
    }
}

fn show_recording_overlay_window(
    region: CaptureRegion,
    fps: u32,
    command_receiver: Receiver<RecordingOverlayCommand>,
) -> Result<()> {
    let class_name = to_wide_null(RECORDING_OVERLAY_CLASS_NAME);
    let state = Box::new(RecordingOverlayWindowState {
        command_receiver,
        region,
        started_at: Instant::now(),
        status: RecordingOverlayStatus {
            elapsed: Duration::ZERO,
            fps,
        },
    });

    unsafe {
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let window_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(recording_overlay_window_proc),
            hCursor: cursor,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassW(&window_class);
    }

    let state_ptr = Box::into_raw(state);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED,
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            region.x,
            region.y,
            region.width as i32,
            region.height as i32,
            None,
            None,
            None,
            Some(state_ptr.cast()),
        )
    };

    let Ok(hwnd) = hwnd else {
        unsafe {
            drop(Box::from_raw(state_ptr));
        }
        return Err(RsnipError::Message(
            "failed to create recording overlay window".to_owned(),
        ));
    };

    apply_overlay_window_region(hwnd, region);

    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 230, LWA_ALPHA);
        if let Err(error) = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
            warn!(%error, "recording overlay exclusion from capture is unavailable");
        }
        let _ = SetTimer(hwnd, COMMAND_TIMER_ID, COMMAND_TIMER_MS, None);
        let _ = SetTimer(hwnd, CLOCK_TIMER_ID, CLOCK_TIMER_MS, None);
        let _ = ShowWindow(hwnd, SW_SHOWNA);
    }

    info!(
        x = region.x,
        y = region.y,
        width = region.width,
        height = region.height,
        fps,
        "recording overlay started"
    );

    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        unsafe {
            let _ = DispatchMessageW(&message);
        }
    }

    info!("recording overlay stopped");
    Ok(())
}

unsafe extern "system" fn recording_overlay_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create_struct =
                lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
            if !create_struct.is_null() {
                let state_ptr =
                    unsafe { (*create_struct).lpCreateParams } as *mut RecordingOverlayWindowState;
                unsafe {
                    let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
                }
            }
            LRESULT(1)
        }
        WM_PAINT => {
            paint_recording_overlay(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                COMMAND_TIMER_ID => poll_recording_overlay_commands(hwnd),
                CLOCK_TIMER_ID => update_recording_overlay_clock(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                let _ = KillTimer(hwnd, COMMAND_TIMER_ID);
                let _ = KillTimer(hwnd, CLOCK_TIMER_ID);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) }
                as *mut RecordingOverlayWindowState;
            if !state_ptr.is_null() {
                unsafe {
                    drop(Box::from_raw(state_ptr));
                    let _ = SetWindowLongPtrW(
                        hwnd,
                        GWLP_USERDATA,
                        null_mut::<RecordingOverlayWindowState>() as isize,
                    );
                }
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn poll_recording_overlay_commands(hwnd: HWND) {
    let state_ptr =
        unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut RecordingOverlayWindowState;
    if state_ptr.is_null() {
        return;
    }

    loop {
        let command = unsafe { &*state_ptr }.command_receiver.try_recv();
        match command {
            Ok(RecordingOverlayCommand::Update(status)) => {
                unsafe {
                    (*state_ptr).status = status;
                }
                invalidate(hwnd);
            }
            Ok(RecordingOverlayCommand::Stop) => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                break;
            }
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                break;
            }
        }
    }
}

fn update_recording_overlay_clock(hwnd: HWND) {
    let state_ptr =
        unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut RecordingOverlayWindowState;
    if state_ptr.is_null() {
        return;
    }

    unsafe {
        (*state_ptr).status.elapsed = (*state_ptr).started_at.elapsed();
    }
    invalidate(hwnd);
}

fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

fn apply_overlay_window_region(hwnd: HWND, region: CaptureRegion) {
    let width = region.width as i32;
    let height = region.height as i32;
    let border = BORDER_THICKNESS.max(1);
    let bar_height = BAR_HEIGHT.min(height).max(border);

    unsafe {
        let combined = CreateRectRgn(0, 0, width, bar_height);
        let top = CreateRectRgn(0, 0, width, border);
        let bottom = CreateRectRgn(0, (height - border).max(0), width, height);
        let left = CreateRectRgn(0, 0, border, height);
        let right = CreateRectRgn((width - border).max(0), 0, width, height);

        let _ = CombineRgn(combined, combined, top, RGN_OR);
        let _ = CombineRgn(combined, combined, bottom, RGN_OR);
        let _ = CombineRgn(combined, combined, left, RGN_OR);
        let _ = CombineRgn(combined, combined, right, RGN_OR);

        let result = SetWindowRgn(hwnd, combined, BOOL(1));
        let _ = DeleteObject(top);
        let _ = DeleteObject(bottom);
        let _ = DeleteObject(left);
        let _ = DeleteObject(right);

        if result == 0 {
            let _ = DeleteObject(combined);
            warn!("failed to apply recording overlay window region");
        }
    }
}

fn paint_recording_overlay(hwnd: HWND) {
    unsafe {
        let state_ptr =
            GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RecordingOverlayWindowState;
        if state_ptr.is_null() {
            return;
        }
        let state = &*state_ptr;
        let mut paint = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut paint);

        let overlay_brush = CreateSolidBrush(COLORREF(0x00302020));
        let width = state.region.width as i32;
        let height = state.region.height as i32;
        let border = BORDER_THICKNESS.max(1);
        let bar_height = BAR_HEIGHT.min(height).max(border);

        let bar_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: bar_height,
        };
        let top_border = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: border,
        };
        let bottom_border = RECT {
            left: 0,
            top: (height - border).max(0),
            right: width,
            bottom: height,
        };
        let left_border = RECT {
            left: 0,
            top: 0,
            right: border,
            bottom: height,
        };
        let right_border = RECT {
            left: (width - border).max(0),
            top: 0,
            right: width,
            bottom: height,
        };
        let _ = FillRect(hdc, &bar_rect, overlay_brush);
        let _ = FillRect(hdc, &top_border, overlay_brush);
        let _ = FillRect(hdc, &bottom_border, overlay_brush);
        let _ = FillRect(hdc, &left_border, overlay_brush);
        let _ = FillRect(hdc, &right_border, overlay_brush);
        let _ = DeleteObject(overlay_brush);

        let border_pen = CreatePen(PS_DOT, BORDER_THICKNESS, COLORREF(0x000000FF));
        let old_pen = SelectObject(hdc, border_pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = Rectangle(
            hdc,
            BORDER_INSET,
            BORDER_INSET,
            state.region.width as i32 - BORDER_INSET,
            state.region.height as i32 - BORDER_INSET,
        );
        let _ = SelectObject(hdc, old_brush);
        let _ = SelectObject(hdc, old_pen);
        let _ = DeleteObject(border_pen);

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(0x00FFFFFF));
        let mut text_rect = RECT {
            left: 10,
            top: 5,
            right: width - 10,
            bottom: BAR_HEIGHT,
        };
        let mut text = to_wide_null(&overlay_text(state.status));
        let _ = DrawTextW(
            hdc,
            &mut text,
            &mut text_rect,
            windows::Win32::Graphics::Gdi::DT_SINGLELINE,
        );

        let _ = EndPaint(hwnd, &paint);
    }
}

fn overlay_text(status: RecordingOverlayStatus) -> String {
    let total_seconds = status.elapsed.as_secs();
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes:02}:{seconds:02} | {} FPS", status.fps)
}

fn to_wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_text_formats_elapsed_and_fps() {
        let text = overlay_text(RecordingOverlayStatus {
            elapsed: Duration::from_secs(65),
            fps: 30,
        });
        assert_eq!(text, "01:05 | 30 FPS");
    }
}
