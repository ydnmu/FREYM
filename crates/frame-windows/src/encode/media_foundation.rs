//! Adapter-scoped Media Foundation hardware H.264 transform and session.
//!
//! The preparation diagnostic does not publish an EncoderCapability: activation
//! and media-type negotiation alone do not prove input submission and drain.

use std::ffi::c_void;
use windows::core::{GUID, HRESULT, Interface};

use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFTransform, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateAttributes, MFCreateMediaType,
    MFMediaType_Video, MFSTARTUP_FULL, MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFVideoFormat_AV1, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::CoTaskMemFree;

use super::driver::{
    NativeCollect, NativeEos, NativePacket, NativeStats, NativeSubmit, NvencDriver,
};
use super::{
    MAX_ACCESS_UNIT_BYTES, MAX_CODEC_CONFIG_BYTES, NativeEncoderPreset, NativeEncoderSettings,
    NativeRateControl,
};
use super::{NativeAdapterIdentity, NativeEncoderConfig};
use crate::surface::EncodeTexture;
use crate::surface::SurfaceIdentity;
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonLowLatency, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFDXGIDeviceManager, IMFMediaBuffer,
    IMFMediaEventGenerator, IMFMediaType, IMFShutdown, METransformDrainComplete,
    METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_MT_MPEG_SEQUENCE_HEADER,
    MF_SA_D3D11_AWARE, MFCreateMemoryBuffer, MFCreateSample, MFSampleExtension_CleanPoint,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
};
use windows::Win32::System::Variant::{
    VARIANT, VT_BOOL, VT_EMPTY, VT_I4, VT_UI4, VariantClear, VariantToBoolean, VariantToUInt32,
};

/// One activation's bounded preparation outcome. This is intentionally not a
/// qualified recording encoder until a GPU input, output and EOS are proved.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MfH264Preparation {
    pub name: String,
    pub adapter_luid: u64,
    pub media_types_accepted: bool,
    pub detail: String,
}

struct Foundation;
impl Foundation {
    fn start() -> Result<Self, RecorderError> {
        // SAFETY: MFStartup is process-refcounted; this guard balances its own call.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .map_err(|error| mf_error("MFStartup", error))?;
        Ok(Self)
    }
}
impl Drop for Foundation {
    fn drop(&mut self) {
        // SAFETY: balances a successful MFStartup on this diagnostic thread.
        let _ = unsafe { MFShutdown() };
    }
}

struct Activations {
    data: *mut Option<IMFActivate>,
    count: usize,
}
impl Activations {
    fn iter(&self) -> impl Iterator<Item = &IMFActivate> {
        // SAFETY: MFTEnum2 produced count initialized COM pointer slots in this
        // CoTaskMemAlloc allocation; the guard retains them until iteration ends.
        let items: &[Option<IMFActivate>] = if self.count == 0 {
            &[]
        } else {
            // SAFETY: nonzero count requires a valid allocation from MFTEnum2.
            unsafe { std::slice::from_raw_parts(self.data, self.count) }
        };
        items.iter().filter_map(Option::as_ref)
    }
}
impl Drop for Activations {
    fn drop(&mut self) {
        if !self.data.is_null() {
            // SAFETY: clear each owned COM interface, then free exactly the array
            // returned by MFTEnum2 using its documented allocator.
            for item in unsafe { std::slice::from_raw_parts_mut(self.data, self.count) } {
                item.take();
            }
            unsafe { CoTaskMemFree(Some(self.data.cast::<c_void>())) };
        }
    }
}

fn mf_error(operation: &str, error: windows::core::Error) -> RecorderError {
    RecorderError {
        code: ErrorCode::EncoderUnavailable,
        message: format!("{operation} failed with HRESULT 0x{:08X}.", error.code().0),
        recovery: RecoveryStatus::NoRecording,
        action: "Use a supported hardware encoder on the capture adapter.".into(),
    }
}

fn packed(upper: u32, lower: u32) -> u64 {
    (u64::from(upper) << 32) | u64::from(lower)
}

/// MFTEnum2 appeared after older Windows builds supported by the executable.
/// Resolve this optional entry point only when MF hardware encoding is requested.
fn enum_hardware_h264(
    attributes: &IMFAttributes,
    output_type: &MFT_REGISTER_TYPE_INFO,
    data: &mut *mut Option<IMFActivate>,
    count: &mut u32,
) -> Result<(), RecorderError> {
    type MftEnum2 = unsafe extern "system" fn(
        GUID,
        MFT_ENUM_FLAG,
        *const MFT_REGISTER_TYPE_INFO,
        *const MFT_REGISTER_TYPE_INFO,
        *mut c_void,
        *mut *mut Option<IMFActivate>,
        *mut u32,
    ) -> HRESULT;
    // SAFETY: the symbol is called only while its system-DLL owner is live;
    // parameters use the official windows bindings.
    let library = optional_mfplat()?;
    // SAFETY: MFTEnum2's system ABI and exact documented parameters are represented
    // by the official binding types above. A missing symbol is an ordinary error.
    let function: libloading::Symbol<MftEnum2> =
        unsafe { library.get(b"MFTEnum2\0") }.map_err(|_| RecorderError {
            code: ErrorCode::EncoderUnavailable,
            message:
                "This Windows build does not export MFTEnum2 for adapter-scoped hardware encoding."
                    .into(),
            recovery: RecoveryStatus::NoRecording,
            action: "Use Windows 10 1703 or newer with an adapter-scoped hardware MFT.".into(),
        })?;
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    // SAFETY: output pointers are valid and the attribute COM reference remains
    // alive for the call. An owned activation array is returned on success.
    unsafe {
        function(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            std::ptr::null(),
            output_type,
            attributes.as_raw(),
            data,
            count,
        )
        .ok()
    }
    .map_err(|error| mf_error("MFTEnum2(hardware H264, adapter LUID)", error))
}

fn configure(transform: &IMFTransform, config: &NativeEncoderConfig) -> Result<(), RecorderError> {
    configure_profile(transform, config, 0)
}
fn output_subtype(codec: u32) -> GUID {
    if codec == 2 {
        MFVideoFormat_AV1
    } else {
        MFVideoFormat_H264
    }
}
fn configure_profile(
    transform: &IMFTransform,
    config: &NativeEncoderConfig,
    codec: u32,
) -> Result<(), RecorderError> {
    // Hardware MFTs are asynchronous; unlock before asking for media types.
    // SAFETY: the transform owns the returned attributes for this call.
    let attributes = unsafe { transform.GetAttributes() }
        .map_err(|error| mf_error("IMFTransform::GetAttributes", error))?;
    // SAFETY: a documented UINT32 attribute, set before media-type negotiation.
    unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
        .map_err(|error| mf_error("MF_TRANSFORM_ASYNC_UNLOCK", error))?;
    // SAFETY: MFCreateMediaType returns distinct owned media types.
    let output = unsafe { MFCreateMediaType() }
        .map_err(|error| mf_error("MFCreateMediaType(output)", error))?;
    let input = unsafe { MFCreateMediaType() }
        .map_err(|error| mf_error("MFCreateMediaType(input)", error))?;
    for (media_type, subtype) in [
        (&output, &output_subtype(codec)),
        (&input, &MFVideoFormat_NV12),
    ] {
        // SAFETY: all keys and values are official Media Foundation GUIDs; the packed
        // values use the documented high/low UINT32 media-type encoding.
        unsafe {
            media_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|_| media_type.SetGUID(&MF_MT_SUBTYPE, subtype))
                .and_then(|_| {
                    media_type.SetUINT64(&MF_MT_FRAME_SIZE, packed(config.width, config.height))
                })
                .and_then(|_| {
                    media_type.SetUINT64(
                        &MF_MT_FRAME_RATE,
                        packed(config.rate_num.get(), config.rate_den.get()),
                    )
                })
                .and_then(|_| {
                    media_type
                        .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                })
        }
        .map_err(|error| mf_error("IMFMediaType configuration", error))?;
    }
    if codec == 2 {
        use windows::Win32::Media::MediaFoundation::{
            MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
            MF_MT_YUV_MATRIX, MFNominalRange_16_235, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
            MFVideoTransferMatrix_BT709,
        };
        for media in [&input, &output] {
            unsafe {
                media
                    .SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)
                    .and_then(|_| {
                        media.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)
                    })
                    .and_then(|_| {
                        media.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
                    })
                    .and_then(|_| {
                        media.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
                    })
            }
            .map_err(|e| mf_error("MF AV1 colour signal", e))?;
        }
    }
    // SAFETY: the requested positive bitrate was validated by NativeEncoderConfig.
    unsafe { output.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps) }
        .map_err(|error| mf_error("MF_MT_AVG_BITRATE", error))?;
    // The documented H.264 MFT requires output before input.
    // SAFETY: the transform and both media types remain live through negotiation.
    unsafe { transform.SetOutputType(0, &output, 0) }
        .map_err(|error| mf_error("IMFTransform::SetOutputType(H264)", error))?;
    unsafe { transform.SetInputType(0, &input, 0) }
        .map_err(|error| mf_error("IMFTransform::SetInputType(NV12)", error))?;
    Ok(())
}

