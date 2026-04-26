use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::w;

use crate::errors::{Result, RsnipError};

const DAEMON_MUTEX_NAME: windows::core::PCWSTR = w!("Global\\RsnipDaemonMutex");

#[derive(Debug)]
pub struct SingleInstance {
    handle: HANDLE,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SingleInstanceStatus {
    Acquired,
    AlreadyRunning,
}

impl SingleInstance {
    pub fn acquire_daemon() -> Result<(Self, SingleInstanceStatus)> {
        // SAFETY: Passing a null security attributes pointer and a valid static wide string.
        // The returned HANDLE is owned by SingleInstance and closed in Drop.
        let handle = unsafe { CreateMutexW(None, false, DAEMON_MUTEX_NAME) }.map_err(|error| {
            RsnipError::Message(format!("failed to create daemon mutex: {error}"))
        })?;
        if handle.is_invalid() {
            return Err(RsnipError::Message(
                "CreateMutexW returned an invalid handle".to_owned(),
            ));
        }

        // SAFETY: GetLastError reads the thread-local Win32 last error immediately after CreateMutexW.
        let last_error = unsafe { GetLastError() };
        let status = if last_error == ERROR_ALREADY_EXISTS {
            SingleInstanceStatus::AlreadyRunning
        } else {
            SingleInstanceStatus::Acquired
        };

        Ok((Self { handle }, status))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: The handle is owned by this struct and is closed exactly once on drop.
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }
}
