use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitStatus};
use std::time::{SystemTime, UNIX_EPOCH};

use image::{ImageBuffer, Rgba, RgbaImage};

use crate::config::OcrConfig;
use crate::errors::{Result, RsnipError};
use crate::screen::capture::CapturedImage;

const TESSERACT_INSTALL_URL: &str = "https://github.com/UB-Mannheim/tesseract/releases/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrRequest {
    pub image_path: PathBuf,
    pub tesseract_path: PathBuf,
    pub languages: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcrTextResult {
    Text(String),
    NoText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TesseractOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

pub fn resolve_tesseract(config: &OcrConfig) -> Result<PathBuf> {
    config.validate()?;
    if config.tesseract_path.is_file() {
        return Ok(config.tesseract_path.clone());
    }

    Err(RsnipError::Message(format!(
        "Tesseract OCR no detectado en `{}`. Instalar desde {TESSERACT_INSTALL_URL} o actualizar ocr.tesseract_path en rsnip.toml",
        config.tesseract_path.display()
    )))
}

pub fn verify_tesseract(config: &OcrConfig) -> Result<PathBuf> {
    let tesseract_path = resolve_tesseract(config)?;
    let output = ProcessCommand::new(&tesseract_path)
        .arg("--version")
        .output()
        .map_err(|error| {
            RsnipError::Message(format!(
                "failed to run Tesseract `{}`: {error}. Install from {TESSERACT_INSTALL_URL}",
                tesseract_path.display()
            ))
        })?;

    if !output.status.success() {
        return Err(RsnipError::Message(format!(
            "Tesseract `{}` failed version check: {}",
            tesseract_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    Ok(tesseract_path)
}

pub fn save_ocr_input_png(image: &CapturedImage) -> Result<PathBuf> {
    let path = next_ocr_temp_path()?;
    crate::paths::ensure_parent_dir(&path)?;
    let rgba = bgra_to_rgba(&image.bgra)?;
    let buffer: RgbaImage =
        ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(image.width, image.height, rgba).ok_or_else(
            || {
                RsnipError::Message(format!(
                    "failed to build OCR PNG buffer for {}x{} image",
                    image.width, image.height
                ))
            },
        )?;
    buffer
        .save(&path)
        .map_err(|error| RsnipError::Message(format!("failed to save OCR PNG: {error}")))?;
    Ok(path)
}

pub fn run_ocr(image_path: &Path, config: &OcrConfig) -> Result<OcrTextResult> {
    let tesseract_path = verify_tesseract(config)?;
    let request = OcrRequest {
        image_path: image_path.to_path_buf(),
        tesseract_path,
        languages: config.languages.clone(),
    };
    run_ocr_request(&request)
}

pub fn run_ocr_request(request: &OcrRequest) -> Result<OcrTextResult> {
    let output = ProcessCommand::new(&request.tesseract_path)
        .arg(&request.image_path)
        .arg("stdout")
        .arg("-l")
        .arg(&request.languages)
        .output()
        .map_err(|error| {
            RsnipError::Message(format!(
                "failed to run Tesseract `{}`: {error}",
                request.tesseract_path.display()
            ))
        })?;

    parse_tesseract_output(TesseractOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn parse_tesseract_output(output: TesseractOutput) -> Result<OcrTextResult> {
    if !output.status.success() {
        return Err(RsnipError::Message(format!(
            "Tesseract OCR failed: {}",
            output.stderr.trim()
        )));
    }

    let text = normalize_ocr_text(&output.stdout);
    if text.is_empty() {
        return Ok(OcrTextResult::NoText);
    }
    Ok(OcrTextResult::Text(text))
}

fn normalize_ocr_text(text: &str) -> String {
    text.trim().to_owned()
}

pub fn remove_temp_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn next_ocr_temp_path() -> Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| RsnipError::Message(format!("system time before UNIX_EPOCH: {error}")))?
        .as_nanos();
    Ok(crate::paths::temp_dir().join(format!("rsnip-ocr-{}-{timestamp}.png", std::process::id())))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn success_status() -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[cfg(windows)]
    fn failure_status() -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(1)
    }

    #[test]
    fn normalizes_ocr_text_by_trimming_outer_whitespace() {
        assert_eq!(normalize_ocr_text("  hola\n mundo\n\n"), "hola\n mundo");
    }

    #[test]
    fn parses_empty_success_as_no_text() {
        let result = parse_tesseract_output(TesseractOutput {
            status: success_status(),
            stdout: "  \n\t".to_owned(),
            stderr: String::new(),
        })
        .expect("successful no-text result");
        assert_eq!(result, OcrTextResult::NoText);
    }

    #[test]
    fn parses_text_success_as_text() {
        let result = parse_tesseract_output(TesseractOutput {
            status: success_status(),
            stdout: " texto extraido \n".to_owned(),
            stderr: String::new(),
        })
        .expect("successful text result");
        assert_eq!(result, OcrTextResult::Text("texto extraido".to_owned()));
    }

    #[test]
    fn parses_failed_status_as_error() {
        let result = parse_tesseract_output(TesseractOutput {
            status: failure_status(),
            stdout: String::new(),
            stderr: "missing language data".to_owned(),
        });
        assert!(result.is_err());
    }
}