/// Enumerate and configure hardware H.264 MFTs associated with one DXGI LUID.
/// This preparation reports only media-type acceptance; recording qualification
/// separately requires a same-device GPU input, H.264 output and completed drain.
pub fn prepare_hardware_h264(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
) -> Result<Vec<MfH264Preparation>, RecorderError> {
    config.validate()?;
    let _apartment = crate::capture::native::ComApartment::initialize_mta()?;
    let _foundation = Foundation::start()?;
    let activations = enumerate_activations(adapter)?;
    let mut found = Vec::new();
    for activation in activations.iter() {
        // SAFETY: the activation owns the attribute value during the read.
        let name = unsafe { activation.GetStringLength(&MFT_FRIENDLY_NAME_Attribute) }
            .ok()
            .filter(|length| *length <= 512)
            .and_then(|length| {
                let mut wide = vec![0u16; length as usize + 1];
                // SAFETY: the buffer has room for the documented NUL terminator.
                unsafe { activation.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut wide, None) }
                    .ok()
                    .map(|_| String::from_utf16_lossy(&wide[..length as usize]))
            })
            .unwrap_or_else(|| "unnamed hardware MFT".into());
        // SAFETY: activation returns an owned transform. Configuration errors stay
        // attached to this activation and do not become a usable capability.
        let result = unsafe { activation.ActivateObject::<IMFTransform>() }
            .map_err(|error| mf_error("IMFActivate::ActivateObject", error))
            .and_then(|transform| configure(&transform, config));
        // The diagnostic does not retain an activated object.
        let _ = unsafe { activation.ShutdownObject() };
        found.push(MfH264Preparation {
            name,
            adapter_luid: adapter.luid,
            media_types_accepted: result.is_ok(),
            detail: result.map_or_else(|error| error.message, |_| {
                "H.264 output and NV12 input types accepted; this diagnostic does not qualify a recording session".into()
            }),
        });
    }
    Ok(found)
}

fn enumerate_activations(adapter: &NativeAdapterIdentity) -> Result<Activations, RecorderError> {
    enumerate_profile_activations(adapter, 0)
}
fn enumerate_profile_activations(
    adapter: &NativeAdapterIdentity,
    codec: u32,
) -> Result<Activations, RecorderError> {
    let mut attributes: Option<IMFAttributes> = None;
    // SAFETY: a valid out pointer receives one owned attribute store.
    unsafe { MFCreateAttributes(&mut attributes, 1) }
        .map_err(|error| mf_error("MFCreateAttributes", error))?;
    let attributes =
        attributes.ok_or_else(|| RecorderError::invalid("MFCreateAttributes returned no store"))?;
    // MFT_ENUM_ADAPTER_LUID is a LUID blob: two native-endian 32-bit halves.
    // SAFETY: SetBlob copies the eight bytes before the local array is dropped.
    unsafe { attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &adapter.luid.to_ne_bytes()) }
        .map_err(|error| mf_error("MFT_ENUM_ADAPTER_LUID", error))?;
    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: output_subtype(codec),
    };
    let mut data = std::ptr::null_mut();
    let mut count = 0u32;
    // The guard below owns the returned COM array on success.
    enum_hardware_h264(&attributes, &output_type, &mut data, &mut count)?;
    if count != 0 && data.is_null() {
        return Err(RecorderError::invalid(
            "MFTEnum2 reported encoder activations without an activation array",
        ));
    }
    if count > 64 {
        let _cleanup = Activations {
            data,
            count: count as usize,
        };
        return Err(RecorderError::invalid(
            "Media Foundation returned more than 64 hardware encoder activations",
        ));
    }
    Ok(Activations {
        data,
        count: count as usize,
    })
}

fn codec_property_name(key: &GUID) -> &'static str {
    if key == &CODECAPI_AVEncCommonRateControlMode {
        "CODECAPI_AVEncCommonRateControlMode"
    } else if key == &CODECAPI_AVEncMPVDefaultBPictureCount {
        "CODECAPI_AVEncMPVDefaultBPictureCount"
    } else if key == &CODECAPI_AVEncMPVGOPSize {
        "CODECAPI_AVEncMPVGOPSize"
    } else if key == &CODECAPI_AVEncCommonLowLatency {
        "CODECAPI_AVEncCommonLowLatency"
    } else if key == &CODECAPI_AVEncVideoForceKeyFrame {
        "CODECAPI_AVEncVideoForceKeyFrame"
    } else {
        "unknown CODECAPI property"
    }
}

/// Read only scalar fields matching the actual discriminant. Other native types
/// are reported by tag, never dereferenced or converted into unbounded text.
fn codec_variant_detail(value: &VARIANT) -> String {
    // SAFETY: VARIANT's tag is initialized by its scalar constructor or COM out
    // parameter; only the union field corresponding to that tag is read.
    unsafe {
        let scalar = &*value.Anonymous.Anonymous;
        match scalar.vt {
            VT_EMPTY => "VT_EMPTY".into(),
            VT_UI4 => format!("VT_UI4:{}", scalar.Anonymous.ulVal),
            VT_I4 => format!("VT_I4:{}", scalar.Anonymous.lVal),
            VT_BOOL => format!("VT_BOOL:{}", scalar.Anonymous.boolVal.0),
            other => format!("VT({})", other.0),
        }
    }
}

fn clear_codec_diagnostic(value: &mut VARIANT) -> String {
    // SAFETY: the initialized out parameter belongs to this call. It is cleared
    // even when the codec's query failed after partially initializing it.
    match unsafe { VariantClear(value) } {
        Ok(()) => String::new(),
        Err(error) => format!("/clear=0x{:08X}", error.code().0),
    }
}

/// Failure-only, bounded capability evidence. It does not retry a rejected
/// setting, choose a different value, or turn unsupported settings into success.
fn codec_property_error(
    codec: &ICodecAPI,
    key: &GUID,
    requested: &VARIANT,
    operation: &str,
    error: windows::core::Error,
) -> RecorderError {
    // SAFETY: use the official vtable to retain S_FALSE. The generated
    // IsSupported Result<()> erases this documented unsupported outcome.
    let supported = unsafe { (codec.vtable().IsSupported)(codec.as_raw(), key) };
    let modifiable = unsafe { codec.IsModifiable(key) };
    let mut minimum = VARIANT::default();
    let mut maximum = VARIANT::default();
    let mut step = VARIANT::default();
    // SAFETY: three owned initialized variants receive the native range; their
    // allocations are cleared below regardless of the returned HRESULT.
    let range = unsafe {
        (codec.vtable().GetParameterRange)(
            codec.as_raw(),
            key,
            &mut minimum,
            &mut maximum,
            &mut step,
        )
    };
    let range_detail = format!(
        "{}/{}/{}",
        codec_variant_detail(&minimum),
        codec_variant_detail(&maximum),
        codec_variant_detail(&step),
    );
    let clear_range = format!(
        "{}{}{}",
        clear_codec_diagnostic(&mut minimum),
        clear_codec_diagnostic(&mut maximum),
        clear_codec_diagnostic(&mut step),
    );
    let mut current = VARIANT::default();
    // SAFETY: an owned initialized VARIANT receives any readback. Its tag is
    // inspected without coercion and its native allocation is always cleared.
    let current_result = unsafe { (codec.vtable().GetValue)(codec.as_raw(), key, &mut current) };
    let current_detail = codec_variant_detail(&current);
    let clear_current = clear_codec_diagnostic(&mut current);
    let mut result = mf_error(operation, error);
    result.message = format!(
        "{} Property {} ({key:?}); requested={}; IsSupported=0x{:08X}; \
         IsModifiable=0x{:08X}; range=0x{:08X}[{}]{}; readback=0x{:08X}[{}]{}.",
        result.message,
        codec_property_name(key),
        codec_variant_detail(requested),
        supported.0,
        modifiable.0,
        range.0,
        range_detail,
        clear_range,
        current_result.0,
        current_detail,
        clear_current,
    );
    result
}

fn codec_value(codec: &ICodecAPI, key: &GUID) -> Result<u32, RecorderError> {
    // SAFETY: the returned VARIANT is owned and is cleared after conversion.
    let mut value =
        unsafe { codec.GetValue(key) }.map_err(|error| mf_error("ICodecAPI::GetValue", error))?;
    let number =
        unsafe { VariantToUInt32(&value) }.map_err(|error| mf_error("VariantToUInt32", error));
    let cleared =
        unsafe { VariantClear(&mut value) }.map_err(|error| mf_error("VariantClear", error));
    cleared?;
    number
}

fn set_codec_u32(codec: &ICodecAPI, key: &GUID, number: u32) -> Result<(), RecorderError> {
    // SAFETY: this union contains only a scalar VT_UI4; no allocation needs clearing.
    let mut value = VARIANT::default();
    unsafe {
        (*value.Anonymous.Anonymous).vt = VT_UI4;
        (*value.Anonymous.Anonymous).Anonymous.ulVal = number;
    }
    unsafe { codec.SetValue(key, &value) }
        .map_err(|error| codec_property_error(codec, key, &value, "ICodecAPI::SetValue", error))?;
    let applied = codec_value(codec, key)?;
    if applied != number {
        return Err(super::preparation_error(format!(
            "Media Foundation encoder reported codec property {key:?}={applied}, requested {number}."
        )));
    }
    Ok(())
}

fn force_next_keyframe(transform: &IMFTransform) -> Result<(), RecorderError> {
    let codec: ICodecAPI = transform
        .cast()
        .map_err(|error| mf_error("IMFTransform::QueryInterface(ICodecAPI)", error))?;
    let mut value = VARIANT::default();
    // SAFETY: the one-shot property takes scalar VT_UI4 and owns no allocation.
    unsafe {
        (*value.Anonymous.Anonymous).vt = VT_UI4;
        (*value.Anonymous.Anonymous).Anonymous.ulVal = 1;
        codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &value)
    }
    .map_err(|error| {
        codec_property_error(
            &codec,
            &CODECAPI_AVEncVideoForceKeyFrame,
            &value,
            "ICodecAPI::SetValue(force keyframe)",
            error,
        )
    })
}

