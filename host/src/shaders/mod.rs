//! BGRA -> NV12 color conversion via a D3D11 compute shader. Runs entirely
//! on the GPU; frame data never touches the CPU.

use anyhow::{anyhow, Context};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_SHADER_MACRO};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11UnorderedAccessView, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BIND_UNORDERED_ACCESS, D3D11_TEX2D_UAV, D3D11_TEXTURE2D_DESC,
    D3D11_UNORDERED_ACCESS_VIEW_DESC, D3D11_UAV_DIMENSION_TEXTURE2D, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
};

const SHADER_SRC: &str = include_str!("bgra_to_nv12.hlsl");

pub struct ColorConverter {
    shader: ID3D11ComputeShader,
    nv12_texture: ID3D11Texture2D,
    y_uav: ID3D11UnorderedAccessView,
    uv_uav: ID3D11UnorderedAccessView,
    width: u32,
    height: u32,
}

impl ColorConverter {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> anyhow::Result<Self> {
        let shader = compile_compute_shader(device)?;

        let nv12_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_SHADER_RESOURCE).0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut nv12_texture: Option<ID3D11Texture2D> = None;
        // NV12 is 4:2:0 chroma-subsampled — width/height must both be even,
        // or CreateTexture2D fails with E_INVALIDARG (odd-sized capture
        // targets, e.g. some non-game utility windows, can hit this).
        unsafe { device.CreateTexture2D(&nv12_desc, None, Some(&mut nv12_texture)) }
            .with_context(|| format!("NV12テクスチャ作成に失敗（{width}x{height}、NV12は幅・高さが偶数である必要があります）"))?;
        let nv12_texture = nv12_texture.ok_or_else(|| anyhow!("NV12テクスチャ作成に失敗"))?;

        let y_uav = create_plane_uav(device, &nv12_texture, DXGI_FORMAT_R8_UNORM)?;
        let uv_uav = create_plane_uav(device, &nv12_texture, DXGI_FORMAT_R8G8_UNORM)?;

        Ok(Self {
            shader,
            nv12_texture,
            y_uav,
            uv_uav,
            width,
            height,
        })
    }

    /// Returns the destination NV12 texture that the encoder registers as
    /// its input resource. The same instance is reused for the converter's
    /// entire lifetime.
    pub fn nv12_texture(&self) -> &ID3D11Texture2D {
        &self.nv12_texture
    }

    /// Converts a BGRA input texture to NV12 and returns the result. The
    /// returned texture is reused across calls, so keep it alive until
    /// encoding (the `NvEncEncodePicture` call) is done with it.
    pub fn convert(
        &self,
        context: &ID3D11DeviceContext,
        input: &ID3D11Texture2D,
    ) -> anyhow::Result<&ID3D11Texture2D> {
        let srv = create_input_srv(context, input)?;

        unsafe {
            context.CSSetShader(&self.shader, None);
            let srvs = [Some(srv)];
            context.CSSetShaderResources(0, Some(&srvs));
            let uavs = [Some(self.y_uav.clone()), Some(self.uv_uav.clone())];
            context.CSSetUnorderedAccessViews(0, uavs.len() as u32, Some(uavs.as_ptr()), None);

            let group_x = self.width.div_ceil(8);
            let group_y = self.height.div_ceil(8);
            context.Dispatch(group_x, group_y, 1);

            // Unbind so a different texture can be bound to t0 next frame.
            context.CSSetShaderResources(0, Some(&[None]));
            let clear_uavs: [Option<ID3D11UnorderedAccessView>; 2] = [None, None];
            context.CSSetUnorderedAccessViews(0, clear_uavs.len() as u32, Some(clear_uavs.as_ptr()), None);
        }

        Ok(&self.nv12_texture)
    }
}

fn create_input_srv(
    context: &ID3D11DeviceContext,
    input: &ID3D11Texture2D,
) -> anyhow::Result<ID3D11ShaderResourceView> {
    let device = unsafe { context.GetDevice()? };
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(input, None, Some(&mut srv))? };
    srv.ok_or_else(|| anyhow!("入力SRV作成に失敗"))
}

fn create_plane_uav(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    plane_format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> anyhow::Result<ID3D11UnorderedAccessView> {
    let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: plane_format,
        ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
        Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
        },
    };
    let mut uav = None;
    unsafe { device.CreateUnorderedAccessView(texture, Some(&desc), Some(&mut uav))? };
    uav.ok_or_else(|| anyhow!("平面UAV作成に失敗"))
}

fn compile_compute_shader(device: &ID3D11Device) -> anyhow::Result<ID3D11ComputeShader> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let entry = c"CSMain";
    let target = c"cs_5_0";

    let result = unsafe {
        D3DCompile(
            SHADER_SRC.as_ptr() as *const _,
            SHADER_SRC.len(),
            None,
            None::<*const D3D_SHADER_MACRO>,
            None,
            windows::core::PCSTR(entry.as_ptr() as *const u8),
            windows::core::PCSTR(target.as_ptr() as *const u8),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };

    if let Err(e) = result {
        let msg = errors
            .map(|e| blob_to_string(&e))
            .unwrap_or_else(|| "(エラーメッセージなし)".to_string());
        return Err(anyhow!("シェーダコンパイル失敗: {e}\n{msg}"));
    }

    let code = code.ok_or_else(|| anyhow!("シェーダバイトコードが空です"))?;
    let bytecode = unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize())
    };

    let mut shader: Option<ID3D11ComputeShader> = None;
    unsafe { device.CreateComputeShader(bytecode, None, Some(&mut shader))? };
    shader.ok_or_else(|| anyhow!("ComputeShader作成に失敗"))
}

fn blob_to_string(blob: &ID3DBlob) -> String {
    unsafe {
        let ptr = blob.GetBufferPointer() as *const u8;
        let len = blob.GetBufferSize();
        let bytes = std::slice::from_raw_parts(ptr, len);
        String::from_utf8_lossy(bytes).to_string()
    }
}
