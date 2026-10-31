//! Strict GPU admission within the existing PipeWire/VA pipeline.
//! PipeWire converts exact DMA_DRM caps to SPA format/modifier offers and
//! negotiates DmaBuf storage. Pool/ParentBufferMeta retain exporter ownership;
//! VA attaches imported surfaces to GstMemory. Each input stays held until
//! the VPP destination fence completes, before it returns to PipeWire.
use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gstreamer as gst;

use crate::video::drm_identity;
use crate::{DmaBufBlocker, DmaDrmFormat, DrmDeviceIdentity, assess_dma_buf_import};

#[derive(Default)]
pub(crate) struct ImportState {
    pub(crate) failure: Option<DmaBufBlocker>,
    pub(crate) format: Option<DmaDrmFormat>,
    pub(crate) capture_device: Option<DrmDeviceIdentity>,
    pub(crate) rejected_buffers: u64,
    caps: Option<gst::Caps>,
    pending_input: Option<gst::Buffer>,
    pending_output: Option<gst::Buffer>,
    display: Option<gst::Object>,
    stopping: bool,
}

pub(crate) type SharedImportState = Arc<Mutex<ImportState>>;

pub(crate) fn install_guard(
    importer: &gst::Element,
    encoder: &gst::Element,
    filter: &gst::Element,
    encoder_device: Option<&DrmDeviceIdentity>,
) -> Result<SharedImportState, DmaBufBlocker> {
    if !cfg!(frame_linux_va_import) {
        return Err(DmaBufBlocker::ImportCapsUnsupported);
    }
    let encoder_device = encoder_device
        .ok_or(DmaBufBlocker::EncoderDeviceUnknown)?
        .clone();
    let import_device = device_path(importer)
        .as_deref()
        .and_then(drm_identity)
        .ok_or(DmaBufBlocker::ImportDeviceUnknown)?;
    assess_dma_buf_import(Some(&import_device), Some(&encoder_device), true)?;
    let (display, _) = va_display(encoder)?;
    // Surface caches are display-specific, even on one physical adapter.
    let (import_display, _) = va_display(importer)?;
    if display != import_display {
        return Err(DmaBufBlocker::DeviceMismatch);
    }
    let sink = importer
        .static_pad("sink")
        .ok_or(DmaBufBlocker::ImportCapsUnsupported)?;
    let supported = exact_import_caps(&sink.query_caps(None));
    if supported.is_empty() {
        return Err(DmaBufBlocker::ImportCapsUnsupported);
    }
    // Advertise the opened driver's exact alternatives to PipeWire before it
    // fixates SPA format/modifier, rather than only validating buffers later.
    filter.set_property("caps", &supported);
    let state = Arc::new(Mutex::new(ImportState {
        display: Some(display.clone()),
        ..Default::default()
    }));
    let input_state = Arc::clone(&state);
    let input_display = display.clone();
    sink.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(buffer) = info.buffer_mut() else {
            return gst::PadProbeReturn::Ok;
        };
        let Ok(mut state) = input_state.lock() else {
            return gst::PadProbeReturn::Drop;
        };
        if state.stopping {
            // Teardown refuses new GPU work before disconnecting PipeWire.
            return gst::PadProbeReturn::Drop;
        }
        let result = if let Some(failure) = state.failure {
            Err(failure)
        } else {
            validate_input(pad, buffer, &supported, &encoder_device, &mut state)
                .and_then(|caps| native::import(&input_display, &caps, buffer))
        };
        match result {
            Err(failure) => reject(&mut state, failure),
            Ok(imported) => {
                // VPP has passthrough disabled: one input -> one output. This ref
                // retains the parent metadata as well as its borrowed descriptors.
                state.pending_input = Some(imported.clone());
                *buffer = imported;
                gst::PadProbeReturn::Ok
            }
        }
    })
    .ok_or(DmaBufBlocker::ImportCapsUnsupported)?;
    let output_state = Arc::clone(&state);
    let output_display = display.clone();
    importer
        .static_pad("src")
        .ok_or(DmaBufBlocker::ImportCapsUnsupported)?
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let Some(buffer) = info.buffer() else {
                return gst::PadProbeReturn::Ok;
            };
            let Ok(mut state) = output_state.lock() else {
                return gst::PadProbeReturn::Drop;
            };
            if state.failure.is_some() {
                return reject(&mut state, DmaBufBlocker::BufferOwnership);
            }
            if !state
                .pending_input
                .as_ref()
                .is_some_and(|input| input.pts().is_some() && input.pts() == buffer.pts())
            {
                state.pending_output = Some(buffer.to_owned());
                return reject(&mut state, DmaBufBlocker::BufferOwnership);
            }
            state.pending_output = Some(buffer.to_owned());
            if let Err(failure) = native::sync(&output_display, buffer) {
                // Retain both refs until checked GPU teardown on error.
                return reject(&mut state, failure);
            }
            state.pending_input = None;
            state.pending_output = None;
            gst::PadProbeReturn::Ok
        })
        .ok_or(DmaBufBlocker::ImportCapsUnsupported)?;
    let native_state = Arc::clone(&state);
    encoder
        .static_pad("sink")
        .ok_or(DmaBufBlocker::ImportCapsUnsupported)?
        .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            let Some(buffer) = info.buffer() else {
                return gst::PadProbeReturn::Ok;
            };
            let caps_valid = pad.current_caps().is_some_and(|caps| {
                caps.features(0)
                    .is_some_and(|f| f.contains("memory:VAMemory"))
                    && caps
                        .structure(0)
                        .is_some_and(|s| s.get::<String>("format").is_ok_and(|f| f == "NV12"))
            });
            let memory_valid =
                buffer.n_memory() > 0 && buffer.iter_memories().all(|m| m.is_type("VAMemory"));
            let Ok(mut state) = native_state.lock() else {
                return gst::PadProbeReturn::Drop;
            };
            if state.failure.is_some() || !caps_valid || !memory_valid {
                reject(&mut state, DmaBufBlocker::MemoryTypeMismatch)
            } else if let Err(failure) = native::sync(&display, buffer) {
                reject(&mut state, failure)
            } else {
                gst::PadProbeReturn::Ok
            }
        })
        .ok_or(DmaBufBlocker::ImportCapsUnsupported)?;
    Ok(state)
}