fn set_codec_bool(codec: &ICodecAPI, key: &GUID, enabled: bool) -> Result<(), RecorderError> {
    let mut value = VARIANT::default();
    // SAFETY: scalar VT_BOOL contains no owned allocation.
    unsafe {
        (*value.Anonymous.Anonymous).vt = VT_BOOL;
        (*value.Anonymous.Anonymous).Anonymous.boolVal = if enabled {
            windows::Win32::Foundation::VARIANT_TRUE
        } else {
            windows::Win32::Foundation::VARIANT_FALSE
        };
    }
    unsafe { codec.SetValue(key, &value) }.map_err(|error| {
        codec_property_error(codec, key, &value, "ICodecAPI::SetValue(VT_BOOL)", error)
    })?;
    let mut applied = unsafe { codec.GetValue(key) }
        .map_err(|error| mf_error("ICodecAPI::GetValue(VT_BOOL)", error))?;
    let readback =
        unsafe { VariantToBoolean(&applied) }.map_err(|error| mf_error("VariantToBoolean", error));
    let cleared =
        unsafe { VariantClear(&mut applied) }.map_err(|error| mf_error("VariantClear", error));
    cleared?;
    if readback?.as_bool() != enabled {
        return Err(super::preparation_error(format!(
            "MF codec property {key:?} did not apply requested {enabled}."
        )));
    }
    Ok(())
}

fn configure_codec(
    transform: &IMFTransform,
    config: &NativeEncoderConfig,
    selected_codec: u32,
) -> Result<(), RecorderError> {
    if !config.encoder_overrides.is_empty()
        || config.rate_control != NativeRateControl::Cbr
        || config.peak_bitrate_bps != config.bitrate_bps
        || config.preset != NativeEncoderPreset::LowLatency
    {
        return Err(RecorderError {
            code: ErrorCode::EncoderUnavailable,
            message: "This MF path supports only explicitly applied CBR and low-latency settings; the requested VBR, peak bitrate or NVENC preset has no equivalent readback.".into(),
            recovery: RecoveryStatus::NoRecording,
            action: "Choose Standard quality with CBR for this hardware encoder.".into(),
        });
    }
    let codec: ICodecAPI = transform
        .cast()
        .map_err(|error| mf_error("IMFTransform::QueryInterface(ICodecAPI)", error))?;
    set_codec_u32(&codec, &CODECAPI_AVEncCommonRateControlMode, 0)?;
    // MPEG B-picture control applies to AVC; AV1 has shown/hidden reference
    // frames instead. Its shared OBU validator and timestamp accounting reject
    // hidden/show_existing/reordered output before it can reach the mux.
    if selected_codec == 0 {
        set_codec_u32(&codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0)?;
    }
    set_codec_u32(&codec, &CODECAPI_AVEncMPVGOPSize, config.gop_frames)?;
    set_codec_bool(&codec, &CODECAPI_AVEncCommonLowLatency, true)?;
    Ok(())
}

fn av1_header(data: &[u8]) -> Result<Vec<u8>, RecorderError> {
    let payload =
        frame_encode::av1::sequence_payload_from_packet(data).map_err(super::preparation_error)?;
    let mut header = vec![0x0a];
    let mut size = payload.len();
    loop {
        let byte = (size & 127) as u8;
        size >>= 7;
        header.push(byte | if size != 0 { 128 } else { 0 });
        if size == 0 {
            break;
        }
    }
    header.extend_from_slice(payload);
    let (_, sequence) =
        frame_encode::av1::configuration(&header).map_err(super::preparation_error)?;
    if (
        sequence.depth,
        sequence.primaries,
        sequence.transfer,
        sequence.matrix,
        sequence.range,
    ) != (8, 1, 1, 1, 1)
    {
        return Err(super::preparation_error(
            "MF AV1 did not preserve its requested 8-bit BT.709 limited signal.".into(),
        ));
    }
    Ok(header)
}
fn av1_picture(data: &[u8], config: Option<&[u8]>) -> Result<bool, String> {
    frame_encode::av1::validate_packet(
        data,
        config
            .map(frame_encode::av1::sequence_payload)
            .transpose()?,
    )
}

fn codec_config(output: &IMFMediaType) -> Result<Vec<u8>, RecorderError> {
    // SAFETY: size is bounded before allocating; GetBlob fills that owned buffer.
    let len = unsafe { output.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }
        .map_err(|error| mf_error("MF_MT_MPEG_SEQUENCE_HEADER size", error))?
        as usize;
    if len == 0 || len > MAX_CODEC_CONFIG_BYTES {
        return Err(super::preparation_error(format!(
            "MF H.264 sequence header is {len} bytes; expected 1..={MAX_CODEC_CONFIG_BYTES}."
        )));
    }
    let mut bytes = vec![0u8; len];
    unsafe { output.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut bytes, None) }
        .map_err(|error| mf_error("MF_MT_MPEG_SEQUENCE_HEADER", error))?;
    if !annexb_has_nal(&bytes, 7) || !annexb_has_nal(&bytes, 8) {
        return Err(super::preparation_error(
            "MF H.264 sequence header lacks Annex-B SPS/PPS NAL units.".into(),
        ));
    }
    Ok(bytes)
}

fn annexb_has_nal(bytes: &[u8], target: u8) -> bool {
    (0..bytes.len().saturating_sub(3)).any(|index| {
        let start = if bytes[index..].starts_with(&[0, 0, 1]) {
            Some(index + 3)
        } else if bytes[index..].starts_with(&[0, 0, 0, 1]) {
            Some(index + 4)
        } else {
            None
        };
        start
            .and_then(|at| bytes.get(at))
            .is_some_and(|nal| nal & 0x1f == target)
    })
}

