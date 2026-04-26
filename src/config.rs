use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::{Result, RsnipError};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub hotkeys: HotkeyConfig,
    pub recording: RecordingConfig,
    pub ocr: OcrConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HotkeyConfig {
    pub snip: String,
    pub record: String,
    pub ocr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordingConfig {
    pub fps: u32,
    pub save_folder: PathBuf,
    pub codec: String,
    pub crf: u8,
    pub preset: String,
    pub ffmpeg_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcrConfig {
    pub tesseract_path: PathBuf,
    pub languages: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiConfig {
    pub toasts: bool,
    pub editor: bool,
}

impl Config {
    pub fn defaults_with_video_dir(video_dir: PathBuf) -> Self {
        Self {
            hotkeys: HotkeyConfig::default(),
            recording: RecordingConfig::defaults_with_save_folder(video_dir),
            ocr: OcrConfig::default(),
            ui: UiConfig::default(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&text).map_err(|source| RsnipError::ConfigParse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            return Self::load(path);
        }

        let config = Self::defaults_with_video_dir(crate::paths::default_video_dir()?);
        config.validate()?;
        crate::paths::ensure_parent_dir(path)?;
        std::fs::write(path, toml::to_string_pretty(&config)?)?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.recording.validate()?;
        self.ocr.validate()?;
        Ok(())
    }
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            snip: "ctrl+shift+s".to_owned(),
            record: "ctrl+shift+r".to_owned(),
            ocr: "ctrl+shift+e".to_owned(),
        }
    }
}

impl RecordingConfig {
    pub fn defaults_with_save_folder(save_folder: PathBuf) -> Self {
        Self {
            fps: 30,
            save_folder,
            codec: "libx264".to_owned(),
            crf: 26,
            preset: "veryfast".to_owned(),
            ffmpeg_path: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !(1..=240).contains(&self.fps) {
            return Err(RsnipError::InvalidConfig {
                field: "recording.fps",
                message: "must be between 1 and 240".to_owned(),
            });
        }

        if self.save_folder.as_os_str().is_empty() {
            return Err(RsnipError::InvalidConfig {
                field: "recording.save_folder",
                message: "must not be empty".to_owned(),
            });
        }

        if self.codec.trim().is_empty() {
            return Err(RsnipError::InvalidConfig {
                field: "recording.codec",
                message: "must not be empty".to_owned(),
            });
        }

        if self.crf > 51 {
            return Err(RsnipError::InvalidConfig {
                field: "recording.crf",
                message: "must be between 0 and 51 for libx264-compatible encoders".to_owned(),
            });
        }

        if self.preset.trim().is_empty() {
            return Err(RsnipError::InvalidConfig {
                field: "recording.preset",
                message: "must not be empty".to_owned(),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{OcrConfig, RecordingConfig};

    #[test]
    fn default_recording_config_is_valid() {
        let config = RecordingConfig::defaults_with_save_folder("C:/Videos".into());
        config
            .validate()
            .expect("default recording config is valid");
    }

    #[test]
    fn rejects_invalid_recording_fps() {
        let mut config = RecordingConfig::defaults_with_save_folder("C:/Videos".into());
        config.fps = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_invalid_recording_crf() {
        let mut config = RecordingConfig::defaults_with_save_folder("C:/Videos".into());
        config.crf = 52;
        assert!(config.validate().is_err());
    }

    #[test]
    fn default_ocr_config_is_valid() {
        OcrConfig::default().validate().expect("valid OCR config");
    }

    #[test]
    fn rejects_empty_ocr_languages() {
        let mut config = OcrConfig::default();
        config.languages = "  ".to_owned();
        assert!(config.validate().is_err());
    }
}

impl OcrConfig {
    pub fn validate(&self) -> Result<()> {
        if self.tesseract_path.as_os_str().is_empty() {
            return Err(RsnipError::InvalidConfig {
                field: "ocr.tesseract_path",
                message: "must not be empty".to_owned(),
            });
        }

        if self.languages.trim().is_empty() {
            return Err(RsnipError::InvalidConfig {
                field: "ocr.languages",
                message: "must not be empty".to_owned(),
            });
        }

        Ok(())
    }
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            tesseract_path: PathBuf::from(r"C:\Program Files\Tesseract-OCR\tesseract.exe"),
            languages: "spa+eng".to_owned(),
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            toasts: true,
            editor: true,
        }
    }
}
