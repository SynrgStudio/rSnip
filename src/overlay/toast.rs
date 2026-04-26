use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::ptr::null_mut;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use tracing::info;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, PAINTSTRUCT,
    SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, IDC_ARROW, KillTimer,
    LWA_ALPHA, LoadCursorW, MSG, PostQuitMessage, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN,
    SW_HIDE, SW_SHOWNA, SendMessageW, SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW,
    ShowWindow, WM_CLOSE, WM_DESTROY, WM_LBUTTONUP, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_TIMER,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::PCWSTR;

const TOAST_CLASS_NAME: &str = "RSnipToastWindow";
const TOAST_TIMER_ID: usize = 1;
const TOAST_WIDTH: i32 = 360;
const TOAST_HEIGHT: i32 = 86;
const TOAST_X_MARGIN: i32 = 24;
const TOAST_BOTTOM_MARGIN: i32 = 54;
const TOAST_DURATION: Duration = Duration::from_millis(2200);
const TOAST_FADE_IN: Duration = Duration::from_millis(140);
const TOAST_FADE_OUT: Duration = Duration::from_millis(260);
const TOAST_FRAME_MS: u32 = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastMessage(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastAction {
    None,
    DebugLog,
    OpenEditor(PathBuf),
    OpenInExplorer(PathBuf),
}

#[derive(Debug)]
struct ToastWindowState {
    title: Vec<u16>,
    message: Vec<u16>,
    kind: ToastKind,
    action: ToastAction,
    created_at: Instant,
}

static ACTIVE_TOAST_HWND: OnceLock<Mutex<Option<isize>>> = OnceLock::new();

pub fn show_simple_toast_async(
    title: impl Into<String>,
    message: impl Into<String>,
    kind: ToastKind,
) {
    show_toast_async(title, message, kind, ToastAction::None);
}

pub fn show_toast_async(
    title: impl Into<String>,
    message: impl Into<String>,
    kind: ToastKind,
    action: ToastAction,
) {
    let title = title.into();
    let message = message.into();
    thread::spawn(move || show_toast_window(title, message, kind, action));
}

fn show_toast_window(title: String, message: String, kind: ToastKind, action: ToastAction) {
    close_active_toast();

    let class_name = to_wide_null(TOAST_CLASS_NAME);
    let state = Box::new(ToastWindowState {
        title: to_wide_null(&title),
        message: to_wide_null(&message),
        kind,
        action,
        created_at: Instant::now(),
    });

    // SAFETY: The class name and window procedure are valid for registration. Duplicate registration is harmless.
    unsafe {
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let window_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(toast_window_proc),
            hCursor: cursor,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassW(&window_class);
    }

    let (x, y) = toast_position();
    let state_ptr = Box::into_raw(state);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            x,
            y,
            TOAST_WIDTH,
            TOAST_HEIGHT,
            None,
            None,
            None,
            Some(state_ptr.cast()),
        )
    };

    let Ok(hwnd) = hwnd else {
        // SAFETY: state_ptr came from Box::into_raw above and was not transferred to a window.
        unsafe {
            drop(Box::from_raw(state_ptr));
        }
        return;
    };
    set_active_toast(hwnd);

    // SAFETY: hwnd is a valid toast window created above.
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
        let _ = SetTimer(hwnd, TOAST_TIMER_ID, TOAST_FRAME_MS, None);
        let _ = ShowWindow(hwnd, SW_SHOWNA);
    }

    let mut message = MSG::default();
    loop {
        // SAFETY: message is a valid MSG pointer for the duration of the call.
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        // SAFETY: message was produced by GetMessageW.
        unsafe {
            let _ = DispatchMessageW(&message);
        }
    }
}

fn active_toast() -> &'static Mutex<Option<isize>> {
    ACTIVE_TOAST_HWND.get_or_init(|| Mutex::new(None))
}