fn extract_annexb_header(bytes: &[u8]) -> Result<Vec<u8>, RecorderError> {
    let mut starts = Vec::new();
    let mut index = 0usize;
    while index + 3 < bytes.len() {
        let prefix = if bytes[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        starts.push((index, prefix));
        index += prefix;
    }
    let mut header = Vec::new();
    let mut sps = false;
    let mut pps = false;
    for (number, &(start, prefix)) in starts.iter().enumerate() {
        let end = starts.get(number + 1).map_or(bytes.len(), |next| next.0);
        let kind = bytes.get(start + prefix).map(|byte| byte & 0x1f);
        if matches!(kind, Some(7 | 8)) {
            sps |= kind == Some(7);
            pps |= kind == Some(8);
            header.extend_from_slice(&bytes[start..end]);
        }
    }
    if !sps || !pps || header.len() > MAX_CODEC_CONFIG_BYTES {
        return Err(super::preparation_error(
            "MF output did not contain bounded Annex-B SPS/PPS configuration.".into(),
        ));
    }
    Ok(header)
}

enum MfRegistration {
    Bgra(super::nv12_convert::Nv12Input),
    Nv12(ID3D11Texture2D),
}

impl MfRegistration {
    fn nv12(&self) -> &ID3D11Texture2D {
        match self {
            Self::Bgra(input) => &input.nv12,
            Self::Nv12(texture) => texture,
        }
    }
}

struct Submitted {
    slot: u32,
    pts_ns: u64,
    duration_ns: u64,
    force_idr: bool,
}

fn optional_mfplat() -> Result<libloading::Library, RecorderError> {
    // SAFETY: restrict this late-loaded OS DLL and its dependencies to System32.
    // Older Windows builds can load the app without this path; an unavailable
    // loader flag or MF DLL fails explicitly only when encoding is requested.
    unsafe {
        libloading::os::windows::Library::load_with_flags(
            "mfplat.dll",
            libloading::os::windows::LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    }
    .map(libloading::Library::from)
    .map_err(|error| RecorderError {
        code: ErrorCode::EncoderUnavailable,
        message: format!("Media Foundation platform DLL is unavailable: {error}"),
        recovery: RecoveryStatus::NoRecording,
        action: "Use a Windows installation with Media Foundation hardware encoding.".into(),
    })
}

fn create_dxgi_manager(
    library: &libloading::Library,
) -> Result<(IMFDXGIDeviceManager, u32), RecorderError> {
    type Create = unsafe extern "system" fn(*mut u32, *mut Option<IMFDXGIDeviceManager>) -> HRESULT;
    // SAFETY: exact mfplat system ABI. A missing optional symbol is reported.
    let create: libloading::Symbol<Create> = unsafe { library.get(b"MFCreateDXGIDeviceManager\0") }
        .map_err(|_| {
            super::preparation_error("This Windows build lacks MFCreateDXGIDeviceManager.".into())
        })?;
    let mut token = 0u32;
    let mut manager = None;
    unsafe { create(&mut token, &mut manager).ok() }
        .map_err(|error| mf_error("MFCreateDXGIDeviceManager", error))?;
    let manager = manager
        .ok_or_else(|| super::preparation_error("MF returned no DXGI device manager.".into()))?;
    Ok((manager, token))
}

fn create_dxgi_surface_buffer(
    library: &libloading::Library,
    texture: &ID3D11Texture2D,
) -> Result<IMFMediaBuffer, RecorderError> {
    type Create = unsafe extern "system" fn(
        *const GUID,
        *mut c_void,
        u32,
        windows::core::BOOL,
        *mut Option<IMFMediaBuffer>,
    ) -> HRESULT;
    // SAFETY: exact mfplat system ABI; texture and library stay live for the call.
    let create: libloading::Symbol<Create> = unsafe { library.get(b"MFCreateDXGISurfaceBuffer\0") }
        .map_err(|_| {
            super::preparation_error("This Windows build lacks MFCreateDXGISurfaceBuffer.".into())
        })?;
    let mut buffer = None;
    unsafe {
        create(
            &ID3D11Texture2D::IID,
            texture.as_raw(),
            0,
            false.into(),
            &mut buffer,
        )
        .ok()
    }
    .map_err(|error| mf_error("MFCreateDXGISurfaceBuffer(NV12)", error))?;
    buffer.ok_or_else(|| super::internal_error("MF returned no DXGI surface buffer.".into()))
}

/// One worker-owned GPU MFT session. The public PacketEncoder's existing worker
/// retains capture leases, packet credits, and controls teardown around this driver.
pub(super) struct MfDriver {
    transform: IMFTransform,
    activation: IMFActivate,
    events: IMFMediaEventGenerator,
    _manager: IMFDXGIDeviceManager,
    mfplat: Arc<libloading::Library>,
    device: ID3D11Device,
    converter: super::nv12_convert::Nv12Converter,
    registrations: Vec<Option<MfRegistration>>,
    submitted: VecDeque<Submitted>,
    ready: VecDeque<(u32, NativePacket)>,
    need_input: u32,
    drain_complete: bool,
    draining: bool,
    closed: bool,
    cleanup_unsafe: bool,
    output_stream_size: u32,
    output_supplies_sample: bool,
    settings: NativeEncoderSettings,
    _foundation: Foundation,
    _apartment: crate::capture::native::ComApartment,
}

// SAFETY: construction, every method call, and destruction occur on the
// PacketEncoder's single MTA worker. The Send bound only transfers the freshly
// constructed driver into that worker's local trait object; no COM call is made
// from the control thread or concurrently.
unsafe impl Send for MfDriver {}

impl MfDriver {
    fn prime_codec_config(&mut self, config: &NativeEncoderConfig) -> Result<(), RecorderError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: config.width,
            Height: config.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|error| mf_error("CreateTexture2D(MF priming)", error))?;
        let texture = texture
            .ok_or_else(|| super::internal_error("D3D11 returned no priming texture.".into()))?;
        let resource: ID3D11Resource = texture
            .cast()
            .map_err(|error| mf_error("ID3D11Resource(priming)", error))?;
        let mut view = None;
        unsafe {
            self.device
                .CreateRenderTargetView(&resource, None, Some(&mut view))
        }
        .map_err(|error| mf_error("CreateRenderTargetView(priming)", error))?;
        let view = view.ok_or_else(|| {
            super::internal_error("D3D11 returned no priming render target.".into())
        })?;
        let context = unsafe { self.device.GetImmediateContext() }
            .map_err(|error| mf_error("GetImmediateContext(priming)", error))?;
        unsafe {
            context.ClearRenderTargetView(&view, &[0.16, 0.28, 0.42, 1.0]);
            context.Flush();
        }
        let registered = self.register_input(EncodeTexture {
            device: self.device.as_raw(),
            texture: texture.as_raw(),
            identity: SurfaceIdentity {
                pool: 0,
                device: self.device.as_raw() as usize,
                texture: texture.as_raw() as usize,
                width: config.width,
                height: config.height,
            },
        })?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let slot = loop {
            match self.submit(registered, 0, 16_666_667, true)? {
                NativeSubmit::Pending { slot } | NativeSubmit::Buffered { slot } => break slot,
                NativeSubmit::Busy if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                NativeSubmit::Busy => {
                    return Err(super::preparation_error(
                        "MF priming never requested input.".into(),
                    ));
                }
            }
        };
        self.begin_eos()?;
        let mut packet = None;
        let mut drained = false;
        while Instant::now() < deadline {
            if packet.is_none()
                && let NativeCollect::Ready(value) =
                    self.collect(slot, 5, MAX_ACCESS_UNIT_BYTES as usize)?
            {
                packet = Some(value);
            }
            if !drained {
                drained = self.poll_eos(5)?;
            }
            if packet.is_some() && drained {
                break;
            }
        }
        let packet = packet.ok_or_else(|| {
            super::preparation_error("MF priming produced no H.264 packet.".into())
        })?;
        if !drained || !packet.keyframe || packet.pts_ns != 0 {
            return Err(super::preparation_error(
                "MF priming did not produce a zero-PTS IDR and drain.".into(),
            ));
        }
        let output = unsafe { self.transform.GetOutputCurrentType(0) }
            .map_err(|error| mf_error("GetOutputCurrentType(after priming)", error))?;
        let header = if self.settings.codec == 2 {
            av1_header(&packet.data)?
        } else {
            codec_config(&output).or_else(|_| extract_annexb_header(&packet.data))?
        };
        self.settings.caps_profile = true;
        self.unregister_input(registered)?;
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) }
            .map_err(|error| mf_error("MFT priming flush", error))?;
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        }
        .map_err(|error| mf_error("MFT restart begin streaming", error))?;
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        }
        .map_err(|error| mf_error("MFT restart start of stream", error))?;
        self.registrations.clear();
        self.submitted.clear();
        self.ready.clear();
        self.need_input = 0;
        self.draining = false;
        self.drain_complete = false;
        self.settings.codec_config = header;
        Ok(())
    }

    pub(super) fn open_profile(
        raw_device: *mut c_void,
        adapter: &NativeAdapterIdentity,
        config: &NativeEncoderConfig,
        codec: u32,
    ) -> Result<Self, RecorderError> {
        if !matches!(codec, 0 | 2) {
            return Err(super::preparation_error(
                "MF profile is not implemented.".into(),
            ));
        }
        config.validate()?;
        let _enumeration_apartment = crate::capture::native::ComApartment::initialize_mta()?;
        let _enumeration_foundation = Foundation::start()?;
        let mfplat = Arc::new(optional_mfplat()?);
        let actual = super::native::device_adapter(raw_device)?;
        if actual.luid != adapter.luid {
            return Err(super::preparation_error(format!(
                "MF requested adapter {:016X} but capture device is on {:016X}.",
                adapter.luid, actual.luid
            )));
        }
        // SAFETY: the capture payload retains the incoming COM reference. Clone adds
        // this session's independent reference before the borrowed pointer expires.
        let borrowed = unsafe { ID3D11Device::from_raw_borrowed(&raw_device) }
            .ok_or_else(|| super::preparation_error("Null capture D3D11 device.".into()))?;
        let device = borrowed.clone();
        let context: ID3D11DeviceContext = unsafe { device.GetImmediateContext() }
            .map_err(|error| mf_error("ID3D11Device::GetImmediateContext", error))?;
        let multithread: ID3D11Multithread = context
            .cast()
            .map_err(|error| mf_error("ID3D11Multithread", error))?;
        let _previous_protection = unsafe { multithread.SetMultithreadProtected(true) };
        if !unsafe { multithread.GetMultithreadProtected() }.as_bool() {
            return Err(super::preparation_error("The capture D3D11 context refused multithread protection required by an asynchronous MFT.".into()));
        }
        let converter = super::nv12_convert::Nv12Converter::new(&device, config)?;
        let (manager, token) = create_dxgi_manager(&mfplat)?;
        unsafe { manager.ResetDevice(&device, token) }
            .map_err(|error| mf_error("IMFDXGIDeviceManager::ResetDevice", error))?;

        let activations = enumerate_profile_activations(adapter, codec)?;
        let mut failure = None;
        for activation in activations.iter() {
            let candidate = unsafe { activation.ActivateObject::<IMFTransform>() }
                .map_err(|error| mf_error("IMFActivate::ActivateObject", error));
            let result = candidate
                .and_then(|transform| Self::open_transform(transform, &manager, config, codec));
            match result {
                Ok((transform, events, settings, stream_size, supplies_sample)) => {
                    let apartment = crate::capture::native::ComApartment::initialize_mta()?;
                    let foundation = Foundation::start()?;
                    let mut driver = Self {
                        transform,
                        activation: activation.clone(),
                        events,
                        _manager: manager.clone(),
                        mfplat: Arc::clone(&mfplat),
                        device: device.clone(),
                        converter: converter.clone(),
                        registrations: Vec::new(),
                        submitted: VecDeque::new(),
                        ready: VecDeque::new(),
                        need_input: 0,
                        drain_complete: false,
                        draining: false,
                        closed: false,
                        cleanup_unsafe: false,
                        output_stream_size: stream_size,
                        output_supplies_sample: supplies_sample,
                        settings,
                        _foundation: foundation,
                        _apartment: apartment,
                    };
                    match driver.prime_codec_config(config) {
                        Ok(()) => return Ok(driver),
                        Err(error) => failure = Some(error),
                    }
                }
                Err(error) => {
                    let _ = unsafe { activation.ShutdownObject() };
                    failure = Some(error);
                }
            }
        }
        Err(failure.unwrap_or_else(|| RecorderError {
            code: ErrorCode::EncoderUnavailable,
            message: format!(
                "No adapter-scoped hardware H.264 MFT exists on {} (LUID {:016X}).",
                adapter.description, adapter.luid
            ),
            recovery: RecoveryStatus::NoRecording,
            action: "Install a hardware H.264 driver for this capture adapter.".into(),
        }))
    }

    fn open_transform(
        transform: IMFTransform,
        manager: &IMFDXGIDeviceManager,
        config: &NativeEncoderConfig,
        codec: u32,
    ) -> Result<
        (
            IMFTransform,
            IMFMediaEventGenerator,
            NativeEncoderSettings,
            u32,
            bool,
        ),
        RecorderError,
    > {
        let attributes = unsafe { transform.GetAttributes() }
            .map_err(|error| mf_error("IMFTransform::GetAttributes", error))?;
        unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            .map_err(|error| mf_error("MF_TRANSFORM_ASYNC_UNLOCK", error))?;
        let aware = unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }
            .map_err(|error| mf_error("MF_SA_D3D11_AWARE", error))?;
        if aware == 0 {
            return Err(super::preparation_error(
                "Hardware MFT does not accept D3D11 surfaces.".into(),
            ));
        }
        unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
            .map_err(|error| mf_error("MFT_MESSAGE_SET_D3D_MANAGER", error))?;
        configure_profile(&transform, config, codec)?;
        configure_codec(&transform, config, codec)?;
        let output = unsafe { transform.GetOutputCurrentType(0) }
            .map_err(|error| mf_error("GetOutputCurrentType", error))?;
        let width_height = unsafe { output.GetUINT64(&MF_MT_FRAME_SIZE) }
            .map_err(|error| mf_error("MF output frame size", error))?;
        let rate = unsafe { output.GetUINT64(&MF_MT_FRAME_RATE) }
            .map_err(|error| mf_error("MF output frame rate", error))?;
        let bitrate = unsafe { output.GetUINT32(&MF_MT_AVG_BITRATE) }
            .map_err(|error| mf_error("MF output bitrate", error))?;
        if width_height != packed(config.width, config.height)
            || rate != packed(config.rate_num.get(), config.rate_den.get())
            || bitrate != config.bitrate_bps
        {
            return Err(super::preparation_error(
                "MF output type readback differs from the requested geometry, rate or bitrate."
                    .into(),
            ));
        }
        let stream = unsafe { transform.GetOutputStreamInfo(0) }
            .map_err(|error| mf_error("GetOutputStreamInfo", error))?;
        let provides = stream.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
        let can_supply = stream.dwFlags & (MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
        let supplies = provides || can_supply;
        if !supplies && stream.cbSize == 0 {
            return Err(super::preparation_error(
                "MF output requires a caller buffer but reported zero size.".into(),
            ));
        }
        if stream.cbSize > MAX_ACCESS_UNIT_BYTES {
            return Err(super::preparation_error(format!(
                "MF output needs {} bytes, beyond the bounded packet slot.",
                stream.cbSize
            )));
        }
        let events: IMFMediaEventGenerator = transform
            .cast()
            .map_err(|error| mf_error("IMFMediaEventGenerator", error))?;
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) }
            .map_err(|error| mf_error("MFT begin streaming", error))?;
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0) }
            .map_err(|error| mf_error("MFT start of stream", error))?;
        let settings = NativeEncoderSettings {
            advanced: Default::default(),
            codec,
            input_format: 0,
            caps_input_formats: 1,
            caps_profile: false,
            caps_10bit: false,
            width: config.width,
            height: config.height,
            rate_num: config.rate_num.get(),
            rate_den: config.rate_den.get(),
            bitrate_bps: bitrate,
            max_bitrate_bps: bitrate,
            gop_length: config.gop_frames,
            idr_period: config.gop_frames,
            frame_interval_p: 1,
            enable_ptd: true,
            enable_async: true,
            repeat_spspps: false,
            level: 0,
            profile_high: false,
            vui_full_range: false,
            rate_control: NativeRateControl::Cbr,
            preset: NativeEncoderPreset::LowLatency,
            caps_async_encode: true,
            caps_width_max: config.width,
            caps_height_max: config.height,
            caps_rate_control_modes: 0x2,
            caps_h264_guid: codec == 0,
            caps_input_argb: false,
            caps_profile_high: false,
            caps_preset_available: true,
            output_buffers: 3,
            output_buffer_capacity: MAX_ACCESS_UNIT_BYTES,
            max_access_unit_bytes: MAX_ACCESS_UNIT_BYTES,
            codec_config: Vec::new(),
        };
        Ok((transform, events, settings, stream.cbSize, supplies))
    }

    fn pump(&mut self) -> Result<(), RecorderError> {
        for _ in 0..16 {
            // SAFETY: NO_WAIT keeps the shared worker's calls bounded.
            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(error) => return Err(mf_error("IMFMediaEventGenerator::GetEvent", error)),
            };
            let status = unsafe { event.GetStatus() }
                .map_err(|error| mf_error("IMFMediaEvent::GetStatus", error))?;
            status
                .ok()
                .map_err(|error| mf_error("MF transform event", error))?;
            let kind = unsafe { event.GetType() }
                .map_err(|error| mf_error("IMFMediaEvent::GetType", error))?;
            if kind == METransformNeedInput.0 as u32 {
                if !self.draining {
                    self.need_input = self
                        .need_input
                        .checked_add(1)
                        .filter(|count| *count <= 16)
                        .ok_or_else(|| {
                            super::internal_error(
                                "MF sent more than 16 unmatched NeedInput events.".into(),
                            )
                        })?;
                }
            } else if kind == METransformHaveOutput.0 as u32 {
                self.read_output()?;
            } else if kind == METransformDrainComplete.0 as u32 {
                self.drain_complete = true;
            }
        }
        Ok(())
    }

    fn read_output(&mut self) -> Result<(), RecorderError> {
        let supplied = if self.output_supplies_sample {
            None
        } else {
            let sample = unsafe { MFCreateSample() }
                .map_err(|error| mf_error("MFCreateSample(output)", error))?;
            let buffer = unsafe { MFCreateMemoryBuffer(self.output_stream_size.max(1)) }
                .map_err(|error| mf_error("MFCreateMemoryBuffer(output)", error))?;
            unsafe { sample.AddBuffer(&buffer) }
                .map_err(|error| mf_error("IMFSample::AddBuffer(output)", error))?;
            Some(sample)
        };
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(supplied),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };
        // SAFETY: ProcessOutput's COM references are released exactly once here,
        // regardless of its HRESULT. The struct uses ManuallyDrop fields.
        let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
        let events = unsafe { ManuallyDrop::take(&mut output.pEvents) };
        drop(events);
        if let Err(error) = &result
            && error.code() == MF_E_TRANSFORM_STREAM_CHANGE
        {
            if output.dwStatus & (MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE.0 as u32) == 0 {
                return Err(super::internal_error(
                    "MF stream change had no FORMAT_CHANGE status.".into(),
                ));
            }
            let changed = unsafe { self.transform.GetOutputAvailableType(0, 0) }
                .map_err(|error| mf_error("GetOutputAvailableType(stream change)", error))?;
            let subtype = unsafe { changed.GetGUID(&MF_MT_SUBTYPE) }
                .map_err(|error| mf_error("MF stream-change subtype", error))?;
            let geometry = unsafe { changed.GetUINT64(&MF_MT_FRAME_SIZE) }
                .map_err(|error| mf_error("MF stream-change geometry", error))?;
            let rate = unsafe { changed.GetUINT64(&MF_MT_FRAME_RATE) }
                .map_err(|error| mf_error("MF stream-change frame rate", error))?;
            let bitrate = unsafe { changed.GetUINT32(&MF_MT_AVG_BITRATE) }
                .map_err(|error| mf_error("MF stream-change bitrate", error))?;
            if subtype != output_subtype(self.settings.codec)
                || geometry != packed(self.settings.width, self.settings.height)
                || rate != packed(self.settings.rate_num, self.settings.rate_den)
                || bitrate != self.settings.bitrate_bps
            {
                return Err(super::internal_error(
                    "MF changed codec, geometry, frame rate or bitrate during recording.".into(),
                ));
            }
            unsafe { self.transform.SetOutputType(0, &changed, 0) }
                .map_err(|error| mf_error("SetOutputType(stream change)", error))?;
            if !self.settings.codec_config.is_empty() {
                let updated = unsafe { self.transform.GetOutputCurrentType(0) }
                    .map_err(|error| mf_error("GetOutputCurrentType(stream change)", error))?;
                let header = if self.settings.codec == 2 {
                    // Some AV1 MFTs expose av1C, some sequence OBUs, and some
                    // nothing until their first output. Actual packet sequence
                    // comparison remains mandatory in ProcessOutput below.
                    self.settings.codec_config.clone()
                } else {
                    codec_config(&updated)?
                };
                if header != self.settings.codec_config {
                    return Err(super::internal_error(
                        "MF changed H.264 codec configuration during recording.".into(),
                    ));
                }
            }
            return Ok(());
        }
        result.map_err(|error| mf_error("IMFTransform::ProcessOutput", error))?;
        if status != 0 || output.dwStatus != 0 {
            return Err(super::internal_error(format!(
                "MF ProcessOutput changed stream status ({status}, {}).",
                output.dwStatus
            )));
        }
        let sample = sample
            .ok_or_else(|| super::internal_error("MF HaveOutput returned no sample.".into()))?;
        let time_100ns = unsafe { sample.GetSampleTime() }
            .map_err(|error| mf_error("IMFSample::GetSampleTime", error))?;
        if time_100ns < 0 {
            return Err(super::internal_error(
                "MF returned a negative sample timestamp.".into(),
            ));
        }
        let position = self
            .submitted
            .iter()
            .position(|entry| entry.pts_ns / 100 == time_100ns as u64)
            .ok_or_else(|| {
                super::internal_error(format!(
                    "MF output timestamp {time_100ns} has no accepted input."
                ))
            })?;
        let entry = self
            .submitted
            .remove(position)
            .ok_or_else(|| super::internal_error("MF pending slot vanished.".into()))?;
        let output_duration = unsafe { sample.GetSampleDuration() };
        if let Ok(duration_100ns) = output_duration
            && duration_100ns != (entry.duration_ns / 100) as i64
            && duration_100ns != entry.duration_ns.div_ceil(100) as i64
        {
            return Err(super::internal_error(format!(
                "MF output duration {duration_100ns} differs from accepted {} ns.",
                entry.duration_ns
            )));
        }
        let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|error| mf_error("IMFSample::ConvertToContiguousBuffer", error))?;
        let length = unsafe { buffer.GetCurrentLength() }
            .map_err(|error| mf_error("IMFMediaBuffer::GetCurrentLength", error))?;
        if length == 0 || length > MAX_ACCESS_UNIT_BYTES {
            return Err(super::internal_error(format!(
                "MF output access unit is {length} bytes."
            )));
        }
        let mut pointer = std::ptr::null_mut();
        unsafe { buffer.Lock(&mut pointer, None, None) }
            .map_err(|error| mf_error("IMFMediaBuffer::Lock", error))?;
        let data = if pointer.is_null() {
            Err(super::internal_error(
                "MF locked output buffer has a null address.".into(),
            ))
        } else {
            // SAFETY: Lock returned at least CurrentLength valid bytes until Unlock.
            Ok(unsafe { std::slice::from_raw_parts(pointer, length as usize) }.to_vec())
        };
        let unlock =
            unsafe { buffer.Unlock() }.map_err(|error| mf_error("IMFMediaBuffer::Unlock", error));
        unlock?;
        let data = data?;
        if self.settings.codec == 0 && !annexb_has_nal(&data, 1) && !annexb_has_nal(&data, 5) {
            return Err(super::internal_error(
                "MF H.264 output is not an Annex-B coded picture.".into(),
            ));
        }
        let keyframe = if self.settings.codec == 2 {
            av1_picture(
                &data,
                (!self.settings.codec_config.is_empty())
                    .then_some(self.settings.codec_config.as_slice()),
            )
            .map_err(super::internal_error)?
        } else {
            annexb_has_nal(&data, 5)
        };
        if entry.force_idr && !keyframe {
            return Err(super::internal_error(
                "MF ignored the requested first IDR frame.".into(),
            ));
        }
        let clean = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
        if clean && !keyframe {
            return Err(super::internal_error(
                "MF marked a non-IDR access unit as a clean point.".into(),
            ));
        }
        if self.ready.len() >= 3 {
            return Err(super::internal_error(
                "MF output exceeded the bounded ready queue.".into(),
            ));
        }
        self.ready.push_back((
            entry.slot,
            NativePacket {
                data,
                pts_ns: entry.pts_ns,
                duration_ns: entry.duration_ns,
                keyframe,
                picture_type: if keyframe { 5 } else { 1 },
                frame_index: 0,
            },
        ));
        Ok(())
    }

    fn convert_and_submit(
        &mut self,
        registration: u32,
        pts_ns: u64,
        duration_ns: u64,
        force_idr: bool,
    ) -> Result<bool, RecorderError> {
        let slot = self
            .registrations
            .get(registration as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| super::internal_error("MF registration no longer exists.".into()))?;
        if let MfRegistration::Bgra(input) = slot {
            self.converter.convert(input)?;
        }
        let context = unsafe { self.device.GetImmediateContext() }
            .map_err(|error| mf_error("GetImmediateContext(after video processor)", error))?;
        unsafe { context.Flush() };
        let buffer = create_dxgi_surface_buffer(&self.mfplat, slot.nv12())?;
        let sample = unsafe { MFCreateSample() }
            .map_err(|error| mf_error("MFCreateSample(input)", error))?;
        unsafe { sample.AddBuffer(&buffer) }
            .map_err(|error| mf_error("IMFSample::AddBuffer(input)", error))?;
        let time = i64::try_from(pts_ns / 100).map_err(|_| {
            super::invalid_request_error("MF timestamp exceeds signed 100 ns range.".into())
        })?;
        let duration = i64::try_from(duration_ns.div_ceil(100)).map_err(|_| {
            super::invalid_request_error("MF duration exceeds signed 100 ns range.".into())
        })?;
        unsafe { sample.SetSampleTime(time) }
            .map_err(|error| mf_error("IMFSample::SetSampleTime", error))?;
        unsafe { sample.SetSampleDuration(duration) }
            .map_err(|error| mf_error("IMFSample::SetSampleDuration", error))?;
        if force_idr {
            force_next_keyframe(&self.transform)?;
        }
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => Ok(true),
            Err(error) if error.code() == MF_E_NOTACCEPTING => Ok(false),
            Err(error) => Err(mf_error("IMFTransform::ProcessInput", error)),
        }
    }
}

