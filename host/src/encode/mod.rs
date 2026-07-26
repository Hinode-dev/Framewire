//! NVENC D3D11 zero-copy encoding.
//!
//! Opens a session with `NV_ENC_DEVICE_TYPE_DIRECTX` and passes the
//! color-converted NV12 `ID3D11Texture2D` straight through
//! `NvEncRegisterResource` -> `NvEncMapInputResource` -> `NvEncEncodePicture`,
//! with no CPU staging copy.
//!
//! The `nvidia-video-codec-sdk` crate's safe wrapper (`Encoder`/`Session`)
//! only supports CUDA devices, not the DirectX device type, so this uses
//! the crate's lower-level `sys` bindings and `ENCODE_API` function table
//! directly instead.

use std::ffi::c_void;

use anyhow::Context;
use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    NVENCAPI_VERSION, NV_ENC_BUFFER_FORMAT,
    NV_ENC_CODEC_H264_GUID, NV_ENC_CONFIG, NV_ENC_CONFIG_VER, NV_ENC_CREATE_BITSTREAM_BUFFER,
    NV_ENC_CREATE_BITSTREAM_BUFFER_VER, NV_ENC_DEVICE_TYPE, NV_ENC_H264_PROFILE_HIGH_GUID,
    NV_ENC_INITIALIZE_PARAMS, NV_ENC_INITIALIZE_PARAMS_VER, NV_ENC_INPUT_PTR,
    NV_ENC_INPUT_RESOURCE_TYPE, NV_ENC_LOCK_BITSTREAM, NV_ENC_LOCK_BITSTREAM_VER,
    NV_ENC_MAP_INPUT_RESOURCE, NV_ENC_MAP_INPUT_RESOURCE_VER,
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
    NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_FLAGS, NV_ENC_PIC_PARAMS, NV_ENC_PIC_PARAMS_VER,
    NV_ENC_PIC_STRUCT, NV_ENC_PRESET_CONFIG, NV_ENC_PRESET_CONFIG_VER, NV_ENC_PRESET_P1_GUID,
    NV_ENC_RC_PARAMS_VER, NV_ENC_RECONFIGURE_PARAMS, NV_ENC_RECONFIGURE_PARAMS_VER,
    NV_ENC_REGISTERED_PTR, NV_ENC_REGISTER_RESOURCE,
    NV_ENC_REGISTER_RESOURCE_VER, NV_ENC_TUNING_INFO,
};
use nvidia_video_codec_sdk::ENCODE_API;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

pub struct NvencEncoder {
    encoder_ptr: *mut c_void,
    registered_resource: NV_ENC_REGISTERED_PTR,
    bitstream_buffer: *mut c_void,
    width: u32,
    height: u32,
    fps: u32,
    frame_idx: u32,
    /// Reused across bitrate reconfigurations. `NV_ENC_INITIALIZE_PARAMS`
    /// keeps a pointer into this, so it must outlive those calls.
    encode_config: NV_ENC_CONFIG,
}

