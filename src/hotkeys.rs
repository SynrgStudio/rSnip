use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};

use tracing::{error, info, warn};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, TranslateMessage, WM_HOTKEY, WM_QUIT,
};

use crate::config::HotkeyConfig;
use crate::errors::{Result, RsnipError};
use crate::ipc::IpcCommand;

const SNIP_HOTKEY_ID: i32 = 1;
const RECORD_HOTKEY_ID: i32 = 2;
const OCR_HOTKEY_ID: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    Snip,
    Record,
    Ocr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedHotkey {
    modifiers: HOT_KEY_MODIFIERS,
    virtual_key: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HotkeyRegistrationSpec {
    id: i32,
    action: HotkeyAction,
    hotkey: ParsedHotkey,
}

#[derive(Debug)]
pub struct HotkeyRuntime {
    thread_id: u32,
    join_handle: Option<JoinHandle<()>>,
}

pub fn parse_hotkey(input: &str) -> Result<ParsedHotkey> {
    let mut modifiers = HOT_KEY_MODIFIERS(0);
    let mut virtual_key = None;

    for raw_part in input.split('+') {
        let part = raw_part.trim().to_ascii_lowercase();
        if part.is_empty() {
            continue;
        }

        match part.as_str() {
            "ctrl" | "control" => modifiers |= MOD_CONTROL,
            "shift" => modifiers |= MOD_SHIFT,
            "alt" => modifiers |= MOD_ALT,
            "win" | "windows" | "meta" => modifiers |= MOD_WIN,
            key => {
                if virtual_key.is_some() {
                    return Err(RsnipError::Message(format!(
                        "hotkey `{input}` contains more than one key"
                    )));
                }
                virtual_key = Some(parse_virtual_key(key, input)?);
            }
        }
    }

    let Some(virtual_key) = virtual_key else {
        return Err(RsnipError::Message(format!(
            "hotkey `{input}` does not contain a key"
        )));
    };

    Ok(ParsedHotkey {
        modifiers,
        virtual_key,
    })
}

pub fn start_hotkey_runtime(
    config: &HotkeyConfig,
    sender: Sender<IpcCommand>,
) -> Result<HotkeyRuntime> {
    let specs = vec![
        HotkeyRegistrationSpec {
            id: SNIP_HOTKEY_ID,
            action: HotkeyAction::Snip,
            hotkey: parse_hotkey(&config.snip)?,
        },
        HotkeyRegistrationSpec {
            id: RECORD_HOTKEY_ID,
            action: HotkeyAction::Record,
            hotkey: parse_hotkey(&config.record)?,
        },
        HotkeyRegistrationSpec {
            id: OCR_HOTKEY_ID,
            action: HotkeyAction::Ocr,
            hotkey: parse_hotkey(&config.ocr)?,
        },
    ];

    let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
    let join_handle = thread::spawn(move || run_hotkey_thread(specs, sender, ready_sender));
    let thread_id = ready_receiver.recv().map_err(|error| {
        RsnipError::Message(format!("hotkey thread failed to start: {error}"))
    })??;

    Ok(HotkeyRuntime {
        thread_id,
        join_handle: Some(join_handle),
    })
}

fn parse_virtual_key(key: &str, input: &str) -> Result<u32> {
    if key.len() == 1 {
        let byte = key.as_bytes()[0];
        if byte.is_ascii_alphabetic() {
            return Ok(byte.to_ascii_uppercase().into());
        }
        if byte.is_ascii_digit() {
            return Ok(byte.into());
        }
    }

    match key {
        "escape" | "esc" => Ok(0x1B),
        "space" => Ok(0x20),
        "enter" | "return" => Ok(0x0D),
        "tab" => Ok(0x09),
        "backspace" => Ok(0x08),
        "delete" | "del" => Ok(0x2E),
        "insert" | "ins" => Ok(0x2D),
        "home" => Ok(0x24),
        "end" => Ok(0x23),
        "pageup" | "pgup" => Ok(0x21),
        "pagedown" | "pgdn" => Ok(0x22),
        "left" => Ok(0x25),
        "up" => Ok(0x26),
        "right" => Ok(0x27),
        "down" => Ok(0x28),
        _ if key.starts_with('f') => parse_function_key(key, input),
        _ => Err(RsnipError::Message(format!(
            "hotkey `{input}` contains unsupported key `{key}`"
        ))),
    }
}

fn parse_function_key(key: &str, input: &str) -> Result<u32> {
    let number: u32 = key[1..].parse().map_err(|error| {
        RsnipError::Message(format!(
            "hotkey `{input}` contains invalid function key `{key}`: {error}"
        ))
    })?;

    if !(1..=24).contains(&number) {
        return Err(RsnipError::Message(format!(
            "hotkey `{input}` contains function key outside F1-F24"
        )));
    }

    Ok(0x70 + number - 1)
}

fn run_hotkey_thread(
    specs: Vec<HotkeyRegistrationSpec>,
    sender: Sender<IpcCommand>,
    ready_sender: Sender<Result<u32>>,
) {
    // SAFETY: GetCurrentThreadId has no preconditions.
    let thread_id = unsafe { GetCurrentThreadId() };
    let mut registered_ids = Vec::new();

    for spec in &specs {
        // SAFETY: RegisterHotKey is called for the current thread with a null HWND.
        let registered = unsafe {
            RegisterHotKey(
                HWND::default(),
                spec.id,
                spec.hotkey.modifiers,
                spec.hotkey.virtual_key,
            )
        };

        if let Err(error) = registered {
            error!(action = ?spec.action, %error, "failed to register global hotkey");
            continue;
        }

        registered_ids.push(spec.id);
        info!(id = spec.id, action = ?spec.action, "registered global hotkey");
    }

    let _ = ready_sender.send(Ok(thread_id));
    message_loop(sender);
    unregister_hotkeys(&registered_ids);
}

fn message_loop(sender: Sender<IpcCommand>) {
    let mut message = MSG::default();

    loop {
        // SAFETY: Message pointer is valid for the duration of the call.
        let result = unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) };
        if result.0 == -1 {
            warn!("GetMessageW failed in hotkey thread");
            break;
        }
        if result.0 == 0 {
            break;
        }

        if message.message == WM_HOTKEY {
            let command = match message.wParam.0 as i32 {
                SNIP_HOTKEY_ID => Some(IpcCommand::Snip),
                RECORD_HOTKEY_ID => Some(IpcCommand::Record),
                OCR_HOTKEY_ID => Some(IpcCommand::Ocr),
                _ => None,
            };

            if let Some(command) = command {
                info!(?command, "global hotkey received");
                if sender.send(command).is_err() {
                    break;
                }
            }
            continue;
        }

        // SAFETY: Message was produced by GetMessageW.
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn unregister_hotkeys(ids: &[i32]) {
    for id in ids {
        // SAFETY: UnregisterHotKey is called for the same thread/window registration style.
        if let Err(error) = unsafe { UnregisterHotKey(HWND::default(), *id) } {
            warn!(id, %error, "failed to unregister hotkey");
        }
    }
}

impl Drop for HotkeyRuntime {
    fn drop(&mut self) {
        // SAFETY: Posting WM_QUIT to the hotkey thread is the intended shutdown signal.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, None, None) };
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_hotkey;

    #[test]
    fn parses_default_snip_hotkey() {
        let hotkey = parse_hotkey("ctrl+shift+s").unwrap();
        assert_eq!(hotkey.virtual_key, u32::from(b'S'));
    }

    #[test]
    fn rejects_hotkey_without_key() {
        assert!(parse_hotkey("ctrl+shift").is_err());
    }
}