fn reject(state: &mut ImportState, failure: DmaBufBlocker) -> gst::PadProbeReturn {
    state.failure.get_or_insert(failure);
    state.rejected_buffers = state.rejected_buffers.saturating_add(1);
    gst::PadProbeReturn::Drop
}

/// Refuse new input and settle the last transform before disconnecting the
/// exporter. Null first could return its buffers while VPP still reads them.
pub(crate) fn quiesce(state: &SharedImportState) -> Result<(), DmaBufBlocker> {
    state
        .lock()
        .map_err(|_| DmaBufBlocker::BufferOwnership)?
        .stopping = true;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        match finish_release(state) {
            Err(DmaBufBlocker::BufferOwnership) if std::time::Instant::now() < deadline => {
                // The transform may be between sink and src probes. Allow its
                // destination fence to arrive; no new input can be admitted.
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            result => return result,
        }
    }
}

/// An unresolved fence retains both input and destination until completion
/// can be proved. The caller quarantines native owners if that is impossible.
pub(crate) fn finish_release(state: &SharedImportState) -> Result<(), DmaBufBlocker> {
    let mut state = state.lock().map_err(|_| DmaBufBlocker::BufferOwnership)?;
    if state.pending_input.is_some() && state.pending_output.is_none() {
        // A failed transform never exposed its destination fence. Syncing the
        // source surface alone cannot prove that all VPP reads completed.
        return Err(DmaBufBlocker::BufferOwnership);
    }
    if let Some(buffer) = state.pending_output.as_ref() {
        native::sync(
            state
                .display
                .as_ref()
                .ok_or(DmaBufBlocker::ImportDeviceUnknown)?,
            buffer,
        )?;
    }
    state.pending_output = None;
    state.pending_input = None;
    Ok(())
}

pub(crate) fn share_va_display(
    encoder: &gst::Element,
    pipeline: &gst::Pipeline,
) -> Result<(), DmaBufBlocker> {
    let (_, context) = va_display(encoder)?;
    pipeline.set_context(&context);
    Ok(())
}