// The NVIDIA driver guards the NVENC session handle internally; moving it
// across threads is fine as long as calls are made sequentially.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    pub fn new(
        device: &ID3D11Device,
        nv12_texture: &ID3D11Texture2D,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> anyhow::Result<Self> {
        let device_ptr = device.as_raw();

        // 1. Open the session (NV_ENC_DEVICE_TYPE_DIRECTX).
        let mut session_params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
            device: device_ptr,
            apiVersion: NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut encoder_ptr: *mut c_void = std::ptr::null_mut();
        unsafe { (ENCODE_API.open_encode_session_ex)(&mut session_params, &mut encoder_ptr) }
            .result_without_string()
            .context("NvEncOpenEncodeSessionEx に失敗（DirectXデバイス）")?;

        // 2. Get the base config for the P1 preset + ULTRA_LOW_LATENCY.
        let mut preset_config = NV_ENC_PRESET_CONFIG {
            version: NV_ENC_PRESET_CONFIG_VER,
            presetCfg: NV_ENC_CONFIG {
                version: NV_ENC_CONFIG_VER,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            (ENCODE_API.get_encode_preset_config_ex)(
                encoder_ptr,
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P1_GUID,
                NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut preset_config,
            )
        }
        .result_without_string()
        .context("NvEncGetEncodePresetConfigEx に失敗")?;

        let mut encode_config = preset_config.presetCfg;
        encode_config.version = NV_ENC_CONFIG_VER;
        encode_config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
        // No B-frames (frameIntervalP=1).
        encode_config.frameIntervalP = 1;
        // Periodic IDR once per second (new viewers also get an on-demand
        // IDR separately, handled by the caller).
        encode_config.gopLength = fps;
        encode_config.rcParams.version = NV_ENC_RC_PARAMS_VER;
        encode_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        encode_config.rcParams.averageBitRate = bitrate_bps;
        encode_config.rcParams.maxBitRate = bitrate_bps;
        unsafe {
            // Resend SPS/PPS with every IDR so viewers joining mid-stream
            // can decode immediately.
            encode_config.encodeCodecConfig.h264Config.set_repeatSPSPPS(1);
        }

        // 3. Initialize the encoder.
        let mut init_params = NV_ENC_INITIALIZE_PARAMS {
            version: NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: NV_ENC_CODEC_H264_GUID,
            presetGUID: NV_ENC_PRESET_P1_GUID,
            encodeWidth: width,
            encodeHeight: height,
            darWidth: width,
            darHeight: height,
            frameRateNum: fps,
            frameRateDen: 1,
            enablePTD: 1,
            tuningInfo: NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            bufferFormat: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            encodeConfig: &mut encode_config,
            maxEncodeWidth: width,
            maxEncodeHeight: height,
            ..Default::default()
        };
        unsafe { (ENCODE_API.initialize_encoder)(encoder_ptr, &mut init_params) }
            .result_without_string()
            .context("NvEncInitializeEncoder に失敗")?;

        // 4. Register the NV12 texture as a DirectX resource. Cached here;
        // never re-registered per frame.
        let texture_ptr = nv12_texture.as_raw();
        let mut register_params = NV_ENC_REGISTER_RESOURCE {
            version: NV_ENC_REGISTER_RESOURCE_VER,
            resourceType: NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            width,
            height,
            pitch: 0,
            resourceToRegister: texture_ptr,
            bufferFormat: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            bufferUsage: nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        unsafe { (ENCODE_API.register_resource)(encoder_ptr, &mut register_params) }
            .result_without_string()
            .context("NvEncRegisterResource に失敗")?;

        // 5. Create the output bitstream buffer.
        let mut bitstream_params = NV_ENC_CREATE_BITSTREAM_BUFFER {
            version: NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
            ..Default::default()
        };
        unsafe { (ENCODE_API.create_bitstream_buffer)(encoder_ptr, &mut bitstream_params) }
            .result_without_string()
            .context("NvEncCreateBitstreamBuffer に失敗")?;

        Ok(Self {
            encoder_ptr,
            registered_resource: register_params.registeredResource,
            bitstream_buffer: bitstream_params.bitstreamBuffer,
            width,
            height,
            fps,
            frame_idx: 0,
            encode_config,
        })
    }

    /// Changes the target bitrate at runtime (bandwidth adaptation).
    /// `NvEncReconfigureEncoder` requires a full `NV_ENC_INITIALIZE_PARAMS`,
    /// so this rebuilds one with the same values as init, only the bitrate
    /// in `encode_config` changed.
    pub fn reconfigure_bitrate(&mut self, bitrate_bps: u32) -> anyhow::Result<()> {
        self.encode_config.rcParams.averageBitRate = bitrate_bps;
        self.encode_config.rcParams.maxBitRate = bitrate_bps;

        let init_params = NV_ENC_INITIALIZE_PARAMS {
            version: NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: NV_ENC_CODEC_H264_GUID,
            presetGUID: NV_ENC_PRESET_P1_GUID,
            encodeWidth: self.width,
            encodeHeight: self.height,
            darWidth: self.width,
            darHeight: self.height,
            frameRateNum: self.fps,
            frameRateDen: 1,
            enablePTD: 1,
            tuningInfo: NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            bufferFormat: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            encodeConfig: &mut self.encode_config,
            maxEncodeWidth: self.width,
            maxEncodeHeight: self.height,
            ..Default::default()
        };

        let mut reconfig_params = NV_ENC_RECONFIGURE_PARAMS {
            version: NV_ENC_RECONFIGURE_PARAMS_VER,
            reInitEncodeParams: init_params,
            ..Default::default()
        };
        unsafe { (ENCODE_API.reconfigure_encoder)(self.encoder_ptr, &mut reconfig_params) }
            .result_without_string()
            .context("NvEncReconfigureEncoder に失敗")?;
        Ok(())
    }

    /// Encodes one frame and returns the Annex-B H.264 byte stream.
    pub fn encode_frame(&mut self, force_idr: bool) -> anyhow::Result<Vec<u8>> {
        // Map/unmap the already-registered resource every frame
        // (registration itself is cached).
        let mut map_params = NV_ENC_MAP_INPUT_RESOURCE {
            version: NV_ENC_MAP_INPUT_RESOURCE_VER,
            registeredResource: self.registered_resource,
            ..Default::default()
        };
        unsafe { (ENCODE_API.map_input_resource)(self.encoder_ptr, &mut map_params) }
            .result_without_string()
            .context("NvEncMapInputResource に失敗")?;

        let mapped: NV_ENC_INPUT_PTR = map_params.mappedResource;

        let mut pic_params = NV_ENC_PIC_PARAMS {
            version: NV_ENC_PIC_PARAMS_VER,
            inputWidth: self.width,
            inputHeight: self.height,
            inputPitch: self.width,
            encodePicFlags: if force_idr {
                NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
            } else {
                0
            },
            frameIdx: self.frame_idx,
            inputBuffer: mapped,
            outputBitstream: self.bitstream_buffer,
            bufferFmt: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            pictureStruct: NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            ..Default::default()
        };
        let encode_result =
            unsafe { (ENCODE_API.encode_picture)(self.encoder_ptr, &mut pic_params) }
                .result_without_string();

        // Safe to unmap right after the encode call.
        unsafe { (ENCODE_API.unmap_input_resource)(self.encoder_ptr, mapped) };

        encode_result.context("NvEncEncodePicture に失敗")?;
        self.frame_idx += 1;

        // Read back only the small compressed bitstream; raw frames never
        // touch the CPU.
        let mut lock_params = NV_ENC_LOCK_BITSTREAM {
            version: NV_ENC_LOCK_BITSTREAM_VER,
            outputBitstream: self.bitstream_buffer,
            ..Default::default()
        };
        unsafe { (ENCODE_API.lock_bitstream)(self.encoder_ptr, &mut lock_params) }
            .result_without_string()
            .context("NvEncLockBitstream に失敗")?;

        let data = unsafe {
            std::slice::from_raw_parts(
                lock_params.bitstreamBufferPtr as *const u8,
                lock_params.bitstreamSizeInBytes as usize,
            )
            .to_vec()
        };

        unsafe { (ENCODE_API.unlock_bitstream)(self.encoder_ptr, self.bitstream_buffer) }
            .result_without_string()
            .context("NvEncUnlockBitstream に失敗")?;

        Ok(data)
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe {
            (ENCODE_API.destroy_bitstream_buffer)(self.encoder_ptr, self.bitstream_buffer);
            (ENCODE_API.unregister_resource)(self.encoder_ptr, self.registered_resource);
            (ENCODE_API.destroy_encoder)(self.encoder_ptr);
        }
    }
}
