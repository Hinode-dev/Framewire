//! Tauri-based GUI: a small HTML/CSS/JS settings window (`gui/`) driving the
//! same capture -> encode -> send pipeline as the headless CLI path.
//!
//! Per-session UI state (selected target/window, fps, bitrate, etc.) lives
//! in the HTML form itself, not here — the frontend just sends resolved
//! values to [`start_streaming`]. Live progress is pushed to the frontend
//! via a `host-status` event emitted roughly twice a second (matching the
//! previous egui build's `request_repaint_after` cadence), rather than
//! polled from JS.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};

use crate::{capture, run_pipeline, Args, HostStatus, SfuMode, SharedStatus};

/// Thumbnails are for a small picker card, not full quality — bound the
/// longer side so capture + base64 + IPC stay cheap.
const THUMBNAIL_MAX_DIM: u32 = 240;

const CAPTURE_WARNING: &str = concat!(
    "Barely any frames are being captured. ",
    "The capture target might just be static (moving it should recover). ",
    "OpenGL/Vulkan titles in exclusive fullscreen can't reach full frame rate ",
    "due to DXGI limitations — try borderless windowed mode, or switch streaming mode.",
);

struct AppState {
    base_args: Args,
    status: SharedStatus,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// When streaming started; used to exclude the initial warm-up period
    /// from the capture-failure warning check.
    running_since: Option<Instant>,
}

impl AppState {
    fn new(base_args: Args) -> Self {
        Self {
            base_args,
            status: Arc::new(Mutex::new(HostStatus::default())),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
            running_since: None,
        }
    }

