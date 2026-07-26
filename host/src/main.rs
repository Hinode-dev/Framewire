// No console window on launch — regular users double-click framewire.exe
// and get the GUI; the log output means nothing to them. A console
// already attached (e.g. running from a terminal, or --headless in CI)
// is inherited independently of this and keeps working as before.
#![windows_subsystem = "windows"]

mod capture;
mod debug_dump;
mod encode;
mod shaders;
mod transport;
mod upnp;

mod tauri_app;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use capture::CaptureFrame;

/// State shared between the GUI and the streaming pipeline: the pipeline
/// updates it, the GUI displays it.
#[derive(Default, Clone)]
pub struct HostStatus {
    pub running: bool,
    pub room_code: String,
    pub viewer_url: String,
    pub measured_fps: f64,
    pub width: u32,
    pub height: u32,
    /// Current NVENC target bitrate from bandwidth adaptation.
    pub current_bitrate_bps: u32,
    pub error: Option<String>,
}

pub type SharedStatus = Arc<Mutex<HostStatus>>;

// Windows' default timer resolution (~15.6ms) is too coarse for accurate
// frame pacing at high fps; raise it to 1ms.
#[cfg(windows)]
#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(uPeriod: u32) -> u32;
}

pub struct Args {
    fps: u32,
    port: u16,
    bitrate_bps: u32,
    adapter_index: u32,
    output_index: u32,
    /// Raw `HWND` of a specific window to capture instead of a whole
    /// monitor. `None` (the default) captures the monitor identified by
    /// `adapter_index`/`output_index`.
    window_hwnd: Option<isize>,
    capture_backend: capture::Backend,
    /// Direct HTTP signaling ("direct") vs. P2P mesh signaling ("mesh").
    sfu: SfuMode,
    /// Base HTTP URL of `backend/`. In mesh mode this is rewritten to a
    /// `ws(s)://` URL for the signaling WebSocket connection.
    backend_url: String,
    /// This machine's address as seen by viewer browsers (e.g. a LAN IP).
    public_host: String,
    /// Skip the GUI and start streaming immediately from the CLI.
    headless: bool,
    /// STUN server (`host:port`) used for ICE candidate gathering. There is
    /// no TURN fallback: a peer that can't connect via P2P simply fails.
    stun_server: String,
}