fn va_display(element: &gst::Element) -> Result<(gst::Object, gst::Context), DmaBufBlocker> {
    let mut query = gst::query::Context::new("gst.va.display.handle");
    let pad = element
        .static_pad("sink")
        .ok_or(DmaBufBlocker::ImportDeviceUnknown)?;
    if !pad.query(&mut query) {
        return Err(DmaBufBlocker::ImportDeviceUnknown);
    }
    let context = query
        .context_owned()
        .ok_or(DmaBufBlocker::ImportDeviceUnknown)?;
    let display = context
        .structure()
        .get::<gst::Object>("gst-display")
        .map_err(|_| DmaBufBlocker::ImportDeviceUnknown)?;
    Ok((display, context))
}

fn exact_import_caps(caps: &gst::CapsRef) -> gst::Caps {
    let mut result = gst::Caps::new_empty();
    for (structure, features) in caps.iter_with_features() {
        if !features.contains("memory:DMABuf")
            || !structure
                .get::<String>("format")
                .is_ok_and(|f| f == "DMA_DRM")
        {
            continue;
        }
        let formats = if let Ok(format) = structure.get::<String>("drm-format") {
            vec![format]
        } else if let Ok(list) = structure.get::<gst::List>("drm-format") {
            list.iter().filter_map(|v| v.get::<String>().ok()).collect()
        } else {
            Vec::new()
        };
        for format in formats
            .into_iter()
            .filter(|f| DmaDrmFormat::parse(f).is_some())
        {
            let mut structure = structure.to_owned();
            structure.set("drm-format", format);
            result
                .make_mut()
                .append_structure_full(structure, Some(features.to_owned()));
        }
    }
    result
}

fn validate_input(
    pad: &gst::Pad,
    buffer: &gst::BufferRef,
    supported: &gst::CapsRef,
    encoder_device: &DrmDeviceIdentity,
    state: &mut ImportState,
) -> Result<gst::Caps, DmaBufBlocker> {
    if state.pending_input.is_some() || state.pending_output.is_some() {
        return Err(DmaBufBlocker::BufferOwnership);
    }
    let caps = pad
        .current_caps()
        .ok_or(DmaBufBlocker::ImportCapsUnsupported)?;
    if !caps.is_fixed() || !caps.is_subset(supported) {
        return Err(DmaBufBlocker::ModifierUnsupported);
    }
    let format = caps
        .structure(0)
        .and_then(|s| s.get::<String>("drm-format").ok())
        .as_deref()
        .and_then(DmaDrmFormat::parse)
        .ok_or(DmaBufBlocker::ModifierUnsupported)?;
    if state
        .caps
        .as_ref()
        .is_some_and(|previous| *previous != caps)
    {
        return Err(DmaBufBlocker::NegotiatedStreamChanged);
    }
    if buffer.n_memory() == 0
        || buffer.flags().contains(gst::BufferFlags::CORRUPTED)
        || buffer.pts().is_none()
    {
        return Err(DmaBufBlocker::MemoryTypeMismatch);
    }
    let mut capture = None;
    for memory in buffer.iter_memories() {
        if !memory.is_type("dmabuf") {
            return Err(DmaBufBlocker::MemoryTypeMismatch);
        }
        // Generic PipeWire allocators do not expose exporter device identity.
        // In that case actual PRIME import on the selected display is the
        // compatibility proof; do not invent a physical exporter identity.
        if let Some(device) = allocator_device(memory) {
            assess_dma_buf_import(Some(&device), Some(encoder_device), true)?;
            capture = Some(device);
        }
    }
    if state.capture_device.as_ref().is_some_and(|previous| {
        capture
            .as_ref()
            .is_none_or(|device| previous.physical_device != device.physical_device)
    }) {
        return Err(DmaBufBlocker::DeviceMismatch);
    }
    state.caps = Some(caps.clone());
    state.format = Some(format);
    state.capture_device = capture;
    Ok(caps)
}

fn allocator_device(memory: &gst::MemoryRef) -> Option<DrmDeviceIdentity> {
    let allocator = memory.allocator()?;
    let property = allocator.find_property("drm-device-path")?;
    if property.value_type() != gst::glib::Type::STRING {
        return None;
    }
    allocator
        .property_value("drm-device-path")
        .get::<Option<String>>()
        .ok()?
        .as_deref()
        .and_then(drm_identity)
}