impl NvencDriver for MfDriver {
    fn settings(&self) -> &NativeEncoderSettings {
        &self.settings
    }

    fn register_input(&mut self, texture: EncodeTexture) -> Result<u32, RecorderError> {
        if texture.device != self.device.as_raw() || self.registrations.len() >= 4 {
            return Err(super::invalid_request_error(
                "MF registration must use one of four textures on the capture D3D11 device.".into(),
            ));
        }
        // SAFETY: the worker retains the surface backing until unregister_input.
        let borrowed = unsafe { ID3D11Texture2D::from_raw_borrowed(&texture.texture) }
            .ok_or_else(|| super::invalid_request_error("Null MF source texture.".into()))?;
        let source = borrowed.clone();
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut source_desc) };
        if source_desc.Width != self.settings.width
            || source_desc.Height != self.settings.height
            || !matches!(
                source_desc.Format,
                DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_NV12
            )
            || source_desc.SampleDesc.Count != 1
            || source_desc.MipLevels != 1
            || source_desc.ArraySize != 1
            || (source_desc.Format == DXGI_FORMAT_NV12
                && source_desc.BindFlags
                    & windows::Win32::Graphics::Direct3D11::D3D11_BIND_VIDEO_ENCODER.0 as u32
                    == 0)
        {
            return Err(super::invalid_request_error(
                "MF source texture is not a matching single-sample BGRA8/NV12 surface.".into(),
            ));
        }
        let input = if source_desc.Format == DXGI_FORMAT_NV12 {
            MfRegistration::Nv12(source)
        } else {
            MfRegistration::Bgra(self.converter.prepare(&source)?)
        };
        let registration = self.registrations.len() as u32;
        self.registrations.push(Some(input));
        Ok(registration)
    }

    fn unregister_input(&mut self, registration: u32) -> Result<(), RecorderError> {
        if self
            .submitted
            .iter()
            .any(|entry| entry.slot == registration)
            || self.ready.iter().any(|(slot, _)| *slot == registration)
        {
            return Err(super::internal_error(
                "MF input is still owned by an in-flight output.".into(),
            ));
        }
        let item = self
            .registrations
            .get_mut(registration as usize)
            .ok_or_else(|| super::internal_error("Unknown MF registration.".into()))?;
        item.take();
        Ok(())
    }

    fn submit(
        &mut self,
        registration: u32,
        pts_ns: u64,
        duration_ns: u64,
        force_idr: bool,
    ) -> Result<NativeSubmit, RecorderError> {
        self.pump()?;
        if self.draining || self.need_input == 0 || self.submitted.len() + self.ready.len() >= 3 {
            return Ok(NativeSubmit::Busy);
        }
        if self
            .submitted
            .iter()
            .any(|entry| entry.pts_ns / 100 == pts_ns / 100)
        {
            return Err(super::invalid_request_error(
                "Two MF frames quantize to the same 100 ns timestamp.".into(),
            ));
        }
        if self.convert_and_submit(registration, pts_ns, duration_ns, force_idr)? {
            self.need_input -= 1;
            self.submitted.push_back(Submitted {
                slot: registration,
                pts_ns,
                duration_ns,
                force_idr,
            });
            Ok(NativeSubmit::Pending { slot: registration })
        } else {
            Ok(NativeSubmit::Busy)
        }
    }

    fn collect(
        &mut self,
        slot: u32,
        wait_ms: u32,
        _capacity: usize,
    ) -> Result<NativeCollect, RecorderError> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(wait_ms.min(5)));
        loop {
            self.pump()?;
            if let Some(index) = self
                .ready
                .iter()
                .position(|(ready_slot, _)| *ready_slot == slot)
            {
                let (_, packet) = self
                    .ready
                    .remove(index)
                    .ok_or_else(|| super::internal_error("MF ready packet vanished.".into()))?;
                return Ok(NativeCollect::Ready(packet));
            }
            if Instant::now() >= deadline {
                return Ok(NativeCollect::Pending);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn begin_eos(&mut self) -> Result<NativeEos, RecorderError> {
        if self.draining {
            return Ok(NativeEos::Submitted);
        }
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
        }
        .map_err(|error| mf_error("MFT end of stream", error))?;
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .map_err(|error| mf_error("MFT command drain", error))?;
        self.draining = true;
        self.need_input = 0;
        Ok(NativeEos::Submitted)
    }

    fn poll_eos(&mut self, wait_ms: u32) -> Result<bool, RecorderError> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(wait_ms.min(5)));
        loop {
            self.pump()?;
            if self.drain_complete {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn stats(&self) -> NativeStats {
        NativeStats::default()
    }

    fn phase(&self) -> String {
        "MF async event processing".into()
    }

    fn close(&mut self) -> Result<(), RecorderError> {
        if self.closed {
            return Ok(());
        }
        let flush = unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) }
            .map_err(|error| mf_error("MFT command flush", error));
        let transform_shutdown = self
            .transform
            .cast::<IMFShutdown>()
            .map_err(|error| mf_error("IMFShutdown", error))
            .and_then(|shutdown| {
                unsafe { shutdown.Shutdown() }
                    .map_err(|error| mf_error("IMFShutdown::Shutdown", error))
            });
        let activation_shutdown = unsafe { self.activation.ShutdownObject() }
            .map_err(|error| mf_error("IMFActivate::ShutdownObject", error));
        if transform_shutdown.is_err() && activation_shutdown.is_err() {
            // The worker must retain its textures and the transform while either
            // shutdown route could still leave native input in flight.
            self.cleanup_unsafe = true;
            let mut error = transform_shutdown.expect_err("both shutdown routes failed");
            let activation = activation_shutdown.expect_err("both shutdown routes failed");
            error.message = format!("{} {}", error.message, activation.message);
            return Err(error);
        }
        self.submitted.clear();
        self.ready.clear();
        self.registrations.clear();
        self.closed = true;
        self.cleanup_unsafe = false;
        flush
    }

    fn cleanup_unsafe(&self) -> bool {
        self.cleanup_unsafe
    }
}

