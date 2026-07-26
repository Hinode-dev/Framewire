//! Diagnostic helper: reads the color-convert shader's NV12 output texture
//! back to the CPU and prints the Y/UV plane value distribution. Used to
//! debug capture/color-conversion issues.

use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_FLAG, D3D11_CPU_ACCESS_READ,
    D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

pub fn dump_nv12_stats(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    nv12_texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> anyhow::Result<()> {
    let mut src_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { nv12_texture.GetDesc(&mut src_desc) };

    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: src_desc.Format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: D3D11_BIND_FLAG(0).0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
    let staging = staging.ok_or_else(|| anyhow::anyhow!("failed to create staging texture"))?;

    unsafe { context.CopyResource(&staging, nv12_texture) };

    let mut mapped = Default::default();
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
    let row_pitch = mapped.RowPitch as usize;
    let base = mapped.pData as *const u8;

    let mut y_min = 255u8;
    let mut y_max = 0u8;
    let mut y_sum: u64 = 0;
    let y_count = (width as usize) * (height as usize);
    for row in 0..height as usize {
        let row_ptr = unsafe { base.add(row * row_pitch) };
        for col in 0..width as usize {
            let v = unsafe { *row_ptr.add(col) };
            y_min = y_min.min(v);
            y_max = y_max.max(v);
            y_sum += v as u64;
        }
    }

    let uv_height = (height as usize) / 2;
    let uv_base = unsafe { base.add((height as usize) * row_pitch) };
    let mut u_min = 255u8;
    let mut u_max = 0u8;
    let mut u_sum: u64 = 0;
    let mut v_min = 255u8;
    let mut v_max = 0u8;
    let mut v_sum: u64 = 0;
    let uv_count = (width as usize / 2) * uv_height;
    for row in 0..uv_height {
        let row_ptr = unsafe { uv_base.add(row * row_pitch) };
        for col in 0..(width as usize / 2) {
            let u = unsafe { *row_ptr.add(col * 2) };
            let v = unsafe { *row_ptr.add(col * 2 + 1) };
            u_min = u_min.min(u);
            u_max = u_max.max(u);
            u_sum += u as u64;
            v_min = v_min.min(v);
            v_max = v_max.max(v);
            v_sum += v as u64;
        }
    }

    unsafe { context.Unmap(&staging, 0) };

    println!(
        "[debug] Y: min={y_min} max={y_max} avg={:.1} (n={y_count})",
        y_sum as f64 / y_count as f64
    );
    println!(
        "[debug] U: min={u_min} max={u_max} avg={:.1} / V: min={v_min} max={v_max} avg={:.1} (n={uv_count})",
        u_sum as f64 / uv_count as f64,
        v_sum as f64 / uv_count as f64
    );

    Ok(())
}
