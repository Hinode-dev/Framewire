//! Capture via DXGI Desktop Duplication.
//!
//! The texture DXGI returns is copied on the GPU into our own persistent
//! texture immediately after `AcquireNextFrame`, then `ReleaseFrame()` is
//! called right away (rather than deferring it until color conversion and
//! encoding finish). Releasing late caused a real game in exclusive
//! fullscreen to hit `DXGI_ERROR_ACCESS_LOST` repeatedly, even after
//! recreating the duplication interface; releasing immediately (as
//! Microsoft's docs recommend) fixed it.

use anyhow::{anyhow, Context};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

use super::{CaptureFrame, CaptureTarget, ScreenCapture};

pub fn list_targets() -> anyhow::Result<Vec<CaptureTarget>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut result = Vec::new();
    let mut ai = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(ai) } {
            Ok(a) => a,
            Err(_) => break,
        };
        let desc = unsafe { adapter.GetDesc1()? };
        let adapter_name = String::from_utf16_lossy(
            &desc.Description[..desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len())],
        );

        let mut oi = 0u32;
        loop {
            let output = match unsafe { adapter.EnumOutputs(oi) } {
                Ok(o) => o,
                Err(_) => break,
            };
            let odesc = unsafe { output.GetDesc()? };
            let output_name = String::from_utf16_lossy(
                &odesc.DeviceName[..odesc
                    .DeviceName
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(odesc.DeviceName.len())],
            );
            result.push(CaptureTarget {
                adapter_index: ai,
                adapter_name: adapter_name.clone(),
                output_index: oi,
                output_name,
            });
            oi += 1;
        }
        ai += 1;
    }
    Ok(result)
}

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

fn duplicate_output(
    factory: &IDXGIFactory1,
    adapter_index: u32,
    output_index: u32,
    device: &ID3D11Device,
) -> anyhow::Result<IDXGIOutputDuplication> {
    let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(adapter_index)? };
    let output = unsafe { adapter.EnumOutputs(output_index)? };
    let output1: IDXGIOutput1 = output.cast()?;
    Ok(unsafe { output1.DuplicateOutput(device)? })
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

pub struct DxgiCapture {
    factory: IDXGIFactory1,
    adapter_index: u32,
    output_index: u32,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    copy_texture: ID3D11Texture2D,
    width: u32,
    height: u32,
    consecutive_access_lost: u32,
    format_logged: bool,
}

impl DxgiCapture {
    pub fn new(adapter_index: u32, output_index: u32) -> anyhow::Result<Self> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(adapter_index)? };
        let (device, context) =
            create_device_for_adapter(&adapter).context("D3D11デバイス作成に失敗")?;
        let duplication = duplicate_output(&factory, adapter_index, output_index, &device)
            .context("IDXGIOutputDuplication作成に失敗（マルチGPU構成の可能性）")?;

        let output = unsafe { adapter.EnumOutputs(output_index)? };
        let desc = unsafe { output.GetDesc()? };
        let rc = desc.DesktopCoordinates;
        let width = (rc.right - rc.left) as u32;
        let height = (rc.bottom - rc.top) as u32;

        let copy_texture = create_copy_texture(&device, width, height)?;

        Ok(Self {
            factory,
            adapter_index,
            output_index,
            device,
            context,
            duplication,
            copy_texture,
            width,
            height,
            consecutive_access_lost: 0,
            format_logged: false,
        })
    }

    fn recreate_duplication(&mut self) -> anyhow::Result<()> {
        loop {
            match duplicate_output(&self.factory, self.adapter_index, self.output_index, &self.device) {
                Ok(d) => {
                    self.duplication = d;
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("[capture] failed to recreate IDXGIOutputDuplication, retrying in 100ms: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }
}

impl ScreenCapture for DxgiCapture {
    fn next_frame(&mut self, timeout_ms: u32) -> anyhow::Result<CaptureFrame> {
        // Sync capture timing to the presentation interval, as Sunshine does.
        let _ = unsafe { DwmFlush() };

        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        let acquire_result =
            unsafe { self.duplication.AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource) };

        match acquire_result {
            Ok(()) => {
                self.consecutive_access_lost = 0;
                let result = if frame_info.LastPresentTime != 0 {
                    if let Some(resource) = resource {
                        let texture: ID3D11Texture2D = resource.cast()?;
                        if !self.format_logged {
                            self.format_logged = true;
                            let mut src_desc = D3D11_TEXTURE2D_DESC::default();
                            unsafe { texture.GetDesc(&mut src_desc) };
                            println!(
                                "[capture] captured texture format: {:?} (expected: {:?})",
                                src_desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM
                            );
                            if src_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                                eprintln!(
                                    "[capture] warning: unexpected pixel format, possibly HDR/Auto \
                                     HDR is enabled. The color-convert shader assumes SDR \
                                     (B8G8R8A8_UNORM), so video may be corrupted or black."
                                );
                            }
                        }
                        // Copy into our own texture on the GPU right away and
                        // release DXGI's buffer immediately (minimize how
                        // long it's held).
                        unsafe { self.context.CopyResource(&self.copy_texture, &texture) };
                        CaptureFrame::Frame(self.copy_texture.clone())
                    } else {
                        CaptureFrame::Timeout
                    }
                } else {
                    CaptureFrame::Timeout
                };
                // Release right away, never deferred until the next
                // AcquireNextFrame, per Microsoft's recommendation.
                let _ = unsafe { self.duplication.ReleaseFrame() };
                Ok(result)
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(CaptureFrame::Timeout),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.consecutive_access_lost += 1;
                // Some exclusive-fullscreen games keep re-triggering
                // ACCESS_LOST immediately after the interface is recreated.
                // Back off on repeated failures and give up with an
                // explicit error past a threshold, rather than spinning
                // forever.
                if self.consecutive_access_lost > 200 {
                    eprintln!(
                        "[capture] DXGI_ERROR_ACCESS_LOSTが{}回連続で発生し、DXGIでの継続を断念します。\
                         排他的フルスクリーンのゲーム・アンチチート/キャプチャ防止機能との非互換の\
                         可能性があります。WGCへフォールバックします。",
                        self.consecutive_access_lost
                    );
                    return Ok(CaptureFrame::GiveUp);
                }
                if self.consecutive_access_lost > 5 {
                    eprintln!(
                        "[capture] DXGI_ERROR_ACCESS_LOST occurred {} times in a row, recreating duplication",
                        self.consecutive_access_lost
                    );
                    std::thread::sleep(std::time::Duration::from_millis(50));
                } else {
                    eprintln!("[capture] DXGI_ERROR_ACCESS_LOST detected, recreating duplication");
                }
                self.recreate_duplication()?;
                Ok(CaptureFrame::Recovered)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn release_frame(&mut self) -> anyhow::Result<()> {
        // No-op: frames are copied out immediately in next_frame(), so
        // there's nothing left to release here. Kept for trait compatibility.
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