impl Drop for MfDriver {
    fn drop(&mut self) {
        while !self.closed {
            let _ = self.close();
            if self.cleanup_unsafe {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// Qualification means a real GPU texture was converted, accepted, emitted as
/// Annex-B H.264, and drained. Multiple MFT activations still form one adapter
/// candidate; only the first complete successful proof is returned.
/// A solid-color GPU frame copied into the exact bind-free BGRA texture shape
/// allocated by display capture. The temporary render target is never encoded.
fn synthetic_capture_texture(
    device: &ID3D11Device,
    config: &NativeEncoderConfig,
) -> Result<ID3D11Texture2D, RecorderError> {
    synthetic_capture_texture_format(device, config, false)
}

fn synthetic_capture_texture_format(
    device: &ID3D11Device,
    config: &NativeEncoderConfig,
    hdr: bool,
) -> Result<ID3D11Texture2D, RecorderError> {
    let capture_desc = D3D11_TEXTURE2D_DESC {
        Width: config.width,
        Height: config.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: if hdr {
            DXGI_FORMAT_R16G16B16A16_FLOAT
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        },
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: if hdr {
            windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE.0 as u32
        } else {
            0
        },
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut capture = None;
    unsafe { device.CreateTexture2D(&capture_desc, None, Some(&mut capture)) }
        .map_err(|error| mf_error("CreateTexture2D(synthetic capture)", error))?;
    let capture =
        capture.ok_or_else(|| super::internal_error("No synthetic capture texture.".into()))?;
    let render_desc = D3D11_TEXTURE2D_DESC {
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..capture_desc
    };
    let mut render = None;
    unsafe { device.CreateTexture2D(&render_desc, None, Some(&mut render)) }
        .map_err(|error| mf_error("CreateTexture2D(synthetic render)", error))?;
    let render =
        render.ok_or_else(|| super::internal_error("No synthetic render texture.".into()))?;
    let render_resource: ID3D11Resource = render
        .cast()
        .map_err(|error| mf_error("ID3D11Resource(synthetic render)", error))?;
    let capture_resource: ID3D11Resource = capture
        .cast()
        .map_err(|error| mf_error("ID3D11Resource(synthetic capture)", error))?;
    let mut view = None;
    unsafe { device.CreateRenderTargetView(&render_resource, None, Some(&mut view)) }
        .map_err(|error| mf_error("CreateRenderTargetView(synthetic render)", error))?;
    let view = view.ok_or_else(|| super::internal_error("No synthetic render target.".into()))?;
    let context = unsafe { device.GetImmediateContext() }
        .map_err(|error| mf_error("GetImmediateContext(synthetic capture)", error))?;
    unsafe {
        context.ClearRenderTargetView(&view, &[0.16, 0.28, 0.42, 1.0]);
        context.CopyResource(&capture_resource, &render_resource);
        context.Flush();
    }
    Ok(capture)
}

pub(super) fn qualify_profile(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
    codec: u32,
) -> Result<NativeEncoderSettings, RecorderError> {
    prove_profile_packet(adapter, config, codec).map(|(settings, _)| settings)
}
fn prove_profile_packet(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
    codec: u32,
) -> Result<(NativeEncoderSettings, NativePacket), RecorderError> {
    let device = super::native::qualification_device_for_luid(adapter.luid)?;
    let texture = synthetic_capture_texture(&device, config)?;
    let mut driver = MfDriver::open_profile(device.as_raw(), adapter, config, codec)?;
    let settings = driver.settings().clone();
    let registered = driver.register_input(EncodeTexture {
        device: device.as_raw(),
        texture: texture.as_raw(),
        identity: SurfaceIdentity {
            pool: 0,
            device: device.as_raw() as usize,
            texture: texture.as_raw() as usize,
            width: config.width,
            height: config.height,
        },
    })?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let slot = loop {
        match driver.submit(registered, 0, 16_666_667, true)? {
            NativeSubmit::Pending { slot } | NativeSubmit::Buffered { slot } => break slot,
            NativeSubmit::Busy if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2))
            }
            NativeSubmit::Busy => {
                return Err(super::preparation_error(
                    "MF proof never requested input.".into(),
                ));
            }
        }
    };
    driver.begin_eos()?;
    let mut packet = None;
    let mut drained = false;
    while Instant::now() < deadline {
        if packet.is_none()
            && let NativeCollect::Ready(value) =
                driver.collect(slot, 5, MAX_ACCESS_UNIT_BYTES as usize)?
        {
            packet = Some(value);
        }
        if !drained {
            drained = driver.poll_eos(5)?;
        }
        if packet.is_some() && drained {
            break;
        }
    }
    let packet = packet.ok_or_else(|| {
        super::preparation_error("MF proof produced no H.264 access unit.".into())
    })?;
    if !drained || !packet.keyframe || packet.pts_ns != 0 {
        return Err(super::preparation_error(
            "MF proof did not produce a zero-PTS IDR and complete drain.".into(),
        ));
    }
    driver.unregister_input(registered)?;
    driver.close()?;
    Ok((settings, packet))
}

/// Runs the production PacketEncoder facade with a one-slot synthetic capture
/// surface. The same adapter/device/texture path is used; no display is opened.
pub(super) fn synthetic_packet_encoder_frame(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
) -> Result<(frame_encode::EncodedStreamInfo, frame_encode::EncodedPacket), RecorderError> {
    use frame_capture::CapturedFrame;
    use frame_clock::MediaTimestamp;
    use frame_encode::{EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome};
    use std::num::NonZeroU64;

    let device = super::native::qualification_device_for_luid(adapter.luid)?;
    let texture = synthetic_capture_texture(&device, config)?;
    let pool = crate::surface::SurfacePool::new(0);
    let payload: crate::surface::SharedPayload = crate::surface::GpuSurfaceSet::new(
        device,
        vec![texture],
        config.width,
        config.height,
        pool.id(),
        crate::surface::PayloadLifetime::new(),
    );
    let slot = pool
        .try_acquire_copy_slot()
        .ok_or_else(|| super::internal_error("Synthetic capture pool has no free slot.".into()))?;
    slot.mark_ready();
    slot.mark_consumer_owned();
    let surface = crate::surface::WindowsSurface::new(slot, payload);
    let mut encoder = super::WindowsH264Encoder::open(*config, &surface)?;
    let info = encoder.info().clone();
    // Deliberately not divisible by 100 ns: the MFT receives a rounded time,
    // while PacketEncoder must return the capture timeline's exact PTS.
    let pts = MediaTimestamp(1_000_037);
    let input = EncoderInput {
        input: CapturedFrame {
            surface,
            timestamp: pts,
            width: config.width,
            height: config.height,
        },
        duration_ns: NonZeroU64::new(16_666_667)
            .ok_or_else(|| super::internal_error("Synthetic duration is zero.".into()))?,
    };
    match encoder.submit(input)? {
        SubmitOutcome::Accepted => {}
        SubmitOutcome::Backpressure(_) => {
            return Err(super::internal_error(
                "One synthetic frame met encoder backpressure.".into(),
            ));
        }
    }
    encoder.begin_drain()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut packet = None;
    loop {
        match encoder.try_packet()? {
            EncoderPoll::Packet(value) if packet.is_none() => packet = Some(value),
            EncoderPoll::Packet(_) => {
                return Err(super::internal_error(
                    "One synthetic frame produced multiple packets.".into(),
                ));
            }
            EncoderPoll::Drained => break,
            EncoderPoll::Pending if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2))
            }
            EncoderPoll::Pending => {
                return Err(super::internal_error(
                    "Synthetic PacketEncoder drain timed out.".into(),
                ));
            }
        }
    }
    encoder.stop()?;
    let packet = packet.ok_or_else(|| {
        super::internal_error("Synthetic PacketEncoder produced no packet.".into())
    })?;
    if packet.pts != pts || !packet.keyframe {
        return Err(super::internal_error(
            "Synthetic PacketEncoder changed the source PTS or omitted the first IDR.".into(),
        ));
    }
    Ok((info, packet))
}

