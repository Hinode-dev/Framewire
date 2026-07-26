//! Windows.Graphics.Capture (WGC) capture path.
//!
//! Used for two cases:
//! - As a fallback when DXGI Desktop Duplication isn't usable (multi-GPU
//!   setups) or keeps returning `DXGI_ERROR_ACCESS_LOST` for a particular
//!   game. WGC has its own limitation in exclusive fullscreen (irregular
//!   frame-rate division, e.g. 144Hz -> 48fps), so it's the fallback rather
//!   than the primary path for monitor capture.
//! - As the only option for capturing a single window, since DXGI Desktop
//!   Duplication can only target a whole monitor.
//!
//! Polls `TryGetNextFrame` rather than registering a `FrameArrived`
//! closure, since the WinRT/Rust interop for the latter is more involved.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use windows::core::Interface;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use super::{CaptureFrame, ScreenCapture};

fn create_device_for_adapter(
    adapter: &IDXGIAdapter1,
) -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((
        device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?,
        context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?,
    ))
}

fn create_copy_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> anyhow::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or_else(|| anyhow!("コピー先テクスチャ作成に失敗"))
}

pub struct WgcCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    _item: GraphicsCaptureItem,
    frame_pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    copy_texture: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl WgcCapture {
    pub fn for_monitor(adapter_index: u32, output_index: u32) -> anyhow::Result<Self> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(adapter_index)? };
        let (device, context) =
            create_device_for_adapter(&adapter).context("D3D11デバイス作成に失敗")?;

        let output = unsafe { adapter.EnumOutputs(output_index)? };
        let desc = unsafe { output.GetDesc()? };
        let hmonitor = desc.Monitor;
        let rc = desc.DesktopCoordinates;
        let width = (rc.right - rc.left) as u32;
        let height = (rc.bottom - rc.top) as u32;

        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .context("IGraphicsCaptureItemInteropの取得に失敗")?;
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForMonitor(hmonitor) }.context("GraphicsCaptureItem作成に失敗")?;

        Self::finish_setup(device, context, item, width, height)
    }

    /// Captures a single window rather than a whole monitor. `hwnd_raw` is
    /// a raw `HWND` value as returned by `list_windows()`.
    ///
    /// Not tied to any particular monitor/adapter selection (a window isn't
    /// pinned to one output the way a monitor capture is), so this always
    /// uses the default adapter (index 0).
    pub fn for_window(hwnd_raw: isize) -> anyhow::Result<Self> {
        let hwnd = HWND(hwnd_raw as *mut c_void);

        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(0)? };
        let (device, context) =
            create_device_for_adapter(&adapter).context("D3D11デバイス作成に失敗")?;

        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .context("IGraphicsCaptureItemInteropの取得に失敗")?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd) }
            .context("GraphicsCaptureItem作成に失敗（ウィンドウが閉じられた可能性があります）")?;

        let size = item.Size().context("ウィンドウサイズの取得に失敗")?;
        if size.Width <= 0 || size.Height <= 0 {
            anyhow::bail!("キャプチャ対象のウィンドウサイズが不正です（最小化されている可能性があります）");
        }
        let width = size.Width as u32;
        let height = size.Height as u32;

        Self::finish_setup(device, context, item, width, height)
    }

    fn finish_setup(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        item: GraphicsCaptureItem,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Self> {
        let dxgi_device: IDXGIDevice = device.cast()?;
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)? };
        let winrt_device: IDirect3DDevice = inspectable.cast()?;

        let size = SizeInt32 {
            Width: width as i32,
            Height: height as i32,
        };
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .context("Direct3D11CaptureFramePool作成に失敗")?;

        let session = frame_pool
            .CreateCaptureSession(&item)
            .context("GraphicsCaptureSession作成に失敗")?;
        // Hide the yellow capture border (Windows 11+). Not fatal if it fails.
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture().context("StartCaptureに失敗")?;

        let copy_texture = create_copy_texture(&device, width, height)?;

        Ok(Self {
            device,
            context,
            _item: item,
            frame_pool,
            session,
            copy_texture,
            width,
            height,
        })
    }
}

impl ScreenCapture for WgcCapture {
    fn next_frame(&mut self, timeout_ms: u32) -> anyhow::Result<CaptureFrame> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            if let Ok(frame) = self.frame_pool.TryGetNextFrame() {
                // A resized window (or, rarely, a monitor resolution
                // change) changes the frame's content size. The rest of
                // the pipeline (color convert, NVENC) is sized once at
                // startup and can't adapt mid-stream, so surface this as
                // an error asking the user to restart rather than silently
                // corrupting frames.
                if let Ok(content_size) = frame.ContentSize() {
                    if content_size.Width as u32 != self.width || content_size.Height as u32 != self.height {
                        return Err(anyhow!(
                            "キャプチャ対象のサイズが{}x{}から{}x{}に変わりました。\
                             一度配信を停止し、再度開始してください。",
                            self.width,
                            self.height,
                            content_size.Width,
                            content_size.Height
                        ));
                    }
                }
                if let Ok(surface) = frame.Surface() {
                    if let Ok(access) = surface.cast::<IDirect3DDxgiInterfaceAccess>() {
                        if let Ok(texture) = unsafe { access.GetInterface::<ID3D11Texture2D>() } {
                            unsafe { self.context.CopyResource(&self.copy_texture, &texture) };
                            return Ok(CaptureFrame::Frame(self.copy_texture.clone()));
                        }
                    }
                }
            }
            if Instant::now() >= deadline {
                return Ok(CaptureFrame::Timeout);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn release_frame(&mut self) -> anyhow::Result<()> {
        // The Direct3D11CaptureFrame from TryGetNextFrame releases itself
        // when it goes out of scope after the copy; there's no explicit
        // release API like DXGI's. Kept for trait compatibility.
        Ok(())
    }

    fn device(&self) -> &ID3D11Device {
        &self.device
    }

    fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.frame_pool.Close();
    }
}
