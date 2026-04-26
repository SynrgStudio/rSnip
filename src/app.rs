use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::errors::{Result, RsnipError};
use crate::ipc::IpcCommand;
use crate::overlay::selection::{
    Selection, SelectionCancelReason, SelectionFlags, SelectionMode, SelectionOutcome,
    SelectionRequest,
};
use crate::recording::{RecordingController, RecordingWorker};
use crate::screen::capture::CaptureRegion;
use crate::single_instance::{SingleInstance, SingleInstanceStatus};

const OVERLAY_OUTCOME_PREFIX: &str = "RSNIP_OVERLAY_OUTCOME_JSON=";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Daemon,
    Snip,
    Record,
    Ocr,
    Stop,
    Config,
    CaptureDebug,
    ClipboardTextDebug,
    ClipboardImageDebug,
    ClipboardFileDebug,
    OverlayDebug,
    OverlayOnce(SelectionMode),
    EditorDebug,
    EditorOnce(PathBuf),
    ToastDebug,
    WindowsDebug,
    Help,
}

#[derive(Debug)]
struct DaemonState {
    config: Config,
    recording: RecordingController,
    recording_worker: Option<RecordingWorker>,
    recording_overlay: Option<crate::overlay::recording::RecordingOverlayHandle>,
    recording_active: Arc<AtomicBool>,
}