/// A bounded, unpaced synthetic sequence through the production isolated encoder.
/// FREYM_MF_PROBE_PATTERN=1 copies four generated color/edge patterns into the
/// acquired slot before publication. Otherwise four solid-color surfaces are used.
pub(super) fn synthetic_packet_encoder_sequence(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
    selected_id: Option<&str>,
    frame_count: u32,
    pattern_period: u32,
    cancelled: impl Fn() -> bool,
    mut receive: impl FnMut(
        &frame_encode::EncodedStreamInfo,
        frame_encode::EncodedPacket,
    ) -> Result<(), RecorderError>,
) -> Result<super::SyntheticEncoderSequence, RecorderError> {
    use frame_capture::CapturedFrame;
    use frame_clock::MediaTimestamp;
    use frame_encode::{EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome};

    if !matches!(adapter.vendor_id, 0x10de | 0x8086 | 0x1002)
        || !(1..=240).contains(&frame_count)
        || !(1..=240).contains(&pattern_period)
    {
        return Err(RecorderError::invalid(
            "The calibration sequence needs supported hardware and 1–240 frames.",
        ));
    }
    config.validate()?;
    let hdr = selected_id.is_some_and(|id| id.contains("main10"));
    let plan = crate::surface::plan_owned_payload_with_bytes(
        config.width,
        config.height,
        false,
        if hdr { 8 } else { 4 },
    )?;
    let device = super::native::qualification_device_for_luid(adapter.luid)?;
    let textures = (0..plan.slot_count())
        .map(|_| synthetic_capture_texture_format(&device, config, hdr))
        .collect::<Result<Vec<_>, _>>()?;
    let patterned = selected_id.is_some()
        || std::env::var_os("FREYM_MF_PROBE_PATTERN").is_some_and(|value| value == "1");
    let context = unsafe { device.GetImmediateContext() }
        .map_err(|error| mf_error("GetImmediateContext(sequence)", error))?;
    let multithread: ID3D11Multithread = context
        .cast()
        .map_err(|error| mf_error("ID3D11Multithread(sequence)", error))?;
    let _previous_protection = unsafe { multithread.SetMultithreadProtected(true) };
    if !unsafe { multithread.GetMultithreadProtected() }.as_bool() {
        return Err(super::preparation_error(
            "Synthetic sequence needs a protected D3D11 context.".into(),
        ));
    }
    let pattern_sources = if patterned {
        (0..4)
            .map(|variant| {
                synthetic_pattern_texture(&device, config, variant, hdr).and_then(|texture| {
                    texture
                        .cast::<ID3D11Resource>()
                        .map_err(|error| mf_error("pattern resource", error))
                })
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let capture_resources = textures
        .iter()
        .map(|texture| {
            texture
                .cast::<ID3D11Resource>()
                .map_err(|error| mf_error("capture resource", error))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let pool = crate::surface::SurfacePool::new(plan.slots_bytes);
    let payload: crate::surface::SharedPayload = crate::surface::GpuSurfaceSet::new(
        device,
        textures,
        config.width,
        config.height,
        pool.id(),
        crate::surface::PayloadLifetime::new(),
    );
    struct SyntheticPayload {
        backing: crate::surface::SharedPayload,
        hdr: bool,
    }
    impl crate::surface::PayloadOwner for SyntheticPayload {
        fn encode_texture(&self, slot: usize) -> Option<crate::surface::EncodeTexture> {
            self.backing.encode_texture(slot)
        }
        fn format(&self) -> crate::surface::SurfaceFormat {
            if self.hdr {
                crate::surface::SurfaceFormat::ScRgbFloat16
            } else {
                crate::surface::SurfaceFormat::Bgra8
            }
        }
        fn signal(&self) -> Option<crate::surface::SurfaceSignal> {
            self.hdr.then_some(crate::surface::SurfaceSignal::SCRGB)
        }
    }
    let payload: crate::surface::SharedPayload = Arc::new(SyntheticPayload {
        backing: payload,
        hdr,
    });
    let make_input = |index: u32| -> Result<Option<_>, RecorderError> {
        let Some(slot) = pool.try_acquire_copy_slot() else {
            return Ok(None);
        };
        if patterned {
            // The pool grants exclusive producer ownership. The queued copy precedes
            // the consumer's conversion/readback on the same protected context.
            unsafe {
                context.CopyResource(
                    &capture_resources[slot.index()],
                    &pattern_sources[(index / pattern_period) as usize % 4],
                )
            };
        }
        slot.mark_ready();
        slot.mark_consumer_owned();
        let duration = frame_encode::nominal_duration(config.rate_num, config.rate_den);
        let pts = u64::from(index)
            .checked_mul(duration.get())
            .and_then(|n| n.checked_add(1_000_037))
            .ok_or_else(|| RecorderError::invalid("Synthetic timestamps overflowed."))?;
        Ok(Some(EncoderInput {
            input: CapturedFrame {
                surface: crate::surface::WindowsSurface::new(slot, Arc::clone(&payload)),
                timestamp: MediaTimestamp(pts),
                width: config.width,
                height: config.height,
            },
            duration_ns: duration,
        }))
    };
    let first =
        make_input(0)?.ok_or_else(|| super::internal_error("No synthetic surface slot.".into()))?;
    let mut encoder = if let Some(id) = selected_id {
        super::WindowsH264Encoder::open_selected(*config, &first.input.surface, id)?
    } else {
        super::WindowsH264Encoder::open(*config, &first.input.surface)?
    };
    encoder.set_wake_signal(frame_capture::PipelineWake::for_current_thread());
    let info = encoder.info().clone();
    let mut pending = Some(first);
    let mut admitted = 0u32;
    let mut emitted = 0u32;
    let mut draining = false;
    let mut deadline = Instant::now() + Duration::from_secs(10);
    let started = Instant::now();
    let mut submitted_at = std::collections::VecDeque::new();
    let mut latencies = Vec::with_capacity(frame_count as usize);
    let mut overheads = Vec::with_capacity(frame_count as usize);
    loop {
        if cancelled() {
            encoder.stop()?;
            return Err(RecorderError::invalid(
                "Idle encoder calibration interrupted by recording.",
            ));
        }
        let turn_started = Instant::now();
        let mut progress = false;
        match encoder.try_packet()? {
            EncoderPoll::Packet(packet) => {
                if let Some(submitted) = submitted_at.pop_front() {
                    let submitted: Instant = submitted;
                    latencies
                        .push(u64::try_from(submitted.elapsed().as_nanos()).unwrap_or(u64::MAX));
                }
                let duration = frame_encode::nominal_duration(config.rate_num, config.rate_den);
                let expected = 1_000_037 + u64::from(emitted) * duration.get();
                if emitted >= frame_count
                    || packet.pts.0 != expected
                    || packet.duration_ns != duration.get()
                    || (emitted == 0 && !packet.keyframe)
                {
                    return Err(super::internal_error(
                        "Synthetic MF sequence changed frame count, PTS, duration or first IDR."
                            .into(),
                    ));
                }
                receive(&info, packet)?;
                emitted += 1;
                progress = true;
            }
            EncoderPoll::Drained if draining && emitted == frame_count => break,
            EncoderPoll::Drained => {
                return Err(super::internal_error(
                    "Synthetic MF sequence drained early.".into(),
                ));
            }
            EncoderPoll::Pending => {}
        }
        if pending.is_none() && admitted < frame_count {
            pending = make_input(admitted)?;
        }
        if let Some(input) = pending.take() {
            match encoder.submit(input)? {
                SubmitOutcome::Accepted => {
                    submitted_at.push_back(Instant::now());
                    admitted += 1;
                    progress = true;
                }
                SubmitOutcome::Backpressure(input) => pending = Some(input),
            }
        }
        if admitted == frame_count && !draining {
            encoder.begin_drain()?;
            draining = true;
        }
        if progress {
            overheads.push(u64::try_from(turn_started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        }
        if progress {
            deadline = Instant::now() + Duration::from_secs(10);
        } else if Instant::now() >= deadline {
            return Err(super::internal_error(format!(
                "Synthetic MF sequence made no progress for ten seconds: admitted={admitted}, emitted={emitted}, draining={draining}, pending={}, counters={:?}.",
                pending.is_some(),
                encoder.counters(),
            )));
        } else {
            std::thread::park_timeout(Duration::from_millis(1));
        }
    }
    encoder.stop()?;
    let p95 = |mut values: Vec<u64>| {
        values.sort_unstable();
        values
            .get((values.len() * 95).div_ceil(100).saturating_sub(1))
            .copied()
    };
    Ok(super::SyntheticEncoderSequence {
        settings: encoder.settings().clone(),
        qp: encoder.qp_statistics(),
        stats: encoder.stats(),
        transfer: encoder.mf_transfer_stats(),
        latency_p95_ns: p95(latencies),
        overhead_p95_ns: p95(overheads),
        elapsed_ns: u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
    })
}

/// Immutable fixture: color bars, 2-pixel checkerboard and glyph-like thin lines.
/// Four phases make successive frames differ without any personal media source.
fn synthetic_pattern_texture(
    device: &ID3D11Device,
    config: &NativeEncoderConfig,
    variant: u32,
    hdr: bool,
) -> Result<ID3D11Texture2D, RecorderError> {
    let length = (config.width as usize)
        .checked_mul(config.height as usize)
        .and_then(|n| n.checked_mul(4))
        .filter(|n| *n <= 128 * 1024 * 1024)
        .ok_or_else(|| {
            super::invalid_request_error("Synthetic pattern exceeds the frame bound.".into())
        })?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(length).map_err(|error| {
        super::preparation_error(format!("Synthetic pattern allocation failed: {error}"))
    })?;
    pixels.resize(length, 0);
    let palette = [
        [0, 0, 0],
        [255, 255, 255],
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [0, 255, 255],
        [255, 0, 255],
        [255, 255, 0],
    ];
    for y in 0..config.height {
        for x in 0..config.width {
            let rgb = if y < config.height / 3 {
                palette[((u64::from(x) * 8 / u64::from(config.width) + u64::from(variant)) % 8)
                    as usize]
            } else if y < config.height * 2 / 3 {
                let level = if (x / 2 + y / 2 + variant).is_multiple_of(2) {
                    0
                } else {
                    255
                };
                [level; 3]
            } else {
                let gx = (x + variant * 3) % 32;
                let gy = (y + variant * 5) % 32;
                let ink = ((4..=5).contains(&gx) && (4..=21).contains(&gy))
                    || ((4..=17).contains(&gx) && (gy == 4 || gy == 12 || gy == 20));
                [if ink { 255 } else { 32 }; 3]
            };
            let offset = (y as usize * config.width as usize + x as usize) * 4;
            pixels[offset..offset + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
        }
    }
    let bytes_per_pixel = if hdr { 8 } else { 4 };
    if hdr {
        // Linear half-float fixture with the same changing edges/colour bars.
        let mut half = Vec::with_capacity(pixels.len() * 2);
        for bgra in pixels.chunks_exact(4) {
            for value in [bgra[2], bgra[1], bgra[0], 255] {
                let bits: u16 = match value {
                    0 => 0,
                    255 => 0x3c00,
                    _ => 0x3000,
                };
                half.extend_from_slice(&bits.to_le_bytes());
            }
        }
        pixels = half;
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: config.width,
        Height: config.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: if hdr {
            DXGI_FORMAT_R16G16B16A16_FLOAT
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        },
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let data = windows::Win32::Graphics::Direct3D11::D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: config.width * bytes_per_pixel,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)) }
        .map_err(|error| mf_error("CreateTexture2D(synthetic pattern)", error))?;
    texture.ok_or_else(|| super::internal_error("No synthetic pattern texture.".into()))
}

#[cfg(test)]
mod codec_diagnostic_tests {
    use super::*;

    #[test]
    fn codec_diagnostics_preserve_scalar_tags_and_exact_property_identity() {
        let mut value = VARIANT::default();
        assert_eq!(codec_variant_detail(&value), "VT_EMPTY");
        // SAFETY: all test values are scalar variants and own no allocation.
        unsafe {
            (*value.Anonymous.Anonymous).vt = VT_UI4;
            (*value.Anonymous.Anonymous).Anonymous.ulVal = u32::MAX;
        }
        assert_eq!(codec_variant_detail(&value), "VT_UI4:4294967295");
        unsafe {
            (*value.Anonymous.Anonymous).vt = VT_I4;
            (*value.Anonymous.Anonymous).Anonymous.lVal = -1;
        }
        assert_eq!(codec_variant_detail(&value), "VT_I4:-1");
        unsafe {
            (*value.Anonymous.Anonymous).vt = VT_BOOL;
            (*value.Anonymous.Anonymous).Anonymous.boolVal =
                windows::Win32::Foundation::VARIANT_TRUE;
        }
        assert_eq!(codec_variant_detail(&value), "VT_BOOL:-1");
        assert_eq!(
            codec_property_name(&CODECAPI_AVEncMPVDefaultBPictureCount),
            "CODECAPI_AVEncMPVDefaultBPictureCount",
        );
        assert_eq!(
            codec_property_name(&GUID::zeroed()),
            "unknown CODECAPI property",
        );
    }
}
