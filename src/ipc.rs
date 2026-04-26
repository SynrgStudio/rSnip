use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows::core::w;

use crate::errors::{Result, RsnipError};

const PIPE_NAME: windows::core::PCWSTR = w!(r"\\.\pipe\rsnip");
const PIPE_BUFFER_SIZE: u32 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcCommand {
    Snip,
    Record,
    Ocr,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcRequest {
    pub cmd: IpcCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcResponse {
    Ok { message: String },
    Error { message: String },
}

pub fn start_named_pipe_server(
    sender: Sender<IpcCommand>,
    recording_active: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || run_named_pipe_server(sender, recording_active))
}

pub fn send_command(command: IpcCommand, timeout: Duration) -> Result<IpcResponse> {
    let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: PIPE_NAME is a valid static UTF-16 string. The timeout is bounded to u32.
    let pipe_available = unsafe { WaitNamedPipeW(PIPE_NAME, timeout_ms) };
    if !pipe_available.as_bool() {
        let last_error = unsafe { GetLastError() };
        return Err(RsnipError::Message(format!(
            "rsnip daemon is not reachable on \\.\\pipe\\rsnip within {}ms: {last_error:?}",
            timeout.as_millis()
        )));
    }

    let pipe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(r"\\.\pipe\rsnip")?;
    let mut pipe = BufReader::new(pipe);
    let request = IpcRequest::new(command).to_json_line()?;
    pipe.get_mut().write_all(request.as_bytes())?;
    pipe.get_mut().flush()?;

    let mut response_line = String::new();
    pipe.read_line(&mut response_line)?;
    if response_line.trim().is_empty() {
        return Err(RsnipError::Message(
            "empty response from rsnip daemon".to_owned(),
        ));
    }
    IpcResponse::from_json_line(&response_line)
}

fn run_named_pipe_server(sender: Sender<IpcCommand>, recording_active: Arc<AtomicBool>) {
    info!("IPC named pipe server starting");

    loop {
        match create_pipe_file() {
            Ok(pipe) => {
                let should_continue = handle_client(pipe, &sender, &recording_active);
                if !should_continue {
                    break;
                }
            }
            Err(error) => {
                error!(%error, "failed to create IPC named pipe");
                break;
            }
        }
    }

    info!("IPC named pipe server stopped");
}

fn create_pipe_file() -> Result<File> {
    // SAFETY: The pipe name is a valid static UTF-16 string. Security attributes are omitted.
    let handle = unsafe {
        CreateNamedPipeW(
            PIPE_NAME,
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_SIZE,
            PIPE_BUFFER_SIZE,
            0,
            None,
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err(RsnipError::Message("CreateNamedPipeW failed".to_owned()));
    }

    // SAFETY: The handle is a valid named pipe handle returned by CreateNamedPipeW.
    let connected = unsafe { ConnectNamedPipe(handle, None) };
    if connected.is_err() {
        // SAFETY: GetLastError reads the thread-local Win32 last error immediately after ConnectNamedPipe.
        let last_error = unsafe { GetLastError() };
        if last_error != ERROR_PIPE_CONNECTED {
            // SAFETY: The handle is owned here and must be closed before returning the error.
            let _ = unsafe { CloseHandle(handle) };
            return Err(RsnipError::Message(format!(
                "ConnectNamedPipe failed: {last_error:?}"
            )));
        }
    }

    // SAFETY: Ownership of the valid pipe HANDLE is transferred to File and will be closed by File drop.
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

fn handle_client(pipe: File, sender: &Sender<IpcCommand>, recording_active: &AtomicBool) -> bool {
    let mut reader = BufReader::new(pipe);
    let mut line = String::new();
    let response = match reader.read_line(&mut line) {
        Ok(0) => IpcResponse::error("empty IPC request"),
        Ok(_) => match IpcRequest::from_json_line(&line) {
            Ok(request) => {
                info!(cmd = ?request.cmd, "IPC command received");
                match sender.send(request.cmd) {
                    Ok(()) => IpcResponse::ok(accepted_message(request.cmd, recording_active)),
                    Err(error) => {
                        IpcResponse::error(format!("daemon dispatcher unavailable: {error}"))
                    }
                }
            }
            Err(error) => IpcResponse::error(error.to_string()),
        },
        Err(error) => IpcResponse::error(format!("failed to read IPC request: {error}")),
    };

    let should_continue =
        !matches!(response, IpcResponse::Ok { ref message } if message.contains("Shutdown"));

    let line = match response.to_json_line() {
        Ok(line) => line,
        Err(error) => {
            warn!(%error, "failed to serialize IPC response");
            return should_continue;
        }
    };

    if let Err(error) = reader.get_mut().write_all(line.as_bytes()) {
        warn!(%error, "failed to write IPC response");
    }
    if let Err(error) = reader.get_mut().flush() {
        warn!(%error, "failed to flush IPC response");
    }

    should_continue
}

fn accepted_message(command: IpcCommand, recording_active: &AtomicBool) -> &'static str {
    match command {
        IpcCommand::Snip => {
            "snip accepted: select a region; selected image will be copied to clipboard"
        }
        IpcCommand::Record if recording_active.load(Ordering::SeqCst) => {
            "record stop accepted: active recording will stop"
        }
        IpcCommand::Record => "record start accepted: select a region to start recording",
        IpcCommand::Ocr => "ocr accepted: select a region; text will be copied to clipboard",
        IpcCommand::Shutdown => "shutdown accepted: daemon stopping",
    }
}

impl IpcRequest {
    pub fn new(cmd: IpcCommand) -> Self {
        Self { cmd }
    }

    pub fn to_json_line(&self) -> Result<String> {
        let mut line = serde_json::to_string(self).map_err(|error| {
            RsnipError::Message(format!("failed to serialize IPC request: {error}"))
        })?;
        line.push('\n');
        Ok(line)
    }

    pub fn from_json_line(line: &str) -> Result<Self> {
        serde_json::from_str(line.trim()).map_err(|error| {
            RsnipError::Message(format!(
                "failed to parse IPC request `{}`: {error}",
                line.trim()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_accepted_message_reflects_recording_state() {
        let recording_active = AtomicBool::new(false);
        assert_eq!(
            accepted_message(IpcCommand::Record, &recording_active),
            "record start accepted: select a region to start recording"
        );

        recording_active.store(true, Ordering::SeqCst);
        assert_eq!(
            accepted_message(IpcCommand::Record, &recording_active),
            "record stop accepted: active recording will stop"
        );
    }

    #[test]
    fn command_accepted_messages_are_actionable() {
        let recording_active = AtomicBool::new(false);

        assert!(accepted_message(IpcCommand::Snip, &recording_active).contains("clipboard"));
        assert!(accepted_message(IpcCommand::Ocr, &recording_active).contains("clipboard"));
        assert!(accepted_message(IpcCommand::Shutdown, &recording_active).contains("stopping"));
    }
}

impl IpcResponse {
    pub fn ok(message: impl Into<String>) -> Self {
        Self::Ok {
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }

    pub fn to_json_line(&self) -> Result<String> {
        let mut line = serde_json::to_string(self).map_err(|error| {
            RsnipError::Message(format!("failed to serialize IPC response: {error}"))
        })?;
        line.push('\n');
        Ok(line)
    }

    pub fn from_json_line(line: &str) -> Result<Self> {
        serde_json::from_str(line.trim()).map_err(|error| {
            RsnipError::Message(format!(
                "failed to parse IPC response `{}`: {error}",
                line.trim()
            ))
        })
    }
}