impl Clone for Args {
    fn clone(&self) -> Self {
        Args {
            fps: self.fps,
            port: self.port,
            bitrate_bps: self.bitrate_bps,
            adapter_index: self.adapter_index,
            output_index: self.output_index,
            window_hwnd: self.window_hwnd,
            capture_backend: self.capture_backend,
            sfu: self.sfu,
            backend_url: self.backend_url.clone(),
            public_host: self.public_host.clone(),
            headless: self.headless,
            stun_server: self.stun_server.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SfuMode {
    Direct,
    /// P2P mesh: signals through `backend/` over WebSocket and opens a
    /// separate `RTCPeerConnection` per viewer.
    Mesh,
}

fn parse_args() -> Args {
    let mut fps = 60u32;
    let mut port = 8787u16;
    let mut bitrate_bps = 20_000_000u32;
    let mut adapter_index = 0u32;
    let mut output_index = 0u32;
    let mut window_hwnd: Option<isize> = None;
    let mut capture_backend = capture::Backend::Auto;
    let mut sfu = SfuMode::Mesh;
    let backend_url = "https://framewire.hinodeent.com".to_string();
    let mut public_host = "127.0.0.1".to_string();
    let mut headless = false;
    let mut stun_server = "stun.l.google.com:19302".to_string();

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--fps" => fps = it.next().and_then(|s| s.parse().ok()).unwrap_or(fps),
            "--port" => port = it.next().and_then(|s| s.parse().ok()).unwrap_or(port),
            "--bitrate" => {
                bitrate_bps = it.next().and_then(|s| s.parse().ok()).unwrap_or(bitrate_bps)
            }
            "--adapter" => {
                adapter_index = it.next().and_then(|s| s.parse().ok()).unwrap_or(0)
            }
            "--output" => {
                output_index = it.next().and_then(|s| s.parse().ok()).unwrap_or(0)
            }
            "--window" => {
                window_hwnd = it.next().and_then(|s| s.parse().ok());
            }
            "--capture-backend" => {
                capture_backend = match it.next().as_deref() {
                    Some("dxgi") => capture::Backend::ForceDxgi,
                    Some("wgc") => capture::Backend::ForceWgc,
                    Some("auto") | None => capture::Backend::Auto,
                    Some(other) => {
                        eprintln!("warning: unknown --capture-backend '{other}', using auto");
                        capture::Backend::Auto
                    }
                }
            }
            "--sfu" => {
                sfu = match it.next().as_deref() {
                    Some("mesh") => SfuMode::Mesh,
                    Some("direct") | None => SfuMode::Direct,
                    Some(other) => {
                        eprintln!("warning: unknown --sfu '{other}', using direct");
                        SfuMode::Direct
                    }
                }
            }
            "--public-host" => public_host = it.next().unwrap_or(public_host),
            "--headless" => headless = true,
            "--stun-server" => stun_server = it.next().unwrap_or(stun_server),
            other => eprintln!("warning: ignoring unknown argument '{other}'"),
        }
    }

    Args {
        fps,
        port,
        bitrate_bps,
        adapter_index,
        output_index,
        window_hwnd,
        capture_backend,
        sfu,
        backend_url,
        public_host,
        headless,
        stun_server,
    }
}

/// Rewrites `backend/`'s HTTP base URL into the WebSocket URL used for
/// signaling.
fn http_url_to_ws(http_base: &str) -> String {
    if let Some(rest) = http_base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http_base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http_base.to_owned()
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    unsafe {
        timeBeginPeriod(1);
    }

    let args = parse_args();

    // A one-shot color-data check (FW_DEBUG_DUMP) also runs headless.
    let force_headless = args.headless || std::env::var("FW_DEBUG_DUMP").is_ok();

    if force_headless {
        let status: SharedStatus = Arc::new(Mutex::new(HostStatus::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let switch_target: PendingSwitch = Arc::new(Mutex::new(None));
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(run_pipeline(args, status, stop, switch_target));
    }

    tauri_app::run(args)
}

/// Set by the GUI's `switch_capture_target` command to request a live
/// capture-target change without tearing down the room/WebRTC connections.
/// Checked once per capture-loop iteration and cleared once applied.
pub type PendingSwitch = Arc<Mutex<Option<capture::CaptureSource>>>;

/// Runs the full capture -> encode -> send pipeline until `stop` is set or
/// the sender exits. Called from both the headless CLI path and the GUI's
/// worker thread; progress is reported through `status`.
pub async fn run_pipeline(
    args: Args,
    status: SharedStatus,
    stop: Arc<AtomicBool>,
    switch_target: PendingSwitch,
) -> anyhow::Result<()> {
    let ice_servers = transport::build_ice_servers(&args.stun_server);

    let frame_sender = match args.sfu {
        SfuMode::Direct => {
            let sender =
                transport::start_server(args.port, ice_servers, args.bitrate_bps).await?;
            let url = format!("http://{}:{}/", args.public_host, args.port);
            println!("Open {url} in a browser on the same LAN");
            {
                let mut s = status.lock().unwrap();
                s.room_code = "(direct)".to_string();
                s.viewer_url = url;
            }
            sender
        }
        SfuMode::Mesh => {
            let backend_ws_base = http_url_to_ws(&args.backend_url);
            let (sender, room_code) =
                transport::start_mesh_publisher(backend_ws_base, ice_servers, args.bitrate_bps)
                    .await?;

            // The viewer page itself is served by the backend, so the host
            // never needs an inbound port and viewers only ever see the
            // backend's fixed domain.
            let url = format!("{}/watch/{room_code}", args.backend_url);
            println!();
            println!("========================================");
            println!("  Room code : {room_code}");
            println!("  Watch URL : {url}");
            println!("========================================");
            println!("  Open the URL above to watch (multiple viewers OK)");
            println!();
            {
                let mut s = status.lock().unwrap();
                s.room_code = room_code;
                s.viewer_url = url;
            }

            sender
        }
    };

    {
        let mut s = status.lock().unwrap();
        s.running = true;
    }

    let forward_sender = frame_sender.clone();
    // Bounded channel with a small capacity: if sending can't keep up,
    // drop frames rather than letting latency grow unbounded.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(Vec<u8>, Duration)>(3);
    tokio::spawn(async move {
        while let Some((data, duration)) = rx.recv().await {
            forward_sender.send(data, duration).await;
        }
    });

    let status_for_loop = status.clone();
    let capture_task = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        run_capture_loop(&args, frame_sender, tx, status_for_loop, stop, switch_target)
    });

    let result = capture_task.await?;
    {
        let mut s = status.lock().unwrap();
        s.running = false;
        if let Err(e) = &result {
            s.error = Some(e.to_string());
        }
    }
    result
}

/// Builds the capture -> color-convert -> NVENC encoder stack for one
/// capture source. Used both for the initial setup and every live target
/// switch requested through `switch_capture_target`.
fn build_capture_stack(
    source: capture::CaptureSource,
    backend: capture::Backend,
    fps: u32,
    bitrate_bps: u32,
) -> anyhow::Result<(
    Box<dyn capture::ScreenCapture>,
    shaders::ColorConverter,
    encode::NvencEncoder,
)> {
    let cap = capture::start_capture(source, backend)?;
    println!("[capture] capturing at {}x{}", cap.width(), cap.height());
    let converter = shaders::ColorConverter::new(cap.device(), cap.width(), cap.height())?;
    let encoder = encode::NvencEncoder::new(
        cap.device(),
        converter.nv12_texture(),
        cap.width(),
        cap.height(),
        fps,
        bitrate_bps,
    )?;
    println!("[encode] NVENC (H.264, D3D11 zero-copy) initialized");
    Ok((cap, converter, encoder))
}

fn run_capture_loop(
    args: &Args,
    frame_sender: transport::FrameSender,
    tx: tokio::sync::mpsc::Sender<(Vec<u8>, Duration)>,
    status: SharedStatus,
    stop: Arc<AtomicBool>,
    switch_target: PendingSwitch,
) -> anyhow::Result<()> {
    let source = match args.window_hwnd {
        Some(hwnd) => capture::CaptureSource::Window { hwnd },
        None => capture::CaptureSource::Monitor {
            adapter_index: args.adapter_index,
            output_index: args.output_index,
        },
    };
    let mut current_source = source;
    let (mut cap, mut converter, mut encoder) =
        build_capture_stack(source, args.capture_backend, args.fps, args.bitrate_bps)?;
    {
        let mut s = status.lock().unwrap();
        s.width = cap.width();
        s.height = cap.height();
    }

    if std::env::var("FW_DEBUG_DUMP").is_ok() {
        if let CaptureFrame::Frame(tex) = cap.next_frame(200)? {
            let _nv12 = converter.convert(cap.context(), &tex)?;
            debug_dump::dump_nv12_stats(
                cap.device(),
                cap.context(),
                converter.nv12_texture(),
                cap.width(),
                cap.height(),
            )?;
            cap.release_frame()?;
        }
        return Ok(());
    }

    let frame_duration = Duration::from_secs_f64(1.0 / args.fps as f64);
    let mut frame_count: u64 = 0;
    let mut last_log = std::time::Instant::now();
    let mut frames_since_log = 0u32;
    let mut wait_sum = Duration::ZERO;
    let mut convert_sum = Duration::ZERO;
    let mut encode_sum = Duration::ZERO;
    let mut send_sum = Duration::ZERO;
    let mut bytes_sum: u64 = 0;
    let mut bytes_min = usize::MAX;
    let mut bytes_max = 0usize;

    // Capture arrives at the monitor's native rate; frames faster than the
    // requested --fps are dropped. Track the next due time cumulatively
    // (like a fixed-rate scheduler) to avoid drift.
    let mut next_due = std::time::Instant::now();
    // If the send path can't keep up, skip capture/convert/encode entirely
    // rather than let latency grow unbounded. Frames skipped before ever
    // reaching NVENC don't break the reference chain, so no IDR is needed
    // for those. An IDR is only needed when an already-encoded frame is
    // dropped right before sending (reference chain broken).
    let mut need_idr_after_skip = false;
    // Bandwidth adaptation: check the target bitrate computed from RTCP
    // feedback once per second and push it to NVENC if it changed.
    let mut current_bitrate_bps = args.bitrate_bps;

    loop {
        if stop.load(Ordering::SeqCst) {
            println!("[capture] stop requested, ending stream");
            return Ok(());
        }

        if let Some(new_source) = switch_target.lock().unwrap().take() {
            match build_capture_stack(new_source, args.capture_backend, args.fps, current_bitrate_bps) {
                Ok((new_cap, new_converter, new_encoder)) => {
                    cap = new_cap;
                    converter = new_converter;
                    encoder = new_encoder;
                    current_source = new_source;
                    if let Ok(mut s) = status.lock() {
                        s.width = cap.width();
                        s.height = cap.height();
                    }
                    // Forces an IDR on the next frame (see the force_idr
                    // check below) since the new encoder starts a fresh
                    // reference chain.
                    frame_count = 0;
                    next_due = std::time::Instant::now();
                    need_idr_after_skip = false;
                    println!(
                        "[capture] switched capture target ({}x{})",
                        cap.width(),
                        cap.height()
                    );
                }
                Err(e) => {
                    eprintln!("[capture] failed to switch capture target: {e:#} (keeping current target)");
                }
            }
        }

        let t_wait_start = std::time::Instant::now();
        let frame = cap.next_frame(100)?;
        wait_sum += t_wait_start.elapsed();

        match frame {
            CaptureFrame::GiveUp => {
                eprintln!(
                    "[capture] DXGI couldn't recover, falling back to WGC for the current target"
                );
                let (new_cap, new_converter, new_encoder) = build_capture_stack(
                    current_source,
                    capture::Backend::ForceWgc,
                    args.fps,
                    current_bitrate_bps,
                )
                .context("DXGIで復旧できず、WGCへのフォールバックも失敗しました")?;
                cap = new_cap;
                converter = new_converter;
                encoder = new_encoder;
                if let Ok(mut s) = status.lock() {
                    s.width = cap.width();
                    s.height = cap.height();
                }
                frame_count = 0;
                next_due = std::time::Instant::now();
                need_idr_after_skip = false;
            }
            CaptureFrame::Frame(tex) => {
                let now = std::time::Instant::now();
                if now < next_due {
                    // Too early for the target fps; drop it (released
                    // automatically on the next next_frame() call).
                    continue;
                }
                next_due += frame_duration;
                if next_due < now {
                    // Fell far behind; reset instead of accumulating drift.
                    next_due = now + frame_duration;
                }

                if tx.capacity() == 0 {
                    // Send side is backed up. Skip without encoding so the
                    // backlog doesn't grow (forcing an IDR here instead
                    // would make weak connections send ever-larger
                    // keyframes and make effective fps worse, not better).
                    continue;
                }

                let t_convert = std::time::Instant::now();
                let _nv12 = converter.convert(cap.context(), &tex)?;
                convert_sum += t_convert.elapsed();

                let t_encode = std::time::Instant::now();
                let force_idr =
                    frame_count == 0 || need_idr_after_skip || frame_sender.take_force_idr();
                let bytes = encoder.encode_frame(force_idr)?;
                encode_sum += t_encode.elapsed();

                bytes_sum += bytes.len() as u64;
                bytes_min = bytes_min.min(bytes.len());
                bytes_max = bytes_max.max(bytes.len());
                if frame_count < 3 || force_idr {
                    println!(
                        "[capture] frame#{frame_count} force_idr={force_idr} size={} bytes head16={:02x?}",
                        bytes.len(),
                        &bytes[..bytes.len().min(16)]
                    );
                }

                // Release only after the texture has been read (color
                // convert + encode); releasing right after next_frame()
                // let DXGI reuse the buffer before it was read, zeroing it.
                cap.release_frame()?;

                let t_send = std::time::Instant::now();
                let send_result = tx.try_send((bytes, frame_duration));
                send_sum += t_send.elapsed();
                if let Err(e) = &send_result {
                    if matches!(e, tokio::sync::mpsc::error::TrySendError::Full(_)) {
                        // Channel filled between the capacity() check and
                        // try_send(); drop this frame too.
                        eprintln!("[capture] send channel full, dropping this frame");
                        need_idr_after_skip = true;
                        frame_count += 1;
                        frames_since_log += 1;
                        continue;
                    }
                }
                need_idr_after_skip = false;
                if send_result.is_err() {
                    println!("[capture] receiver gone, stopping");
                    return Ok(());
                }
                frame_count += 1;
                frames_since_log += 1;
            }
            CaptureFrame::Timeout | CaptureFrame::Recovered => {}
        }

        if last_log.elapsed() >= Duration::from_secs(1) {
            let elapsed = last_log.elapsed();
            let fps = frames_since_log as f64 / elapsed.as_secs_f64();
            let n = frames_since_log.max(1) as f64;
            println!(
                "[capture] measured encode fps: {fps:.1}  (avg: wait={:.2}ms convert={:.2}ms encode={:.2}ms send={:.3}ms)  frame size: min={} max={} avg={:.0} bytes",
                wait_sum.as_secs_f64() * 1000.0 / n,
                convert_sum.as_secs_f64() * 1000.0 / n,
                encode_sum.as_secs_f64() * 1000.0 / n,
                send_sum.as_secs_f64() * 1000.0 / n,
                if bytes_min == usize::MAX { 0 } else { bytes_min },
                bytes_max,
                bytes_sum as f64 / n,
            );
            if let Ok(mut s) = status.lock() {
                s.measured_fps = fps;
            }

            let desired_bitrate_bps = frame_sender.target_bitrate_bps();
            if desired_bitrate_bps != current_bitrate_bps {
                match encoder.reconfigure_bitrate(desired_bitrate_bps) {
                    Ok(()) => {
                        println!(
                            "[encode] bitrate reconfigured: {}kbps -> {}kbps",
                            current_bitrate_bps / 1000,
                            desired_bitrate_bps / 1000
                        );
                        current_bitrate_bps = desired_bitrate_bps;
                    }
                    Err(e) => eprintln!("[encode] failed to reconfigure bitrate: {e:#}"),
                }
            }
            if let Ok(mut s) = status.lock() {
                s.current_bitrate_bps = current_bitrate_bps;
            }

            frames_since_log = 0;
            wait_sum = Duration::ZERO;
            convert_sum = Duration::ZERO;
            encode_sum = Duration::ZERO;
            send_sum = Duration::ZERO;
            bytes_sum = 0;
            bytes_min = usize::MAX;
            bytes_max = 0;
            last_log = std::time::Instant::now();
        }
    }
}
