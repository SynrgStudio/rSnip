use std::path::{Path, PathBuf};

use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::errors::{Result, RsnipError};

const APP_DIR_NAME: &str = "rsnip";
const CONFIG_FILE_NAME: &str = "rsnip.toml";
const LOG_FILE_NAME: &str = "rsnip.log";

pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or(RsnipError::MissingUserDirectory("config"))?;
    Ok(base.join(APP_DIR_NAME))
}

pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE_NAME))
}

pub fn data_dir() -> Result<PathBuf> {
    let base = dirs::data_local_dir().ok_or(RsnipError::MissingUserDirectory("local data"))?;
    Ok(base.join(APP_DIR_NAME))
}

pub fn log_file() -> Result<PathBuf> {
    Ok(data_dir()?.join(LOG_FILE_NAME))
}

pub fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(APP_DIR_NAME)
}

pub fn default_video_dir() -> Result<PathBuf> {
    dirs::video_dir().ok_or(RsnipError::MissingUserDirectory("videos"))
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}

pub fn recording_output_file(save_folder: &Path) -> Result<PathBuf> {
    ensure_dir(save_folder)?;
    Ok(save_folder.join(format!("Recording_{}.mp4", recording_timestamp())))
}

fn recording_timestamp() -> String {
    let local_time = unsafe { GetLocalTime() };

    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        local_time.wYear,
        local_time.wMonth,
        local_time.wDay,
        local_time.wHour,
        local_time.wMinute,
        local_time.wSecond
    )
}

pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}
