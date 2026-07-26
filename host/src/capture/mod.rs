//! Screen capture. DXGI Desktop Duplication is the primary path for
//! whole-monitor capture, falling back to Windows.Graphics.Capture (WGC) if
//! it fails to initialize. Capturing a single window (rather than a whole
//! monitor) is only possible through WGC, since DXGI Desktop Duplication
//! can't target an individual window.

mod dxgi;
mod wgc;

pub use dxgi::DxgiCapture;

pub mod thumbnail;

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, TRUE};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetShellWindow, GetWindow, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW,
    IsIconic, IsWindowVisible, GWL_EXSTYLE, GW_OWNER, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

/// One capturable monitor.
#[derive(Debug, Clone)]
pub struct CaptureTarget {
    pub adapter_index: u32,
    pub adapter_name: String,
    pub output_index: u32,
    pub output_name: String,
}

/// One capturable open window.
#[derive(Debug, Clone)]
pub struct WindowTarget {
    /// Raw `HWND` value. Kept as a plain integer (rather than the
    /// `windows` crate's `HWND` type) so it can be stored in `Args` and
    /// moved across threads freely; it's converted back to `HWND` only at
    /// the point of use.
    pub hwnd: isize,
    pub title: String,
}

/// What to capture: an entire monitor, or a single window.
#[derive(Debug, Clone, Copy)]
pub enum CaptureSource {
    Monitor { adapter_index: u32, output_index: u32 },
    Window { hwnd: isize },
}

/// Result of trying to get one frame.
pub enum CaptureFrame {
    /// A new frame was captured.
    Frame(ID3D11Texture2D),
    /// No new frame arrived within the timeout.
    Timeout,
    /// Recovered automatically from DXGI_ERROR_ACCESS_LOST; the caller may
    /// continue.
    Recovered,
    /// DXGI Desktop Duplication couldn't recover from repeated
    /// `DXGI_ERROR_ACCESS_LOST` (some exclusive-fullscreen games keep
    /// re-triggering it). The caller should fall back to capturing the
    /// same monitor via WGC rather than treat this as fatal.
    GiveUp,
}

pub trait ScreenCapture {
    /// Gets the next frame, waiting up to `timeout_ms`.
    ///
    /// When `CaptureFrame::Frame` is returned, `release_frame()` must be
    /// called once the texture is done being used, and not before (DXGI
    /// reuses the texture for the next frame as soon as it's released, so
    /// releasing too early zeroes or corrupts the content being read).
    fn next_frame(&mut self, timeout_ms: u32) -> anyhow::Result<CaptureFrame>;
    /// Call after finishing with the texture returned by `next_frame()`.
    fn release_frame(&mut self) -> anyhow::Result<()>;
    fn device(&self) -> &ID3D11Device;
    fn context(&self) -> &ID3D11DeviceContext;
    fn width(&self) -> u32;
    fn height(&self) -> u32;
}

/// Lists the available monitor capture targets.
pub fn list_targets() -> anyhow::Result<Vec<CaptureTarget>> {
    dxgi::list_targets()
}

/// Lists visible, non-minimized, titled top-level application windows for
/// window-specific capture — the same "real applications only" heuristic
/// used by Alt-Tab and app-picker UIs like Discord's, filtering out
/// toolbars, notification popups, and other overlay/utility windows.
pub fn list_windows() -> anyhow::Result<Vec<WindowTarget>> {
    let mut targets: Vec<WindowTarget> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_proc),
            LPARAM(&mut targets as *mut Vec<WindowTarget> as isize),
        );
    }
    Ok(targets)
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let targets = &mut *(lparam.0 as *mut Vec<WindowTarget>);

        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }
        if IsIconic(hwnd).as_bool() {
            // Minimized; there's no live frame to capture.
            return TRUE;
        }
        // IsWindowVisible stays true for windows on another virtual desktop
        // (Windows hides them via DWM cloaking, not by unsetting the
        // visible style), so without this check background/other-desktop
        // apps would show up as if they were on screen right now.
        let mut cloaked: u32 = 0;
        let is_cloaked = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0;
        if is_cloaked {
            return TRUE;
        }
        if hwnd == GetShellWindow() {
            return TRUE;
        }
        if GetWindow(hwnd, GW_OWNER).is_ok() {
            // Has an owner window (tooltip, popup, etc.), not a top-level
            // app window.
            return TRUE;
        }
        // The standard Alt-Tab / "real application window" heuristic:
        // WS_EX_TOOLWINDOW marks floating toolbars, notification popups,
        // and other overlay/utility windows that aren't meant to represent
        // an application — unless the window explicitly opts back in with
        // WS_EX_APPWINDOW. Excluding these also filters out some windows
        // that Windows.Graphics.Capture simply can't create a capture item
        // for in the first place.
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let is_tool_window = ex_style & WS_EX_TOOLWINDOW.0 != 0;
        let is_app_window = ex_style & WS_EX_APPWINDOW.0 != 0;
        if is_tool_window && !is_app_window {
            return TRUE;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len == 0 {
            return TRUE;
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let actual = GetWindowTextW(hwnd, &mut buf);
        if actual == 0 {
            return TRUE;
        }
        let title = String::from_utf16_lossy(&buf[..actual as usize]);
        targets.push(WindowTarget {
            hwnd: hwnd.0 as isize,
            title,
        });
        TRUE
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// DXGI first, falling back to WGC on failure (default). Only applies
    /// to monitor capture; window capture always uses WGC.
    Auto,
    /// Forces DXGI only (for testing). Not valid for window capture.
    ForceDxgi,
    /// Forces WGC only (for testing).
    ForceWgc,
}

/// Starts a capture session for the given source.
pub fn start_capture(source: CaptureSource, backend: Backend) -> anyhow::Result<Box<dyn ScreenCapture>> {
    match source {
        CaptureSource::Window { hwnd } => {
            if backend == Backend::ForceDxgi {
                anyhow::bail!(
                    "ウィンドウ指定のキャプチャはDXGIでは非対応です（WGCのみ対応。\
                     DXGI Desktop Duplicationは単一ウィンドウを対象にできません）"
                );
            }
            println!("[capture] using WGC (window capture)");
            wgc::WgcCapture::for_window(hwnd).map(|c| Box::new(c) as Box<dyn ScreenCapture>)
        }
        CaptureSource::Monitor {
            adapter_index,
            output_index,
        } => start_monitor_capture(adapter_index, output_index, backend),
    }
}

fn start_monitor_capture(
    adapter_index: u32,
    output_index: u32,
    backend: Backend,
) -> anyhow::Result<Box<dyn ScreenCapture>> {
    match backend {
        Backend::ForceWgc => {
            println!("[capture] forcing WGC (Windows.Graphics.Capture)");
            return wgc::WgcCapture::for_monitor(adapter_index, output_index)
                .map(|c| Box::new(c) as Box<dyn ScreenCapture>);
        }
        Backend::ForceDxgi => {
            println!("[capture] forcing DXGI Desktop Duplication");
            return DxgiCapture::new(adapter_index, output_index)
                .map(|c| Box::new(c) as Box<dyn ScreenCapture>);
        }
        Backend::Auto => {}
    }

    match DxgiCapture::new(adapter_index, output_index) {
        Ok(cap) => {
            println!("[capture] using DXGI Desktop Duplication");
            Ok(Box::new(cap))
        }
        Err(e) => {
            eprintln!("[capture] DXGI init failed, falling back to WGC: {e}");
            wgc::WgcCapture::for_monitor(adapter_index, output_index)
                .map(|c| Box::new(c) as Box<dyn ScreenCapture>)
        }
    }
}