fn close_active_toast() {
    let hwnd = active_toast()
        .lock()
        .ok()
        .and_then(|mut active| active.take());
    if let Some(hwnd) = hwnd {
        // SAFETY: The value was produced from a valid HWND. Hiding then sending WM_CLOSE avoids visible overlap with the replacement toast.
        unsafe {
            let hwnd = HWND(hwnd as *mut std::ffi::c_void);
            let _ = ShowWindow(hwnd, SW_HIDE);
            let _ = SendMessageW(hwnd, WM_CLOSE, None, None);
        }
    }
}

fn set_active_toast(hwnd: HWND) {
    if let Ok(mut active) = active_toast().lock() {
        *active = Some(hwnd.0 as isize);
    }
}

fn clear_active_toast(hwnd: HWND) {
    if let Ok(mut active) = active_toast().lock() {
        if *active == Some(hwnd.0 as isize) {
            *active = None;
        }
    }
}

fn toast_position() -> (i32, i32) {
    // SAFETY: GetSystemMetrics has no preconditions.
    let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    (
        screen_width - TOAST_WIDTH - TOAST_X_MARGIN,
        screen_height - TOAST_HEIGHT - TOAST_BOTTOM_MARGIN,
    )
}

unsafe extern "system" fn toast_window_proc(
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
                let state_ptr = unsafe { (*create_struct).lpCreateParams } as *mut ToastWindowState;
                unsafe {
                    let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
                }
            }
            LRESULT(1)
        }
        WM_PAINT => {
            paint_toast(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TOAST_TIMER_ID {
                update_toast_opacity_or_close(hwnd);
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_LBUTTONUP => {
            handle_toast_click(hwnd);
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
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            clear_active_toast(hwnd);
            let state_ptr =
                unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ToastWindowState;
            if !state_ptr.is_null() {
                unsafe {
                    drop(Box::from_raw(state_ptr));
                    let _ = SetWindowLongPtrW(
                        hwnd,
                        GWLP_USERDATA,
                        null_mut::<ToastWindowState>() as isize,
                    );
                }
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn handle_toast_click(hwnd: HWND) {
    let action = toast_action(hwnd).unwrap_or(ToastAction::None);
    match action {
        ToastAction::None => {}
        ToastAction::DebugLog => info!("toast debug click action executed"),
        ToastAction::OpenEditor(path) => open_editor_from_toast(path),
        ToastAction::OpenInExplorer(path) => open_path_in_explorer_from_toast(path),
    }

    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

fn toast_action(hwnd: HWND) -> Option<ToastAction> {
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const ToastWindowState;
    if state_ptr.is_null() {
        return None;
    }
    Some(unsafe { (*state_ptr).action.clone() })
}

fn open_editor_from_toast(path: PathBuf) {
    thread::spawn(move || {
        if !path.exists() {
            info!(path = %path.display(), "toast editor action ignored because image path does not exist");
            return;
        }
        match std::env::current_exe()
            .map_err(|error| error.to_string())
            .and_then(|exe| {
                ProcessCommand::new(exe)
                    .arg("__editor-once")
                    .arg(&path)
                    .spawn()
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }) {
            Ok(()) => info!(path = %path.display(), "toast opened editor"),
            Err(error) => info!(path = %path.display(), %error, "toast failed to open editor"),
        }
    });
}

fn open_path_in_explorer_from_toast(path: PathBuf) {
    thread::spawn(move || {
        if !path.exists() {
            info!(path = %path.display(), "toast explorer action ignored because path does not exist");
            return;
        }
        let select_arg = format!("/select,{}", path.display());
        match ProcessCommand::new("explorer.exe").arg(select_arg).spawn() {
            Ok(_) => info!(path = %path.display(), "toast opened Explorer for file"),
            Err(error) => info!(path = %path.display(), %error, "toast failed to open Explorer"),
        }
    });
}

fn update_toast_opacity_or_close(hwnd: HWND) {
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const ToastWindowState;
    if state_ptr.is_null() {
        return;
    }

    let elapsed = unsafe { &*state_ptr }.created_at.elapsed();
    if elapsed >= TOAST_DURATION {
        unsafe {
            let _ = KillTimer(hwnd, TOAST_TIMER_ID);
            let _ = DestroyWindow(hwnd);
        }
        return;
    }

    let alpha = toast_alpha(elapsed);
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
    }
}

fn toast_alpha(elapsed: Duration) -> u8 {
    if elapsed < TOAST_FADE_IN {
        return scaled_alpha(elapsed, TOAST_FADE_IN);
    }

    let fade_out_start = TOAST_DURATION.saturating_sub(TOAST_FADE_OUT);
    if elapsed >= fade_out_start {
        return u8::MAX.saturating_sub(scaled_alpha(elapsed - fade_out_start, TOAST_FADE_OUT));
    }

    u8::MAX
}

fn scaled_alpha(elapsed: Duration, duration: Duration) -> u8 {
    let elapsed_ms = elapsed.as_millis().min(duration.as_millis());
    let duration_ms = duration.as_millis().max(1);
    ((elapsed_ms * u128::from(u8::MAX)) / duration_ms) as u8
}

fn paint_toast(hwnd: HWND) {
    // SAFETY: hwnd is provided by Windows during WM_PAINT handling.
    unsafe {
        let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ToastWindowState;
        if state_ptr.is_null() {
            return;
        }
        let state = &*state_ptr;
        let mut paint = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut paint);
        let background = match state.kind {
            ToastKind::Info => COLORREF(0x00202020),
            ToastKind::Error => COLORREF(0x00202060),
        };
        let brush = CreateSolidBrush(background);
        let mut full_rect = RECT {
            left: 0,
            top: 0,
            right: TOAST_WIDTH,
            bottom: TOAST_HEIGHT,
        };
        let _ = FillRect(hdc, &full_rect, brush);
        let _ = DeleteObject(brush);

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(0x00FFFFFF));
        full_rect.left = 16;
        full_rect.top = 12;
        full_rect.right = TOAST_WIDTH - 16;
        full_rect.bottom = 34;
        let mut title = state.title.clone();
        let _ = DrawTextW(
            hdc,
            &mut title,
            &mut full_rect,
            windows::Win32::Graphics::Gdi::DT_SINGLELINE,
        );

        let _ = SetTextColor(hdc, COLORREF(0x00E0E0E0));
        let mut message_rect = RECT {
            left: 16,
            top: 40,
            right: TOAST_WIDTH - 16,
            bottom: TOAST_HEIGHT - 12,
        };
        let mut message = state.message.clone();
        let _ = DrawTextW(
            hdc,
            &mut message,
            &mut message_rect,
            windows::Win32::Graphics::Gdi::DT_WORDBREAK,
        );
        let _ = EndPaint(hwnd, &paint);
    }
}

fn to_wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_nul_terminated() {
        let value = to_wide_null("hola");
        assert_eq!(value.last(), Some(&0));
        assert_eq!(&value[..4], &[104, 111, 108, 97]);
    }

    #[test]
    fn toast_alpha_fades_in_holds_and_fades_out() {
        assert_eq!(toast_alpha(Duration::from_millis(0)), 0);
        assert!(toast_alpha(Duration::from_millis(70)) > 0);
        assert_eq!(toast_alpha(Duration::from_millis(140)), u8::MAX);
        assert_eq!(toast_alpha(Duration::from_millis(1000)), u8::MAX);
        assert!(toast_alpha(Duration::from_millis(2100)) < u8::MAX);
    }

    #[test]
    fn toast_actions_are_explicit() {
        assert_eq!(ToastAction::None, ToastAction::None);
        assert_eq!(ToastAction::DebugLog, ToastAction::DebugLog);
    }
}