#[derive(Debug, Deserialize)]
struct OverlayOutcomeDto {
    outcome: String,
    mode: Option<String>,
    region: Option<OverlayRegionDto>,
    shift_pressed: Option<bool>,
    window_handle: Option<isize>,
    reason: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OverlayRegionDto {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

pub fn run() -> Result<()> {
    let command = parse_command(std::env::args().skip(1))?;
    init_logging(&command)?;
    crate::screen::monitor::initialize_dpi_awareness()?;
    info!(?command, "starting rsnip command");
    run_command(command)
}

pub fn parse_command(args: impl IntoIterator<Item = String>) -> Result<Command> {
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Ok(Command::Help);
    };

    match command.as_str() {
        "daemon" => Ok(Command::Daemon),
        "snip" => Ok(Command::Snip),
        "record" => Ok(Command::Record),
        "ocr" => Ok(Command::Ocr),
        "stop" => Ok(Command::Stop),
        "config" => Ok(Command::Config),
        "capture-debug" | "debug-capture" => Ok(Command::CaptureDebug),
        "clipboard-text-debug" => Ok(Command::ClipboardTextDebug),
        "clipboard-image-debug" => Ok(Command::ClipboardImageDebug),
        "clipboard-file-debug" => Ok(Command::ClipboardFileDebug),
        "overlay-debug" | "selection-overlay-debug" => Ok(Command::OverlayDebug),
        "editor-debug" => Ok(Command::EditorDebug),
        "toast-debug" => Ok(Command::ToastDebug),
        "windows-debug" => Ok(Command::WindowsDebug),
        "__overlay-once" => {
            let Some(mode) = args.next() else {
                return Err(RsnipError::Message(
                    "__overlay-once requires mode snip, record, or ocr".to_owned(),
                ));
            };
            Ok(Command::OverlayOnce(parse_selection_mode(&mode)?))
        }
        "__editor-once" => {
            let Some(path) = args.next() else {
                return Err(RsnipError::Message(
                    "__editor-once requires an input image path".to_owned(),
                ));
            };
            Ok(Command::EditorOnce(PathBuf::from(path)))
        }
        "help" | "--help" | "-h" => Ok(Command::Help),
        other => Err(RsnipError::UnknownCommand(other.to_owned())),
    }
}

pub fn run_command(command: Command) -> Result<()> {
    match command {
        Command::Daemon => run_daemon(),
        Command::Snip => send_daemon_command(IpcCommand::Snip),
        Command::Record => send_daemon_command(IpcCommand::Record),
        Command::Ocr => send_daemon_command(IpcCommand::Ocr),
        Command::Stop => send_daemon_command(IpcCommand::Shutdown),
        Command::Config => print_config_path(),
        Command::CaptureDebug => run_capture_debug(),
        Command::ClipboardTextDebug => run_clipboard_text_debug(),
        Command::ClipboardImageDebug => run_clipboard_image_debug(),
        Command::ClipboardFileDebug => run_clipboard_file_debug(),
        Command::OverlayDebug => run_overlay_debug(),
        Command::OverlayOnce(mode) => run_overlay_once(mode),
        Command::EditorDebug => run_editor_debug(),
        Command::EditorOnce(path) => run_editor_once(path),
        Command::ToastDebug => run_toast_debug(),
        Command::WindowsDebug => run_windows_debug(),
        Command::Help => print_help(),
    }
}

fn init_logging(command: &Command) -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if matches!(command, Command::Daemon) {
        let log_path = crate::paths::log_file()?;
        crate::paths::ensure_parent_dir(&log_path)?;
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let make_writer = move || {
            log_file
                .try_clone()
                .expect("failed to clone rsnip daemon log file handle")
        };

        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(make_writer)
            .with_ansi(false)
            .init();
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();
    Ok(())
}

fn run_daemon() -> Result<()> {
    let (_single_instance, status) = SingleInstance::acquire_daemon()?;
    if status == SingleInstanceStatus::AlreadyRunning {
        warn!("rsnip daemon is already running");
        println!("rsnip daemon is already running");
        return Ok(());
    }

    let config_path = crate::paths::config_file()?;
    let config = Config::load_or_create(&config_path)?;
    info!(config = %config_path.display(), "daemon config loaded");
    println!("rsnip daemon running");
    println!("config: {}", config_path.display());
    println!("log: {}", crate::paths::log_file()?.display());

    let (sender, receiver) = mpsc::channel();
    let recording_active = Arc::new(AtomicBool::new(false));
    let _ipc_server = crate::ipc::start_named_pipe_server(sender.clone(), recording_active.clone());
    let _hotkeys = crate::hotkeys::start_hotkey_runtime(&config.hotkeys, sender)?;
    run_daemon_event_loop(receiver, config, recording_active)
}

fn run_daemon_event_loop(
    receiver: Receiver<IpcCommand>,
    config: Config,
    recording_active: Arc<AtomicBool>,
) -> Result<()> {
    let mut state = DaemonState {
        config,
        recording: RecordingController::new(),
        recording_worker: None,
        recording_overlay: None,
        recording_active,
    };
    info!("daemon event loop started");

    while let Ok(command) = receiver.recv() {
        if !dispatch_daemon_command(&mut state, command) {
            break;
        }
    }

    info!("daemon event loop stopped");
    Ok(())
}

fn dispatch_daemon_command(state: &mut DaemonState, command: IpcCommand) -> bool {
    match command {
        IpcCommand::Snip => {
            info!("snip action received");
            run_snip_selection_flow();
            true
        }
        IpcCommand::Record => {
            if state.recording.is_recording() || state.recording_active.load(Ordering::SeqCst) {
                if let Some(overlay) = state.recording_overlay.take() {
                    overlay.stop();
                }
                if let Some(worker) = state.recording_worker.take() {
                    match worker.stop() {
                        Ok(result) => {
                            let output_path = result.output_path.clone();
                            if let Some(timing) = &result.timing {
                                info!(
                                    output = %output_path.display(),
                                    status = %result.status,
                                    elapsed_ms = timing.elapsed.as_millis(),
                                    target_fps = timing.target_fps,
                                    frames_written = timing.frames_written,
                                    duplicate_frames = timing.duplicate_frames,
                                    effective_fps = timing.effective_fps(),
                                    "record_stop completed; recording_active=false"
                                );
                            } else {
                                info!(
                                    output = %output_path.display(),
                                    status = %result.status,
                                    "record_stop completed; recording_active=false"
                                );
                            }
                            if let Err(error) = crate::clipboard::copy_file(&output_path) {
                                warn!(%error, output = %output_path.display(), "failed to copy recording file to clipboard");
                                crate::overlay::toast::show_simple_toast_async(
                                    "RSnip",
                                    format!("No se pudo copiar el video: {error}"),
                                    crate::overlay::toast::ToastKind::Error,
                                );
                            } else {
                                crate::overlay::toast::show_toast_async(
                                    "RSnip",
                                    "Video guardado y copiado — Click para abrir ubicación",
                                    crate::overlay::toast::ToastKind::Info,
                                    crate::overlay::toast::ToastAction::OpenInExplorer(output_path),
                                );
                            }
                        }
                        Err(error) => {
                            state.recording.mark_failed(error.to_string());
                            warn!(%error, "record_stop failed");
                            crate::overlay::toast::show_simple_toast_async(
                                "RSnip",
                                format!("No se pudo guardar el video: {error}"),
                                crate::overlay::toast::ToastKind::Error,
                            );
                        }
                    }
                }
                match state.recording.stop_stub() {
                    Ok(Some(session)) => {
                        info!(output = %session.output_path.display(), "recording session closed")
                    }
                    Ok(None) => info!("record_stop requested with no active recording session"),
                    Err(error) => warn!(%error, "record_stop state update failed"),
                }
                state.recording_active.store(false, Ordering::SeqCst);
                return true;
            }

            info!("record_start selection action received");
            if let Some(SelectionOutcome::Selected(selection)) =
                run_daemon_selection_overlay(SelectionMode::Record)
            {
                match state
                    .recording
                    .start_stub(selection.region, &state.config.recording)
                {
                    Ok(session) => {
                        let worker = match RecordingWorker::start(session.clone()) {
                            Ok(worker) => worker,
                            Err(error) => {
                                state.recording.mark_failed(error.to_string());
                                state.recording_active.store(false, Ordering::SeqCst);
                                warn!(%error, "record_start failed");
                                crate::overlay::toast::show_simple_toast_async(
                                    "RSnip",
                                    format!("No se pudo iniciar la grabación: {error}"),
                                    crate::overlay::toast::ToastKind::Error,
                                );
                                return true;
                            }
                        };
                        match crate::overlay::recording::RecordingOverlayHandle::start(
                            session.region,
                            session.config.fps,
                        ) {
                            Ok(overlay) => {
                                let _ = overlay.update(
                                    crate::overlay::recording::RecordingOverlayStatus {
                                        elapsed: std::time::Duration::ZERO,
                                        fps: session.config.fps,
                                    },
                                );
                                state.recording_overlay = Some(overlay);
                            }
                            Err(error) => warn!(%error, "recording overlay failed to start"),
                        }
                        state.recording_worker = Some(worker);
                        state.recording_active.store(true, Ordering::SeqCst);
                        info!(
                            x = selection.region.x,
                            y = selection.region.y,
                            width = selection.region.width,
                            height = selection.region.height,
                            shift = selection.flags.shift_pressed,
                            fps = session.config.fps,
                            output = %session.output_path.display(),
                            "record_start session started"
                        );
                    }
                    Err(error) => {
                        state.recording.mark_failed(error.to_string());
                        state.recording_active.store(false, Ordering::SeqCst);
                        warn!(%error, "record_start stub failed");
                    }
                }
            }
            true
        }
        IpcCommand::Ocr => {
            info!("ocr action received");
            run_ocr_selection_flow(&state.config);
            true
        }
        IpcCommand::Shutdown => {
            info!("shutdown action received");
            if let Some(overlay) = state.recording_overlay.take() {
                overlay.stop();
            }
            if let Some(worker) = state.recording_worker.take() {
                match worker.stop() {
                    Ok(result) => {
                        if let Some(timing) = &result.timing {
                            info!(
                                output = %result.output_path.display(),
                                elapsed_ms = timing.elapsed.as_millis(),
                                frames_written = timing.frames_written,
                                duplicate_frames = timing.duplicate_frames,
                                effective_fps = timing.effective_fps(),
                                "recording stopped during shutdown"
                            )
                        } else {
                            info!(output = %result.output_path.display(), "recording stopped during shutdown")
                        }
                    }
                    Err(error) => warn!(%error, "failed to stop recording during shutdown"),
                }
            }
            state.recording_active.store(false, Ordering::SeqCst);
            false
        }
    }
}

fn run_ocr_selection_flow(config: &Config) {
    let started = Instant::now();
    match run_daemon_selection_overlay(SelectionMode::Ocr) {
        Some(SelectionOutcome::Selected(selection)) => {
            crate::overlay::toast::show_simple_toast_async(
                "RSnip OCR",
                "Extrayendo texto...",
                crate::overlay::toast::ToastKind::Info,
            );
            match extract_text_from_selection(selection, config) {
                Ok(crate::ocr::OcrTextResult::Text(text)) => {
                    if let Err(error) = crate::clipboard::copy_text(&text) {
                        warn!(%error, "ocr clipboard copy failed");
                        crate::overlay::toast::show_simple_toast_async(
                            "RSnip OCR",
                            format!("No se pudo copiar el texto: {error}"),
                            crate::overlay::toast::ToastKind::Error,
                        );
                    } else {
                        info!(
                            chars = text.chars().count(),
                            elapsed_ms = started.elapsed().as_millis(),
                            "ocr text copied to clipboard"
                        );
                        crate::overlay::toast::show_simple_toast_async(
                            "RSnip OCR",
                            "Texto extraído y copiado",
                            crate::overlay::toast::ToastKind::Info,
                        );
                    }
                }
                Ok(crate::ocr::OcrTextResult::NoText) => {
                    info!(
                        elapsed_ms = started.elapsed().as_millis(),
                        "ocr completed without clear text"
                    );
                    crate::overlay::toast::show_simple_toast_async(
                        "RSnip OCR",
                        "No se encontró texto claro",
                        crate::overlay::toast::ToastKind::Info,
                    );
                }
                Err(error) => {
                    warn!(%error, elapsed_ms = started.elapsed().as_millis(), "ocr failed");
                    crate::overlay::toast::show_simple_toast_async(
                        "RSnip OCR",
                        format!("Error OCR: {error}"),
                        crate::overlay::toast::ToastKind::Error,
                    );
                }
            }
        }
        Some(SelectionOutcome::Cancelled(reason)) => info!(?reason, "ocr selection cancelled"),
        Some(SelectionOutcome::Failed(message)) => warn!(%message, "ocr selection failed"),
        None => {}
    }
}

fn extract_text_from_selection(
    selection: Selection,
    config: &Config,
) -> Result<crate::ocr::OcrTextResult> {
    if let Some(handle) = selection.window_handle {
        let foreground_set = crate::screen::windows::foreground_window(handle);
        info!(
            hwnd = handle.0,
            foreground_set, "attempted foreground for OCR selected window"
        );
    }

    let capture = crate::screen::capture::capture_virtual_screen()?;
    let crop = capture.image.crop(selection.region)?;
    let image_path = crate::ocr::save_ocr_input_png(&crop)?;
    let result = crate::ocr::run_ocr(&image_path, &config.ocr);
    crate::ocr::remove_temp_file(&image_path);
    result
}

fn run_snip_selection_flow() {
    let started = Instant::now();
    match run_daemon_selection_overlay(SelectionMode::Snip) {
        Some(SelectionOutcome::Selected(selection)) => {
            if let Err(error) = copy_selected_region_for_snip(selection) {
                warn!(%error, elapsed_ms = started.elapsed().as_millis(), "snip copy failed");
                crate::overlay::toast::show_simple_toast_async(
                    "RSnip error",
                    format!("No se pudo copiar el recorte: {error}"),
                    crate::overlay::toast::ToastKind::Error,
                );
            }
        }
        Some(SelectionOutcome::Cancelled(reason)) => {
            info!(?reason, "snip selection cancelled");
        }
        Some(SelectionOutcome::Failed(message)) => {
            warn!(%message, "snip selection failed");
        }
        None => {}
    }
}

fn copy_selected_region_for_snip(selection: Selection) -> Result<()> {
    let started = Instant::now();
    let open_editor = selection.flags.shift_pressed;
    if let Some(handle) = selection.window_handle {
        let foreground_set = crate::screen::windows::foreground_window(handle);
        info!(
            hwnd = handle.0,
            foreground_set, "attempted foreground for selected window"
        );
    }
    let region = selection.region;
    let capture = crate::screen::capture::capture_virtual_screen()?;
    let crop = capture.image.crop(region)?;
    info!(
        x = crop.origin_x,
        y = crop.origin_y,
        width = crop.width,
        height = crop.height,
        bytes = crop.bgra.len(),
        "snip crop prepared"
    );
    if open_editor {
        let path = crate::editor::save_editor_input_png(&crop)?;
        info!(path = %path.display(), "opening editor for shift snip");
        run_editor_child(&path)?;
        return Ok(());
    }

    crate::clipboard::copy_image(&crop)?;
    let editor_path = crate::editor::save_editor_input_png(&crop)?;
    info!(
        x = crop.origin_x,
        y = crop.origin_y,
        width = crop.width,
        height = crop.height,
        elapsed_ms = started.elapsed().as_millis(),
        "snip copied to clipboard"
    );
    crate::overlay::toast::show_toast_async(
        "RSnip",
        format!(
            "Copiado al portapapeles ({}x{}) — Click para editar",
            crop.width, crop.height
        ),
        crate::overlay::toast::ToastKind::Info,
        crate::overlay::toast::ToastAction::OpenEditor(editor_path),
    );
    Ok(())
}

fn run_daemon_selection_overlay(mode: SelectionMode) -> Option<SelectionOutcome> {
    match run_selection_overlay_child(mode) {
        Ok(Some(outcome)) => {
            info!(?mode, ?outcome, "selection overlay child completed");
            Some(outcome)
        }
        Ok(None) => {
            info!(
                ?mode,
                "selection overlay child completed without selected outcome"
            );
            None
        }
        Err(error) => {
            warn!(?mode, %error, "selection overlay child failed");
            None
        }
    }
}

fn run_editor_child(path: &std::path::Path) -> Result<()> {
    let executable = std::env::current_exe()?;
    let status = ProcessCommand::new(executable)
        .arg("__editor-once")
        .arg(path)
        .status()?;
    if !status.success() {
        return Err(RsnipError::Message(format!(
            "editor child exited with status {status}"
        )));
    }
    Ok(())
}

fn run_selection_overlay_child(mode: SelectionMode) -> Result<Option<SelectionOutcome>> {
    let started = Instant::now();
    let executable = std::env::current_exe()?;
    let output = ProcessCommand::new(executable)
        .arg("__overlay-once")
        .arg(selection_mode_arg(mode))
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let elapsed = started.elapsed();
    info!(?mode, elapsed_ms = elapsed.as_millis(), status = %output.status, "selection overlay child exited");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RsnipError::Message(format!(
            "selection overlay child exited with status {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    parse_overlay_child_outcome(&stdout, mode)
}

fn parse_overlay_child_outcome(
    stdout: &str,
    expected_mode: SelectionMode,
) -> Result<Option<SelectionOutcome>> {
    let Some(payload) = stdout.lines().find_map(|line| {
        line.trim()
            .strip_prefix(OVERLAY_OUTCOME_PREFIX)
            .map(str::to_owned)
    }) else {
        return Ok(None);
    };

    let dto: OverlayOutcomeDto = serde_json::from_str(&payload).map_err(|error| {
        RsnipError::Message(format!(
            "failed to parse overlay child outcome JSON: {error}"
        ))
    })?;

    match dto.outcome.as_str() {
        "selected" => {
            let mode = dto
                .mode
                .as_deref()
                .map(parse_selection_mode)
                .transpose()?
                .unwrap_or(expected_mode);
            if mode != expected_mode {
                return Err(RsnipError::Message(format!(
                    "overlay child returned mode {:?}, expected {:?}",
                    mode, expected_mode
                )));
            }
            let region = dto.region.ok_or_else(|| {
                RsnipError::Message("overlay selected outcome missing region".to_owned())
            })?;
            Ok(Some(SelectionOutcome::Selected(Selection {
                mode,
                region: CaptureRegion::new(region.x, region.y, region.width, region.height)?,
                flags: SelectionFlags::new(dto.shift_pressed.unwrap_or(false)),
                window_handle: dto
                    .window_handle
                    .map(crate::screen::windows::SelectableWindowHandle),
            })))
        }
        "cancelled" => Ok(Some(SelectionOutcome::Cancelled(parse_cancel_reason(
            dto.reason.as_deref().unwrap_or("window_closed"),
        )?))),
        "failed" => Ok(Some(SelectionOutcome::Failed(
            dto.message
                .unwrap_or_else(|| "overlay child failed without message".to_owned()),
        ))),
        other => Err(RsnipError::Message(format!(
            "unknown overlay child outcome `{other}`"
        ))),
    }
}

fn selection_mode_arg(mode: SelectionMode) -> &'static str {
    match mode {
        SelectionMode::Snip => "snip",
        SelectionMode::Record => "record",
        SelectionMode::Ocr => "ocr",
    }
}

fn parse_selection_mode(mode: &str) -> Result<SelectionMode> {
    match mode {
        "snip" => Ok(SelectionMode::Snip),
        "record" => Ok(SelectionMode::Record),
        "ocr" => Ok(SelectionMode::Ocr),
        other => Err(RsnipError::Message(format!(
            "unknown overlay mode `{other}`; expected snip, record, or ocr"
        ))),
    }
}

fn cancel_reason_arg(reason: SelectionCancelReason) -> &'static str {
    match reason {
        SelectionCancelReason::Escape => "escape",
        SelectionCancelReason::WindowClosed => "window_closed",
        SelectionCancelReason::BelowThreshold => "below_threshold",
    }
}

fn parse_cancel_reason(reason: &str) -> Result<SelectionCancelReason> {
    match reason {
        "escape" => Ok(SelectionCancelReason::Escape),
        "window_closed" => Ok(SelectionCancelReason::WindowClosed),
        "below_threshold" => Ok(SelectionCancelReason::BelowThreshold),
        other => Err(RsnipError::Message(format!(
            "unknown overlay cancel reason `{other}`"
        ))),
    }
}

fn send_daemon_command(command: IpcCommand) -> Result<()> {
    let response = crate::ipc::send_command(command, Duration::from_millis(1_000))?;
    match response {
        crate::ipc::IpcResponse::Ok { message } => {
            println!("{message}");
            Ok(())
        }
        crate::ipc::IpcResponse::Error { message } => Err(RsnipError::Message(message)),
    }
}

fn print_config_path() -> Result<()> {
    println!("{}", crate::paths::config_file()?.display());
    Ok(())
}

fn run_capture_debug() -> Result<()> {
    let virtual_screen = crate::screen::monitor::VirtualScreen::current()?;
    let monitors = crate::screen::monitor::enumerate_monitors()?;
    println!(
        "virtual_screen: x={} y={} width={} height={}",
        virtual_screen.x, virtual_screen.y, virtual_screen.width, virtual_screen.height
    );
    println!("monitors: {}", monitors.len());
    for (index, monitor) in monitors.iter().enumerate() {
        println!(
            "monitor[{index}]: x={} y={} width={} height={} primary={}",
            monitor.bounds.x,
            monitor.bounds.y,
            monitor.bounds.width,
            monitor.bounds.height,
            monitor.primary
        );
    }

    let capture = crate::screen::capture::capture_virtual_screen()?;
    println!(
        "capture: origin={},{} size={}x{} bytes={} elapsed_ms={}",
        capture.image.origin_x,
        capture.image.origin_y,
        capture.image.width,
        capture.image.height,
        capture.image.bgra.len(),
        capture.metrics.elapsed.as_millis()
    );

    let crop_width = capture.image.width.min(128);
    let crop_height = capture.image.height.min(128);
    let crop = capture
        .image
        .crop(crate::screen::capture::CaptureRegion::new(
            capture.image.origin_x,
            capture.image.origin_y,
            crop_width,
            crop_height,
        )?)?;
    println!(
        "crop: origin={},{} size={}x{} bytes={}",
        crop.origin_x,
        crop.origin_y,
        crop.width,
        crop.height,
        crop.bgra.len()
    );

    Ok(())
}

fn run_clipboard_text_debug() -> Result<()> {
    crate::clipboard::copy_text("RSnip clipboard Unicode debug: hola, こんにちは, Привет")?;
    println!("copied debug Unicode text to clipboard");
    Ok(())
}

fn run_clipboard_image_debug() -> Result<()> {
    let capture = crate::screen::capture::capture_virtual_screen()?;
    let crop = capture
        .image
        .crop(crate::screen::capture::CaptureRegion::new(
            capture.image.origin_x,
            capture.image.origin_y,
            capture.image.width.min(512),
            capture.image.height.min(512),
        )?)?;
    crate::clipboard::copy_image(&crop)?;
    println!(
        "copied debug image crop to clipboard: {}x{}",
        crop.width, crop.height
    );
    Ok(())
}

fn run_clipboard_file_debug() -> Result<()> {
    let path = crate::paths::config_file()?;
    crate::clipboard::copy_file(&path)?;
    println!("copied debug file to clipboard: {}", path.display());
    Ok(())
}

fn run_overlay_debug() -> Result<()> {
    run_overlay_once(SelectionMode::Snip)
}

fn run_windows_debug() -> Result<()> {
    let virtual_screen = crate::screen::monitor::VirtualScreen::current()?;
    let windows = crate::screen::windows::enumerate_selectable_windows()?;
    println!(
        "virtual_screen: x={} y={} width={} height={}",
        virtual_screen.x, virtual_screen.y, virtual_screen.width, virtual_screen.height
    );
    println!("selectable_windows: {}", windows.len());
    for (index, window) in windows.iter().enumerate() {
        println!(
            "window[{index}]: kind={:?} hwnd={} x={} y={} width={} height={} title={}",
            window.kind,
            window.handle.0,
            window.bounds.x,
            window.bounds.y,
            window.bounds.width,
            window.bounds.height,
            window.title
        );
    }
    Ok(())
}

fn run_editor_debug() -> Result<()> {
    let capture = crate::screen::capture::capture_virtual_screen()?;
    let crop = capture
        .image
        .crop(crate::screen::capture::CaptureRegion::new(
            capture.image.origin_x,
            capture.image.origin_y,
            capture.image.width.min(640),
            capture.image.height.min(360),
        )?)?;
    let path = crate::editor::save_editor_input_png(&crop)?;
    let loaded = crate::editor::load_editor_input_png(&path)?;
    println!("prepared editor input: {}", path.display());
    println!(
        "editor input loaded: {}x{} bytes={}",
        loaded.width,
        loaded.height,
        loaded.rgba.len()
    );
    println!(
        "run `rsnip __editor-once {}` to validate child loading",
        path.display()
    );
    Ok(())
}

fn run_editor_once(path: PathBuf) -> Result<()> {
    let loaded = crate::editor::load_editor_input_png(&path)?;
    println!(
        "editor child loaded input: {}x{} bytes={}",
        loaded.width,
        loaded.height,
        loaded.rgba.len()
    );
    crate::editor::render::run_editor_shell(&path)?;
    println!(
        "RSNIP_EDITOR_OUTCOME_JSON={}",
        serde_json::json!({
            "outcome": "closed",
            "path": path.display().to_string(),
            "width": loaded.width,
            "height": loaded.height,
        })
    );
    Ok(())
}

fn run_toast_debug() -> Result<()> {
    use crate::overlay::toast::{
        ToastAction, ToastKind, show_simple_toast_async, show_toast_async,
    };

    show_simple_toast_async("RSnip", "Toast info de prueba", ToastKind::Info);
    std::thread::sleep(Duration::from_millis(120));
    show_simple_toast_async("RSnip", "Toast de error de prueba", ToastKind::Error);
    std::thread::sleep(Duration::from_millis(120));
    show_toast_async(
        "RSnip",
        "Toast final clickeable de prueba",
        ToastKind::Info,
        ToastAction::DebugLog,
    );
    println!("queued toast debug sequence");
    Ok(())
}

fn run_overlay_once(mode: SelectionMode) -> Result<()> {
    println!("opening selection overlay debug; press Escape or close the window to exit");
    let outcome =
        crate::overlay::selection::run_selection_overlay_shell(SelectionRequest::new(mode))?;
    println!("overlay outcome: {outcome:?}");
    println!(
        "{}{}",
        OVERLAY_OUTCOME_PREFIX,
        overlay_outcome_json(&outcome)
    );
    Ok(())
}

fn overlay_outcome_json(outcome: &SelectionOutcome) -> String {
    match outcome {
        SelectionOutcome::Selected(selection) => serde_json::json!({
            "outcome": "selected",
            "mode": selection_mode_arg(selection.mode),
            "region": {
                "x": selection.region.x,
                "y": selection.region.y,
                "width": selection.region.width,
                "height": selection.region.height,
            },
            "shift_pressed": selection.flags.shift_pressed,
            "window_handle": selection.window_handle.map(|handle| handle.0),
        })
        .to_string(),
        SelectionOutcome::Cancelled(reason) => serde_json::json!({
            "outcome": "cancelled",
            "reason": cancel_reason_arg(*reason),
        })
        .to_string(),
        SelectionOutcome::Failed(message) => serde_json::json!({
            "outcome": "failed",
            "message": message,
        })
        .to_string(),
    }
}

fn print_help() -> Result<()> {
    println!("rsnip <command>");
    println!();
    println!("Commands:");
    println!("  daemon   start daemon and register hotkeys");
    println!("  snip     trigger screen snip");
    println!("  record   start/stop region recording");
    println!("  ocr      trigger region OCR");
    println!("  stop     stop daemon");
    println!("  config   print config path");
    println!("  capture-debug   dev: capture virtual desktop and print metrics");
    println!("  clipboard-text-debug   dev: copy Unicode text to clipboard");
    println!("  clipboard-image-debug  dev: copy screen crop image to clipboard");
    println!("  clipboard-file-debug   dev: copy config file as CF_HDROP");
    println!("  overlay-debug          dev: open selection overlay shell");
    println!("  editor-debug           dev: prepare and verify editor PNG input");
    println!("  toast-debug            dev: show info/error/clickable toasts");
    println!("  windows-debug          dev: list selectable windows");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_selected_overlay_child_outcome() {
        let stdout = r#"
human log
RSNIP_OVERLAY_OUTCOME_JSON={"outcome":"selected","mode":"snip","region":{"x":10,"y":20,"width":30,"height":40},"shift_pressed":true}
"#;

        let outcome = parse_overlay_child_outcome(stdout, SelectionMode::Snip)
            .unwrap()
            .unwrap();

        assert_eq!(
            outcome,
            SelectionOutcome::Selected(Selection {
                mode: SelectionMode::Snip,
                region: CaptureRegion::new(10, 20, 30, 40).unwrap(),
                flags: SelectionFlags::new(true),
                window_handle: None,
            })
        );
    }

    #[test]
    fn parses_cancelled_overlay_child_outcome() {
        let stdout = "RSNIP_OVERLAY_OUTCOME_JSON={\"outcome\":\"cancelled\",\"reason\":\"escape\"}";

        let outcome = parse_overlay_child_outcome(stdout, SelectionMode::Snip)
            .unwrap()
            .unwrap();

        assert_eq!(
            outcome,
            SelectionOutcome::Cancelled(SelectionCancelReason::Escape)
        );
    }

    #[test]
    fn rejects_overlay_child_mode_mismatch() {
        let stdout = r#"RSNIP_OVERLAY_OUTCOME_JSON={"outcome":"selected","mode":"ocr","region":{"x":10,"y":20,"width":30,"height":40},"shift_pressed":false}"#;

        let error = parse_overlay_child_outcome(stdout, SelectionMode::Snip).unwrap_err();

        assert!(error.to_string().contains("expected Snip"));
    }
}
