pub mod cursor;
pub mod encoder;

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::RecordingConfig;
use crate::errors::{Result, RsnipError};
use crate::screen::capture::CaptureRegion;

use self::encoder::{FfmpegEncoder, FfmpegEncoderResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRecordingConfig {
    pub fps: u32,
    pub codec: String,
    pub crf: u8,
    pub preset: String,
    pub ffmpeg_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingSession {
    pub region: CaptureRegion,
    pub output_path: PathBuf,
    pub config: EffectiveRecordingConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingLifecycle {
    Idle,
    Recording,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecordingState {
    Idle,
    Recording(RecordingSession),
    Stopping(RecordingSession),
    Failed(String),
}

#[derive(Debug)]
pub struct RecordingController {
    state: Mutex<RecordingState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingTimingMetrics {
    pub target_fps: u32,
    pub elapsed: Duration,
    pub frames_written: u64,
    pub duplicate_frames: u64,
}

#[derive(Debug)]
pub struct RecordingWorker {
    stop_sender: Sender<()>,
    join_handle: JoinHandle<Result<FfmpegEncoderResult>>,
}

impl EffectiveRecordingConfig {
    pub fn from_config(config: &RecordingConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            fps: config.fps,
            codec: config.codec.clone(),
            crf: config.crf,
            preset: config.preset.clone(),
            ffmpeg_path: config.ffmpeg_path.clone(),
        })
    }
}

impl RecordingWorker {
    pub fn start(session: RecordingSession) -> Result<Self> {
        let encoder =
            FfmpegEncoder::spawn(session.region, &session.config, session.output_path.clone())?;
        let (stop_sender, stop_receiver) = mpsc::channel();
        let join_handle =
            thread::spawn(move || run_recording_worker(session, encoder, stop_receiver));
        Ok(Self {
            stop_sender,
            join_handle,
        })
    }

    pub fn stop(self) -> Result<FfmpegEncoderResult> {
        let _ = self.stop_sender.send(());
        self.join_handle
            .join()
            .map_err(|_| RsnipError::Message("recording worker panicked".to_owned()))?
    }
}

fn run_recording_worker(
    session: RecordingSession,
    mut encoder: FfmpegEncoder,
    stop_receiver: Receiver<()>,
) -> Result<FfmpegEncoderResult> {
    let target_fps = session.config.fps.max(1);
    let started_at = Instant::now();
    let mut frames_written = 0;
    let mut duplicate_frames = 0;
    let mut last_frame: Option<Vec<u8>> = None;

    loop {
        if stop_receiver.try_recv().is_ok() {
            break;
        }

        let elapsed = started_at.elapsed();
        let expected_frames = expected_frame_count(elapsed, target_fps).max(1);
        if frames_written >= expected_frames {
            if wait_until_next_frame_or_stop(&stop_receiver, started_at, frames_written, target_fps)
            {
                break;
            }
            continue;
        }

        let duplicates_before_capture = frames_to_duplicate_before_capture(
            frames_written,
            expected_frames,
            last_frame.is_some(),
        );
        for _ in 0..duplicates_before_capture {
            if let Some(frame) = &last_frame {
                encoder.write_bgra_frame(session.region, frame)?;
                frames_written += 1;
                duplicate_frames += 1;
            }
        }

        let capture = crate::screen::capture::capture_region(session.region)?;
        let mut image = capture.image;
        let _ = crate::recording::cursor::draw_current_cursor_on_bgra_frame(
            &mut image.bgra,
            image.width,
            image.height,
            session.region,
        )?;
        encoder.write_bgra_frame(session.region, &image.bgra)?;
        frames_written += 1;
        last_frame = Some(image.bgra);
    }

    let elapsed = started_at.elapsed();
    let final_expected_frames = expected_frame_count(elapsed, target_fps);
    let final_padding =
        final_padding_frames(frames_written, final_expected_frames, last_frame.is_some());
    for _ in 0..final_padding {
        if let Some(frame) = &last_frame {
            encoder.write_bgra_frame(session.region, frame)?;
            frames_written += 1;
            duplicate_frames += 1;
        }
    }

    let metrics = RecordingTimingMetrics {
        target_fps,
        elapsed,
        frames_written,
        duplicate_frames,
    };
    let mut result = encoder.finish()?;
    result.timing = Some(metrics);
    Ok(result)
}

impl RecordingTimingMetrics {
    pub fn effective_fps(&self) -> f64 {
        let elapsed = self.elapsed.as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }
        self.frames_written as f64 / elapsed
    }
}

fn expected_frame_count(elapsed: Duration, fps: u32) -> u64 {
    if elapsed.is_zero() {
        return 0;
    }
    let fps = u128::from(fps.max(1));
    let nanos = elapsed.as_nanos();
    let frames = (nanos * fps).div_ceil(1_000_000_000);
    frames.min(u128::from(u64::MAX)) as u64
}

fn target_frame_instant(started_at: Instant, frames_written: u64, fps: u32) -> Instant {
    let next_frame = frames_written.saturating_add(1);
    let delay = Duration::from_secs_f64(next_frame as f64 / f64::from(fps.max(1)));
    started_at + delay
}

fn wait_until_next_frame_or_stop(
    stop_receiver: &Receiver<()>,
    started_at: Instant,
    frames_written: u64,
    fps: u32,
) -> bool {
    let now = Instant::now();
    let target = target_frame_instant(started_at, frames_written, fps);
    if target <= now {
        return false;
    }
    stop_receiver.recv_timeout(target - now).is_ok()
}

fn frames_to_duplicate_before_capture(
    frames_written: u64,
    expected_frames: u64,
    has_last_frame: bool,
) -> u64 {
    if !has_last_frame {
        return 0;
    }
    expected_frames.saturating_sub(frames_written.saturating_add(1))
}

fn final_padding_frames(frames_written: u64, expected_frames: u64, has_last_frame: bool) -> u64 {
    if !has_last_frame {
        return 0;
    }
    expected_frames.saturating_sub(frames_written)
}

fn even_sized_recording_region(region: CaptureRegion) -> Result<CaptureRegion> {
    let width = region.width - (region.width % 2);
    let height = region.height - (region.height % 2);
    if width < 2 || height < 2 {
        return Err(RsnipError::Message(format!(
            "recording region is too small after encoder alignment: {}x{} at {},{}",
            region.width, region.height, region.x, region.y
        )));
    }
    CaptureRegion::new(region.x, region.y, width, height)
}

impl RecordingController {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RecordingState::Idle),
        }
    }

    pub fn lifecycle(&self) -> RecordingLifecycle {
        match &*self.lock_state() {
            RecordingState::Idle => RecordingLifecycle::Idle,
            RecordingState::Recording(_) => RecordingLifecycle::Recording,
            RecordingState::Stopping(_) => RecordingLifecycle::Stopping,
            RecordingState::Failed(_) => RecordingLifecycle::Failed,
        }
    }

    pub fn is_recording(&self) -> bool {
        matches!(self.lifecycle(), RecordingLifecycle::Recording)
    }

    pub fn start_stub(
        &self,
        region: CaptureRegion,
        config: &RecordingConfig,
    ) -> Result<RecordingSession> {
        let effective_config = EffectiveRecordingConfig::from_config(config)?;
        let region = even_sized_recording_region(region)?;
        let output_path = crate::paths::recording_output_file(&config.save_folder)?;
        let session = RecordingSession {
            region,
            output_path,
            config: effective_config,
        };

        let mut state = self.lock_state();
        match &*state {
            RecordingState::Idle | RecordingState::Failed(_) => {
                *state = RecordingState::Recording(session.clone());
                Ok(session)
            }
            RecordingState::Recording(_) | RecordingState::Stopping(_) => Err(RsnipError::Message(
                "recording is already active or stopping".to_owned(),
            )),
        }
    }

    pub fn stop_stub(&self) -> Result<Option<RecordingSession>> {
        let mut state = self.lock_state();
        match &*state {
            RecordingState::Recording(session) => {
                let session = session.clone();
                *state = RecordingState::Stopping(session.clone());
                *state = RecordingState::Idle;
                Ok(Some(session))
            }
            RecordingState::Stopping(session) => Ok(Some(session.clone())),
            RecordingState::Idle | RecordingState::Failed(_) => Ok(None),
        }
    }

    pub fn mark_failed(&self, message: String) {
        *self.lock_state() = RecordingState::Failed(message);
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, RecordingState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for RecordingController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{RecordingController, RecordingLifecycle, RecordingTimingMetrics};
    use crate::config::RecordingConfig;
    use crate::screen::capture::CaptureRegion;

    #[test]
    fn recording_controller_transitions_start_and_stop() {
        let controller = RecordingController::new();
        let config = RecordingConfig::defaults_with_save_folder(std::env::temp_dir());
        let region = CaptureRegion::new(10, 20, 300, 200).expect("valid region");

        let session = controller
            .start_stub(region, &config)
            .expect("recording starts");
        assert_eq!(controller.lifecycle(), RecordingLifecycle::Recording);
        assert_eq!(session.region, region);
        assert!(
            session
                .output_path
                .ends_with(session.output_path.file_name().unwrap())
        );

        let stopped = controller.stop_stub().expect("recording stops");
        assert_eq!(stopped.expect("active session returned").region, region);
        assert_eq!(controller.lifecycle(), RecordingLifecycle::Idle);
    }

    #[test]
    fn recording_controller_rejects_double_start() {
        let controller = RecordingController::new();
        let config = RecordingConfig::defaults_with_save_folder(std::env::temp_dir());
        let region = CaptureRegion::new(0, 0, 100, 100).expect("valid region");

        controller
            .start_stub(region, &config)
            .expect("first start succeeds");
        assert!(controller.start_stub(region, &config).is_err());
    }

    #[test]
    fn recording_region_is_aligned_to_even_dimensions_for_yuv420p() {
        let region = CaptureRegion::new(10, 20, 121, 95).expect("valid region");
        let aligned = super::even_sized_recording_region(region).expect("aligned region");
        assert_eq!(aligned.x, 10);
        assert_eq!(aligned.y, 20);
        assert_eq!(aligned.width, 120);
        assert_eq!(aligned.height, 94);
    }

    #[test]
    fn expected_frame_count_uses_wall_clock_without_drift() {
        assert_eq!(super::expected_frame_count(Duration::ZERO, 30), 0);
        assert_eq!(super::expected_frame_count(Duration::from_millis(1), 30), 1);
        assert_eq!(super::expected_frame_count(Duration::from_secs(5), 30), 150);
        assert_eq!(
            super::expected_frame_count(Duration::from_millis(5_001), 30),
            151
        );
    }

    #[test]
    fn duplicate_count_leaves_room_for_fresh_capture() {
        assert_eq!(super::frames_to_duplicate_before_capture(10, 15, true), 4);
        assert_eq!(super::frames_to_duplicate_before_capture(10, 11, true), 0);
        assert_eq!(super::frames_to_duplicate_before_capture(10, 15, false), 0);
    }

    #[test]
    fn final_padding_duplicates_until_expected_frame_count() {
        assert_eq!(super::final_padding_frames(148, 150, true), 2);
        assert_eq!(super::final_padding_frames(150, 150, true), 0);
        assert_eq!(super::final_padding_frames(0, 5, false), 0);
    }

    #[test]
    fn timing_metrics_report_effective_fps() {
        let metrics = RecordingTimingMetrics {
            target_fps: 30,
            elapsed: Duration::from_secs(5),
            frames_written: 150,
            duplicate_frames: 3,
        };
        assert_eq!(metrics.effective_fps(), 30.0);
    }
}
