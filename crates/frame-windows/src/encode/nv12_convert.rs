//! Same-device SDR BGRA8 → NV12 conversion shared by MF and its IPC parent.
//! Full-range BT.709 RGB → studio-range BT.709 YUV matches the original MF path.

use super::NativeEncoderConfig;
use frame_contracts::RecorderError;
use std::mem::ManuallyDrop;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

fn mf_error(api: &str, error: windows::core::Error) -> RecorderError {
    super::preparation_error(format!(
        "NV12 conversion {api} failed with HRESULT 0x{:08X}.",
        error.code().0
    ))
}

pub(super) struct Nv12Input {
    _source: ID3D11Texture2D,
    pub(super) nv12: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    output_view: ID3D11VideoProcessorOutputView,
}

#[derive(Clone)]
pub(super) struct Nv12Converter {
    device: ID3D11Device,
    video_context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    width: u32,
    height: u32,
}

impl Nv12Converter {
    pub(super) fn new(
        device: &ID3D11Device,
        config: &NativeEncoderConfig,
    ) -> Result<Self, RecorderError> {
        config.validate()?;
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|e| mf_error("GetImmediateContext", e))?;
        let multithread: ID3D11Multithread = context
            .cast()
            .map_err(|e| mf_error("ID3D11Multithread", e))?;
        let _previous_protection = unsafe { multithread.SetMultithreadProtected(true) };
        if !unsafe { multithread.GetMultithreadProtected() }.as_bool() {
            return Err(super::preparation_error(
                "NV12 conversion requires a protected D3D11 context.".into(),
            ));
        }
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|error| mf_error("ID3D11VideoDevice", error))?;
        let video_context: ID3D11VideoContext1 = context
            .cast()
            .map_err(|error| mf_error("ID3D11VideoContext1", error))?;
        let description = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: config.rate_num.get(),
                Denominator: config.rate_den.get(),
            },
            InputWidth: config.width,
            InputHeight: config.height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: config.rate_num.get(),
                Denominator: config.rate_den.get(),
            },
            OutputWidth: config.width,
            OutputHeight: config.height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&description) }
            .map_err(|error| mf_error("CreateVideoProcessorEnumerator", error))?;
        let bgra = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM) }
            .map_err(|error| mf_error("CheckVideoProcessorFormat(BGRA)", error))?;
        let nv12 = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12) }
            .map_err(|error| mf_error("CheckVideoProcessorFormat(NV12)", error))?;
        if bgra & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
            || nv12 & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
        {
            return Err(super::preparation_error(
                "Capture adapter's D3D11 video processor lacks BGRA input or NV12 output.".into(),
            ));
        }
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|error| mf_error("CreateVideoProcessor", error))?;
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video_context.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            video_context.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
        }
        Ok(Self {
            device: device.clone(),
            video_context,
            enumerator,
            processor,
            width: config.width,
            height: config.height,
        })
    }

    pub(super) fn prepare(&self, source: &ID3D11Texture2D) -> Result<Nv12Input, RecorderError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        if desc.Width != self.width
            || desc.Height != self.height
            || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            || desc.SampleDesc.Count != 1
            || desc.MipLevels != 1
            || desc.ArraySize != 1
        {
            return Err(super::invalid_request_error(
                "NV12 conversion requires a matching single-sample BGRA8 texture.".into(),
            ));
        }
        let source_resource: ID3D11Resource = source
            .cast()
            .map_err(|error| mf_error("ID3D11Resource(source)", error))?;
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous:
                windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            ..Default::default()
        };
        let mut input_view = None;
        let video_device: ID3D11VideoDevice = self
            .device
            .cast()
            .map_err(|error| mf_error("ID3D11VideoDevice", error))?;
        unsafe {
            video_device.CreateVideoProcessorInputView(
                &source_resource,
                &self.enumerator,
                &input_desc,
                Some(&mut input_view),
            )
        }
        .map_err(|error| mf_error("CreateVideoProcessorInputView", error))?;
        let input_view = input_view
            .ok_or_else(|| super::internal_error("D3D11 returned no video input view.".into()))?;
        let nv12_desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut nv12 = None;
        unsafe {
            self.device
                .CreateTexture2D(&nv12_desc, None, Some(&mut nv12))
        }
        .map_err(|error| mf_error("CreateTexture2D(NV12)", error))?;
        let nv12 =
            nv12.ok_or_else(|| super::internal_error("D3D11 returned no NV12 texture.".into()))?;
        let nv12_resource: ID3D11Resource = nv12
            .cast()
            .map_err(|error| mf_error("ID3D11Resource(NV12)", error))?;
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous:
                windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
        };
        let mut output_view = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &nv12_resource,
                &self.enumerator,
                &output_desc,
                Some(&mut output_view),
            )
        }
        .map_err(|error| mf_error("CreateVideoProcessorOutputView", error))?;
        let output_view = output_view
            .ok_or_else(|| super::internal_error("D3D11 returned no video output view.".into()))?;
        Ok(Nv12Input {
            _source: source.clone(),
            input_view,
            nv12,
            output_view,
        })
    }

    /// Enqueues GPU conversion. The caller's following Flush/Map or MFT consumes it.
    pub(super) fn convert(&self, slot: &Nv12Input) -> Result<(), RecorderError> {
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(slot.input_view.clone())),
            ..Default::default()
        };
        let converted = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &slot.output_view,
                0,
                std::slice::from_ref(&stream),
            )
        }
        .map_err(|error| mf_error("ID3D11VideoContext::VideoProcessorBlt", error));
        // SAFETY: the stream does not own a Rust drop impl; its COM clone is released
        // immediately after the synchronous API returns.
        drop(unsafe { ManuallyDrop::take(&mut stream.pInputSurface) });
        converted
    }
}
