use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};

use crate::errors::{Result, RsnipError};
use crate::recording::{EffectiveRecordingConfig, RecordingTimingMetrics};
use crate::screen::capture::CaptureRegion;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegCommandSpec {
    pub executable: PathBuf,
    pub args: Vec<String>,
}

#[derive(Debug)]
pub struct FfmpegEncoder {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr_reader: Option<JoinHandle<String>>,
    output_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegEncoderResult {
    pub output_path: PathBuf,
    pub status: ExitStatus,
    pub stderr: String,
    pub timing: Option<RecordingTimingMetrics>,
}

impl FfmpegCommandSpec {
    pub fn new(
        region: CaptureRegion,
        config: &EffectiveRecordingConfig,
        output_path: &Path,
    ) -> Self {
        let executable = config
            .ffmpeg_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("ffmpeg.exe"));
        let args = vec![
            "-y".to_owned(),
            "-f".to_owned(),
            "rawvideo".to_owned(),
            "-pixel_format".to_owned(),
            "bgra".to_owned(),
            "-video_size".to_owned(),
            format!("{}x{}", region.width, region.height),
            "-framerate".to_owned(),
            config.fps.to_string(),
            "-i".to_owned(),
            "-".to_owned(),
            "-c:v".to_owned(),
            config.codec.clone(),
            "-crf".to_owned(),
            config.crf.to_string(),
            "-preset".to_owned(),
            config.preset.clone(),
            "-pix_fmt".to_owned(),
            "yuv420p".to_owned(),
            output_path.display().to_string(),
        ];

        Self { executable, args }
    }
}

impl FfmpegEncoder {
    pub fn spawn(
        region: CaptureRegion,
        config: &EffectiveRecordingConfig,
        output_path: PathBuf,
    ) -> Result<Self> {
        let spec = FfmpegCommandSpec::new(region, config, &output_path);
        let mut child = Command::new(&spec.executable)
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| {
                RsnipError::Message(format!(
                    "failed to start ffmpeg `{}`: {source}",
                    spec.executable.display()
                ))
            })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            RsnipError::Message("failed to open ffmpeg stdin for rawvideo frames".to_owned())
        })?;
        let stderr = child.stderr.take();
        let stderr_reader = stderr.map(|mut stderr| {
            thread::spawn(move || {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text);
                text
            })
        });

        Ok(Self {
            child,
            stdin: Some(stdin),
            stderr_reader,
            output_path,
        })
    }

    pub fn write_bgra_frame(&mut self, region: CaptureRegion, bgra: &[u8]) -> Result<()> {
        let expected_len = region.width as usize * region.height as usize * 4;
        if bgra.len() != expected_len {
            return Err(RsnipError::Message(format!(
                "invalid recording frame size: expected {expected_len} bytes, got {}",
                bgra.len()
            )));
        }

        let stdin = self.stdin.as_mut().ok_or_else(|| {
            RsnipError::Message("cannot write frame after ffmpeg stdin was closed".to_owned())
        })?;
        stdin.write_all(bgra)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<FfmpegEncoderResult> {
        drop(self.stdin.take());
        let status = self.child.wait()?;
        let stderr = match self.stderr_reader.take() {
            Some(reader) => reader.join().unwrap_or_default(),
            None => String::new(),
        };

        if !status.success() {
            return Err(RsnipError::Message(format!(
                "ffmpeg failed with status {status}: {stderr}"
            )));
        }

        Ok(FfmpegEncoderResult {
            output_path: self.output_path,
            status,
            stderr,
            timing: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::FfmpegCommandSpec;
    use crate::config::RecordingConfig;
    use crate::recording::EffectiveRecordingConfig;
    use crate::screen::capture::CaptureRegion;

    #[test]
    fn builds_ffmpeg_rawvideo_command() {
        let mut source = RecordingConfig::defaults_with_save_folder("C:/Videos".into());
        source.ffmpeg_path = Some("C:/Tools/ffmpeg.exe".into());
        let config = EffectiveRecordingConfig::from_config(&source).expect("valid config");
        let region = CaptureRegion::new(0, 0, 640, 480).expect("valid region");

        let spec = FfmpegCommandSpec::new(region, &config, "C:/Videos/out.mp4".as_ref());

        assert_eq!(spec.executable, PathBuf::from("C:/Tools/ffmpeg.exe"));
        assert!(
            spec.args
                .windows(2)
                .any(|pair| pair[0] == "-f" && pair[1] == "rawvideo")
        );
        assert!(
            spec.args
                .windows(2)
                .any(|pair| pair[0] == "-pixel_format" && pair[1] == "bgra")
        );
        assert!(
            spec.args
                .windows(2)
                .any(|pair| pair[0] == "-video_size" && pair[1] == "640x480")
        );
        assert!(
            spec.args
                .windows(2)
                .any(|pair| pair[0] == "-framerate" && pair[1] == "30")
        );
        assert_eq!(spec.args.last().expect("output path"), "C:/Videos/out.mp4");
    }
}