pub(crate) fn device_path(element: &gst::Element) -> Option<String> {
    let property = element.find_property("device-path")?;
    if property.value_type() != gst::glib::Type::STRING {
        return None;
    }
    element
        .property_value("device-path")
        .get::<Option<String>>()
        .ok()
        .flatten()
}

// Pointer borrowing is confined here. The native API never retains or closes
// Rust-owned descriptors/objects; state owns refs through GPU completion.
#[cfg(frame_linux_va_import)]
#[allow(unsafe_code)]
mod native {
    use super::*;
    use std::ffi::{c_int, c_void};
    unsafe extern "C" {
        fn freym_va_import_buffer(
            display: *mut c_void,
            caps: *mut c_void,
            buffer: *mut c_void,
            imported: *mut *mut gst::ffi::GstBuffer,
        ) -> c_int;
        fn freym_va_sync_buffer(display: *mut c_void, buffer: *mut c_void) -> c_int;
    }
    fn result(code: c_int) -> Result<(), DmaBufBlocker> {
        match code {
            0 => Ok(()),
            1 => Err(DmaBufBlocker::InvalidPlaneLayout),
            2 => Err(DmaBufBlocker::MemoryTypeMismatch),
            3 => Err(DmaBufBlocker::SynchronizationUnsupported),
            4 => Err(DmaBufBlocker::ImportFailed),
            5 => Err(DmaBufBlocker::DeviceMismatch),
            _ => Err(DmaBufBlocker::SynchronizationFailed),
        }
    }
    pub(super) fn import(
        display: &gst::Object,
        caps: &gst::CapsRef,
        buffer: &gst::BufferRef,
    ) -> Result<gst::Buffer, DmaBufBlocker> {
        use gst::glib::translate::from_glib_full;
        let mut imported = std::ptr::null_mut();
        // SAFETY: Borrowed objects live through this synchronous call. Surface
        // qdata is owned by GstMemory, never by the C caller.
        result(unsafe {
            freym_va_import_buffer(
                display.as_ptr().cast(),
                caps.as_mut_ptr().cast(),
                buffer.as_mut_ptr().cast(),
                &mut imported,
            )
        })?;
        if imported.is_null() {
            return Err(DmaBufBlocker::ImportFailed);
        }
        // SAFETY: The native function transfers one GstBuffer ref on success.
        Ok(unsafe { from_glib_full(imported) })
    }
    pub(super) fn sync(
        display: &gst::Object,
        buffer: &gst::BufferRef,
    ) -> Result<(), DmaBufBlocker> {
        // SAFETY: Borrowed live objects; this only waits for VA GPU work.
        result(unsafe { freym_va_sync_buffer(display.as_ptr().cast(), buffer.as_mut_ptr().cast()) })
    }
}

#[cfg(not(frame_linux_va_import))]
mod native {
    use super::*;
    pub(super) fn import(
        _: &gst::Object,
        _: &gst::CapsRef,
        _: &gst::BufferRef,
    ) -> Result<gst::Buffer, DmaBufBlocker> {
        Err(DmaBufBlocker::ImportCapsUnsupported)
    }
    pub(super) fn sync(_: &gst::Object, _: &gst::BufferRef) -> Result<(), DmaBufBlocker> {
        Err(DmaBufBlocker::ImportCapsUnsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negotiation_advertises_only_enumerated_valid_modifiers() {
        gst::init().unwrap();
        let caps: gst::Caps = "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=(string){NV12,AR24:0x0100000000000002,NV12:0x00ffffffffffffff};video/x-raw(memory:DMABuf),format=DMA_DRM;video/x-raw,format=NV12".parse().unwrap();
        let exact = exact_import_caps(&caps);
        assert_eq!(exact.size(), 2);
        for value in ["NV12", "AR24:0x0100000000000002"] {
            let proposed = gst::Caps::builder("video/x-raw")
                .features(["memory:DMABuf"])
                .field("format", "DMA_DRM")
                .field("drm-format", value)
                .build();
            assert!(proposed.is_subset(&exact));
        }
        let other: gst::Caps =
            "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=AR24:0x0100000000000003"
                .parse()
                .unwrap();
        assert!(!other.can_intersect(&exact));
    }
}