    fn is_running(&self) -> bool {
        self.status.lock().map(|s| s.running).unwrap_or(false) || self.worker.is_some()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureTargetDto {
    adapter_index: u32,
    adapter_name: String,
    output_index: u32,
    output_name: String,
}

impl From<capture::CaptureTarget> for CaptureTargetDto {
    fn from(t: capture::CaptureTarget) -> Self {
        Self {
            adapter_index: t.adapter_index,
            adapter_name: t.adapter_name,
            output_index: t.output_index,
            output_name: t.output_name,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowTargetDto {
    // Serialized as a string: an isize HWND round-tripped through a JSON
    // number risks silent precision loss, and the frontend only ever echoes
    // this value back verbatim.
    hwnd: String,
    title: String,
}

impl From<capture::WindowTarget> for WindowTargetDto {
    fn from(w: capture::WindowTarget) -> Self {
        Self {
            hwnd: w.hwnd.to_string(),
            title: w.title,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DefaultsDto {
    fps_choices: [u32; 5],
    default_fps: u32,
    default_bitrate_mbps: u32,
    default_use_mesh: bool,
    default_public_host: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartSettingsDto {
    /// "monitor" or "window".
    capture_mode: String,
    adapter_index: u32,
    output_index: u32,
    window_hwnd: Option<String>,
    fps: u32,
    bitrate_mbps: u32,
    use_mesh: bool,
    public_host: String,
    host_token: String,
}

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct HostStatusDto {
    running: bool,
    room_code: String,
    viewer_url: String,
    measured_fps: f64,
    width: u32,
    height: u32,
    current_bitrate_bps: u32,
    error: Option<String>,
    capture_warning: Option<String>,
}

#[tauri::command]
fn list_monitor_targets() -> Result<Vec<CaptureTargetDto>, String> {
    capture::list_targets()
        .map(|targets| targets.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_window_targets() -> Result<Vec<WindowTargetDto>, String> {
    capture::list_windows()
        .map(|windows| windows.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

fn to_data_url(png: Vec<u8>) -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

#[tauri::command]
fn capture_monitor_thumbnail(adapter_index: u32, output_index: u32) -> Result<String, String> {
    capture::thumbnail::capture_monitor_thumbnail(adapter_index, output_index, THUMBNAIL_MAX_DIM)
        .map(to_data_url)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn capture_window_thumbnail(hwnd: String) -> Result<String, String> {
    let hwnd: isize = hwnd.parse().map_err(|_| "invalid window handle".to_string())?;
    capture::thumbnail::capture_window_thumbnail(hwnd, THUMBNAIL_MAX_DIM)
        .map(to_data_url)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_defaults(state: tauri::State<'_, Mutex<AppState>>) -> DefaultsDto {
    let s = state.lock().unwrap();
    DefaultsDto {
        fps_choices: [60, 120, 144, 165, 240],
        default_fps: s.base_args.fps,
        default_bitrate_mbps: (s.base_args.bitrate_bps / 1_000_000).max(1),
        default_use_mesh: matches!(s.base_args.sfu, SfuMode::Mesh),
        default_public_host: s.base_args.public_host.clone(),
    }
}

#[tauri::command]
fn start_streaming(
    state: tauri::State<'_, Mutex<AppState>>,
    settings: StartSettingsDto,
) -> Result<(), String> {
    let mut s = state.lock().unwrap();
    if s.is_running() {
        return Err("already streaming".to_string());
    }

    let mut args = s.base_args.clone();
    if settings.capture_mode == "window" {
        args.window_hwnd = settings
            .window_hwnd
            .as_deref()
            .and_then(|h| h.parse::<isize>().ok());
    } else {
        args.adapter_index = settings.adapter_index;
        args.output_index = settings.output_index;
        args.window_hwnd = None;
    }
    args.fps = settings.fps;
    args.bitrate_bps = settings.bitrate_mbps * 1_000_000;
    args.sfu = if settings.use_mesh {
        SfuMode::Mesh
    } else {
        SfuMode::Direct
    };
    args.public_host = settings.public_host;
    args.host_token = settings.host_token;

    *s.status.lock().unwrap() = HostStatus::default();
    s.stop.store(false, Ordering::SeqCst);

    let status = s.status.clone();
    let stop = s.stop.clone();
    s.worker = Some(std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                if let Ok(mut st) = status.lock() {
                    st.error = Some(format!("failed to create tokio runtime: {e}"));
                }
                return;
            }
        };
        if let Err(e) = rt.block_on(run_pipeline(args, status.clone(), stop)) {
            if let Ok(mut st) = status.lock() {
                st.error = Some(e.to_string());
                st.running = false;
            }
        }
    }));
    s.running_since = Some(Instant::now());
    Ok(())
}

#[tauri::command]
fn stop_streaming(state: tauri::State<'_, Mutex<AppState>>) -> Result<(), String> {
    let mut s = state.lock().unwrap();
    s.stop.store(true, Ordering::SeqCst);
    // The worker exits promptly once it observes `stop`; don't block on
    // joining it here.
    s.worker = None;
    s.running_since = None;
    if let Ok(mut st) = s.status.lock() {
        st.running = false;
    }
    Ok(())
}

#[tauri::command]
fn copy_to_clipboard(text: String) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .map_err(|e| e.to_string())
}

/// If fps stays near zero a few seconds after streaming starts, assume
/// capture isn't actually working and surface a warning. There's no
/// reliable way to directly detect the game's graphics API, so this
/// symptom-based check stands in for it.
fn capture_warning(running_since: Option<Instant>, measured_fps: f64) -> Option<String> {
    let elapsed = running_since?.elapsed();
    if elapsed > Duration::from_secs(5) && measured_fps < 5.0 {
        Some(CAPTURE_WARNING.to_string())
    } else {
        None
    }
}

fn spawn_status_emitter(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let state = app.state::<Mutex<AppState>>();
            let dto = {
                let s = state.lock().unwrap();
                let status = s.status.lock().unwrap().clone();
                let warning = capture_warning(s.running_since, status.measured_fps);
                HostStatusDto {
                    running: status.running,
                    room_code: status.room_code,
                    viewer_url: status.viewer_url,
                    measured_fps: status.measured_fps,
                    width: status.width,
                    height: status.height,
                    current_bitrate_bps: status.current_bitrate_bps,
                    error: status.error,
                    capture_warning: warning,
                }
            };
            let _ = app.emit("host-status", dto);
        }
    });
}

pub fn run(args: Args) -> anyhow::Result<()> {
    tauri::Builder::default()
        .manage(Mutex::new(AppState::new(args)))
        .invoke_handler(tauri::generate_handler![
            list_monitor_targets,
            list_window_targets,
            capture_monitor_thumbnail,
            capture_window_thumbnail,
            get_defaults,
            start_streaming,
            stop_streaming,
            copy_to_clipboard,
        ])
        .setup(|app| {
            spawn_status_emitter(&app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("failed to start GUI: {e}"))
}
