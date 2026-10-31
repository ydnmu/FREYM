//! Unit tests for the bounded preview transport and the pure preview arithmetic
//! (FRAME-0208).
//!
//! The D3D11 reduction itself needs a real device and is covered by the
//! `preview_probe` example against the live desktop; these tests cover the parts that
//! must hold without a GPU: the one-latest-frame policy, the single in-flight rule, the
//! 10 fps and explicit Native bounds, the payload preflight and the encoder output.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;

fn image(width: u32, height: u32, marker: u8) -> PreviewImage {
    PreviewImage {
        width,
        height,
        sequence: 0,
        first_of_session: false,
        source_media_ns: Some(u64::from(marker)),
        session_generation: 0,
        mime_type: PREVIEW_IMAGE_MIME,
        data: Ok(vec![marker; 8]),
    }
}

fn hub() -> Arc<PreviewHub> {
    PreviewHub::new(PreviewConfig::default(), 7)
}

#[test]
fn the_request_bounds_are_enforced_rather_than_clamped() {
    assert!(PreviewConfig::default().validate().is_ok());
    assert_eq!(
        PreviewConfig {
            max_width: 0,
            ..PreviewConfig::default()
        }
        .validate()
        .expect_err("a zero width has no pixels")
        .code,
        ErrorCode::InvalidRequest
    );
    assert!(preview_geometry(PreviewConfig::default(), 4097, 2160).is_err());
    assert_eq!(
        PreviewConfig {
            max_width: PREVIEW_MAX_WIDTH + 1,
            ..PreviewConfig::default()
        }
        .validate()
        .expect_err("past the width bound is refused")
        .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        PreviewConfig {
            max_height: PREVIEW_MAX_HEIGHT + 1,
            ..PreviewConfig::default()
        }
        .validate()
        .expect_err("past the height bound is refused")
        .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        PreviewConfig {
            min_interval: PREVIEW_MIN_INTERVAL - Duration::from_millis(1),
            ..PreviewConfig::default()
        }
        .validate()
        .expect_err("faster than 10 fps is refused")
        .code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn real_source_geometry_and_overlapping_stages_count_against_the_session_budget() {
    let config = PreviewConfig::reduced(1920, 1080);
    let capture = crate::surface::plan_owned_payload(1920, 1080, false).unwrap();
    let stage = preview_payload_plan(config, 1920, 1080, capture.total_bytes, 0).unwrap();
    assert_eq!(stage, 1920 * 1080 * 4);

    // Four large capture slots still fit by themselves, but their real full-size
    // preview intermediate does not. This refusal happens before any texture exists.
    let large = crate::surface::plan_owned_payload(7680, 4320, false).unwrap();
    let error = preview_payload_plan(config, 7680, 4320, large.total_bytes, 0)
        .expect_err("the full-source intermediate must be counted");
    assert!(error.message.contains("before allocation"), "{error:?}");

    // A first stage can fit while a geometry change cannot: the old resources are
    // retained until the replacement stage has finished allocating.
    let capture = crate::surface::plan_owned_payload(5900, 3600, false).unwrap();
    let old = preview_payload_plan(config, 5900, 3600, capture.total_bytes, 0).unwrap();
    let error = preview_payload_plan(config, 5800, 3600, capture.total_bytes, old)
        .expect_err("old and new stages together exceed the session limit");
    assert!(
        error.message.contains("previous preview stage"),
        "{error:?}"
    );
}

#[test]
fn native_4k_preview_admits_two_cpu_images_inside_the_payload_budget() {
    let capture = crate::surface::plan_owned_payload(4096, 4096, false).unwrap();
    let stage =
        preview_payload_plan(PreviewConfig::default(), 4096, 4096, capture.total_bytes, 0).unwrap();
    let image_bytes = 4096_u64 * 4096 * 4;
    assert_eq!(stage, image_bytes);
    assert!(capture.total_bytes + stage + image_bytes * 2 <= crate::surface::MAX_PAYLOAD_BYTES);
}

#[test]
fn one_staged_frame_completes_without_a_second_source_frame() {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
        D3D11CreateDevice,
    };

    // WARP is a software D3D11 device. This tests only the small staging readback;
    // it opens no duplication, display, or recording session.
    let (mut device, mut context) = (None, None);
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_WARP,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .expect("WARP creates a software device");
    let device = device.unwrap();
    let context = context.unwrap();
    let staging = create_texture(&device, 4, 4, TextureRole::Staging).unwrap();
    let pixels = [37u8; 4 * 4 * 4];
    let description = D3D11_TEXTURE2D_DESC {
        Width: 4,
        Height: 4,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: 4 * 4,
        SysMemSlicePitch: 0,
    };
    let mut source = None;
    unsafe {
        device
            .CreateTexture2D(&description, Some(&initial), Some(&mut source))
            .unwrap();
        context.CopyResource(&staging, &source.unwrap());
        context.Flush();
    }

    let counters = Arc::new(PreviewCounters::default());
    let mut generator = ThumbnailGenerator::create(
        Some(device),
        Some(context),
        PreviewConfig::default(),
        0,
        &counters,
    )
    .unwrap();
    generator.staging = Some(staging);
    generator.staging_size = (4, 4);
    generator.pending = Some(PendingPreview {
        geometry: preview_geometry(PreviewConfig::default(), 4, 4).unwrap(),
        source_media_ns: Some(123),
    });
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    let completed = loop {
        if let Some(image) = generator.complete_pending(&mut output).unwrap() {
            break image.0;
        }
        assert!(Instant::now() < deadline, "the one readback must complete");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(completed.source_media_ns, Some(123));
    assert!(completed.encoded().is_some());
    assert_eq!(output, pixels);
    assert!(!generator.has_pending());
    assert_eq!(counters.snapshot().generated, 1);
}

#[test]
fn the_whole_source_fits_selected_bounds_without_upscaling_or_integer_ratio_loss() {
    let native = PreviewConfig::default();
    assert_eq!(preview_geometry(native, 2560, 1600).unwrap().width, 2560);
    assert_eq!(preview_geometry(native, 2560, 1600).unwrap().height, 1600);
    assert_eq!(
        preview_geometry(native, 2560, 1600).unwrap().source_box,
        None
    );
    let config = PreviewConfig::reduced(1920, 1080);
    for ((source_width, source_height), expected) in [
        ((2560, 1440), (1920, 1080)),
        ((2560, 1600), (1728, 1080)),
        ((3840, 2160), (1920, 1080)),
        ((3440, 1440), (1920, 803)),
        ((1440, 2560), (607, 1080)),
    ] {
        let geometry = preview_geometry(config, source_width, source_height).unwrap();
        assert_eq!((geometry.width, geometry.height), expected);
        assert_eq!(
            geometry.source_box,
            Some(PreviewBox {
                left: 0,
                top: 0,
                right: source_width,
                bottom: source_height,
            })
        );
    }
    for (width, height) in [(1920, 1080), (1366, 768), (800, 600), (320, 200)] {
        let geometry = preview_geometry(config, width, height).unwrap();
        assert_eq!((geometry.width, geometry.height), (width, height));
        assert_eq!(geometry.source_box, None);
    }
    assert!(preview_geometry(config, 0, 1080).is_err());
    let small = preview_geometry(PreviewConfig::reduced(1280, 720), 2560, 1600).unwrap();
    assert_eq!((small.width, small.height), (1152, 720));
    assert_eq!(small.source_box.unwrap().right, 2560);
}

#[test]
fn the_hub_keeps_one_latest_image_and_replaces_an_untaken_one() {
    let hub = hub();
    let counters = hub.counters();
    assert_eq!(counters, PreviewCounterSnapshot::default());
    assert!(counters.is_idle(), "a fresh hub did no work");

    hub.publish(image(640, 360, 1)).expect("the first image");
    hub.publish(image(640, 360, 2)).expect("the second image");
    let counters = hub.counters();
    assert_eq!(counters.published, 2);
    assert_eq!(
        counters.replaced_not_taken, 1,
        "an untaken image is replaced instead of queued"
    );
    assert_eq!(hub.pending_image().expect("one image").sequence, 2);
    assert!(
        !hub.pending_image().expect("one image").first_of_session,
        "only the first image of a hub is marked as the first of the session"
    );
}

#[test]
fn the_hub_hands_out_one_unacknowledged_image_at_a_time() {
    let hub = hub();
    hub.publish(image(640, 360, 1)).expect("the first image");
    let first = hub.take().expect("the pending image is handed out");
    assert_eq!(first.sequence, 1);
    // While the consumer holds the image, a newer one replaces the pending slot
    // without being lost, and it is not delivered out of order.
    hub.publish(image(640, 360, 2)).expect("the second image");
    assert!(
        hub.take().is_none(),
        "a second image is not handed out before the first is acknowledged"
    );
    hub.acknowledge();
    let next = hub.take().expect("the next image is handed out after ack");
    assert_eq!(next.sequence, 2, "the newest image is delivered next");
    assert_eq!(next.encoded().expect("pixels"), &[2u8; 8]);
    // Acknowledging twice is a no-op, not a corrupt in-flight state.
    hub.acknowledge();
    hub.acknowledge();
    assert!(hub.take().is_none(), "no third image was ever published");
}

#[test]
fn the_first_image_is_marked_and_the_generation_travels_with_it() {
    let hub = hub();
    hub.publish(image(640, 360, 1)).expect("the first image");
    let first = hub.take().expect("the first image");
    assert!(first.first_of_session);
    assert_eq!(first.session_generation, 7);
    hub.acknowledge();
    hub.publish(image(640, 360, 2)).expect("the second image");
    let second = hub.take().expect("the second image");
    assert!(!second.first_of_session);
    assert_eq!(second.session_generation, 7);
    assert_eq!(hub.generation(), 7);
}

#[test]
fn an_image_past_the_encoded_cap_is_published_as_an_error_instead_of_pixels() {
    let hub = hub();
    let oversize = PreviewImage {
        width: 640,
        height: 360,
        sequence: 0,
        first_of_session: false,
        source_media_ns: None,
        session_generation: 0,
        mime_type: PREVIEW_IMAGE_MIME,
        data: Ok(vec![0u8; MAX_PREVIEW_IMAGE_BYTES + 1]),
    };
    hub.publish(oversize)
        .expect("the hub publishes the failure");
    let published = hub.take().expect("one image");
    assert!(published.encoded().is_none());
    assert!(
        published
            .error()
            .expect("an error text")
            .contains("encoded-image cap"),
        "{published:?}"
    );
}

#[test]
fn generation_stops_as_soon_as_the_last_subscriber_detaches() {
    let hub = hub();
    assert!(!hub.enabled(), "an unattached hub publishes no work");
    let subscriber = hub.subscribe();
    assert!(hub.enabled());
    assert_eq!(hub.subscriber_count(), 1);
    let second = hub.subscribe();
    assert_eq!(hub.subscriber_count(), 2);
    drop(second);
    assert!(hub.enabled(), "one subscriber still wants images");
    drop(subscriber);
    assert!(!hub.enabled(), "no subscriber means no preview work");
    assert_eq!(hub.subscriber_count(), 0);
}

#[test]
fn a_new_subscriber_does_not_receive_the_previous_subscribers_image() {
    let hub = hub();
    assert_eq!(hub.subscription_epoch(), 0);
    let first = hub.subscribe();
    assert_eq!(hub.subscription_epoch(), 1);
    hub.publish(image(640, 360, 1)).expect("one image");
    drop(first);
    assert!(hub.pending_image().is_none());
    hub.publish(image(640, 360, 9))
        .expect("an in-flight producer can finish after detach");

    let second = hub.subscribe();
    assert_eq!(hub.subscription_epoch(), 2);
    assert!(hub.take().is_none());
    hub.publish(image(640, 360, 2)).expect("a fresh image");
    assert_eq!(hub.take().expect("fresh image").source_media_ns, Some(2));
    drop(second);

    let initially_published = PreviewHub::new(PreviewConfig::default(), 8);
    initially_published
        .publish(image(640, 360, 3))
        .expect("an initial image");
    let first_viewer = initially_published.subscribe();
    assert_eq!(
        initially_published
            .take()
            .expect("initial image")
            .source_media_ns,
        Some(3)
    );
    drop(first_viewer);
}

#[test]
fn a_closed_hub_wakes_a_waiting_consumer_and_publishes_nothing_more() {
    let hub = hub();
    let subscriber = hub.subscribe();
    let waiter = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || hub.wait_for_image(Duration::from_secs(5)))
    };
    // Let the waiter enter its bounded wait, then close the hub from "the UI".
    std::thread::sleep(Duration::from_millis(50));
    hub.close();
    let outcome = waiter.join().expect("the waiter thread finishes");
    assert!(
        outcome.is_none(),
        "a closed hub reports closure instead of an empty wait"
    );
    assert!(!hub.enabled());
    assert!(
        hub.publish(image(640, 360, 1)).is_none(),
        "a closed hub accepts no image"
    );
    drop(subscriber);
}

#[test]
fn a_waiting_consumer_receives_the_next_image_inside_its_bound() {
    let hub = hub();
    let _subscriber = hub.subscribe();
    let waiter = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || hub.wait_for_image(Duration::from_secs(2)))
    };
    hub.publish(image(640, 360, 9)).expect("one image");
    let outcome = waiter
        .join()
        .expect("the waiter thread finishes")
        .expect("the waiter did not observe closure")
        .expect("an image arrived inside the bound");
    assert_eq!(outcome.encoded().expect("pixels"), &[9u8; 8]);
    assert_eq!(outcome.width, 640);
    assert_eq!(outcome.height, 360);
}

#[test]
fn an_empty_wait_expires_instead_of_blocking_forever() {
    let hub = hub();
    let _subscriber = hub.subscribe();
    let started = Instant::now();
    let outcome = hub.wait_for_image(Duration::from_millis(20));
    assert!(matches!(outcome, Some(Err(()))), "{outcome:?}");
    assert!(started.elapsed() >= Duration::from_millis(15));
}

#[test]
fn the_encoder_writes_a_structurally_valid_png_and_rejects_bad_input() {
    assert_eq!(crc_of(b"123456789"), 0xcbf4_3926);
    // A 3x2 image, one distinct BGRA quadruple per pixel.
    let mut bgra = Vec::new();
    for index in 0..6u8 {
        bgra.extend_from_slice(&[index, 255 - index, index * 10, 255]);
    }
    let mut png = Vec::new();
    bgra[3] = 0; // Capture alpha does not control preview opacity.
    encode_png(&bgra, 3, 2, &mut png).expect("a valid image encodes");
    assert_eq!(
        &png[0..8],
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
    );
    // IHDR carries the real geometry and opaque 8-bit truecolor.
    assert_eq!(&png[12..16], b"IHDR");
    assert_eq!(u32::from_be_bytes([png[16], png[17], png[18], png[19]]), 3);
    assert_eq!(u32::from_be_bytes([png[20], png[21], png[22], png[23]]), 2);
    assert_eq!(png[24], 8);
    assert_eq!(png[25], 2);
    // Every chunk CRC is correct and the stream ends with IEND.
    for (offset, length, kind) in chunk_offsets(&png) {
        let covered = &png[offset + 4..offset + 8 + length];
        let carried = u32::from_be_bytes(
            png[offset + 8 + length..offset + 12 + length]
                .try_into()
                .unwrap(),
        );
        assert_eq!(crc_of(covered), carried, "{kind} CRC");
    }
    assert!(png.ends_with(b"IEND\xae\x42\x60\x82"));
    // Decode IDAT to check channel order and to ensure source alpha was ignored.
    let (offset, length, _) = chunk_offsets(&png)
        .into_iter()
        .find(|(_, _, kind)| kind == "IDAT")
        .expect("IDAT exists");
    let mut decoded = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::ZlibDecoder::new(&png[offset + 8..offset + 8 + length]),
        &mut decoded,
    )
    .expect("IDAT decompresses");
    assert_eq!(decoded.len(), 2 * (1 + 3 * 3));
    assert_eq!(&decoded[0..4], &[0, 0, 255, 0]);
    assert_eq!(&decoded[10..14], &[0, 30, 252, 3]);
    assert_eq!(
        encode_png(&bgra, 4, 2, &mut Vec::new())
            .expect_err("a mismatched size is refused")
            .code,
        ErrorCode::Internal
    );
    assert_eq!(
        encode_png(&bgra, 0, 2, &mut Vec::new())
            .expect_err("a zero width is refused")
            .code,
        ErrorCode::Internal
    );
}

#[test]
fn the_encoder_preserves_rgb_at_vector_boundaries() {
    for width in [4u32, 7] {
        let mut bgra = Vec::new();
        let mut expected = vec![0u8]; // PNG filter None.
        for index in 0..width as u8 {
            bgra.extend_from_slice(&[index + 1, index + 30, index + 100, 0]);
            expected.extend_from_slice(&[index + 100, index + 30, index + 1]);
        }
        let mut png = Vec::new();
        encode_png(&bgra, width, 1, &mut png).expect("the row encodes");
        let (offset, length, _) = chunk_offsets(&png)
            .into_iter()
            .find(|(_, _, kind)| kind == "IDAT")
            .expect("IDAT exists");
        let mut decoded = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::ZlibDecoder::new(&png[offset + 8..offset + 8 + length]),
            &mut decoded,
        )
        .expect("IDAT decompresses");
        assert_eq!(decoded, expected, "width {width}");
    }
}

#[test]
fn a_native_source_image_stays_inside_the_encoded_cap() {
    // Exercise the requested native desktop size without allocating a synthetic 4K square.
    let width = 2560;
    let height = 1600;
    let mut bgra = vec![0u8; (width * height * 4) as usize];
    for (index, byte) in bgra.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    let mut png = Vec::new();
    let started = Instant::now();
    encode_png(&bgra, width, height, &mut png).expect("the native image encodes");
    eprintln!(
        "preview fixture: {} bytes in {:?}",
        png.len(),
        started.elapsed()
    );
    assert!(
        png.len() <= MAX_PREVIEW_IMAGE_BYTES,
        "{} bytes past the cap",
        png.len()
    );
}

/// Walks the PNG chunks and returns `(offset, data length, kind)` for each one.
fn chunk_offsets(png: &[u8]) -> Vec<(usize, usize, String)> {
    let mut chunks = Vec::new();
    let mut offset = 8usize;
    while offset + 12 <= png.len() {
        let length = u32::from_be_bytes([
            png[offset],
            png[offset + 1],
            png[offset + 2],
            png[offset + 3],
        ]) as usize;
        let kind = String::from_utf8_lossy(&png[offset + 4..offset + 8]).to_string();
        chunks.push((offset, length, kind));
        offset += 12 + length;
    }
    chunks
}

/// Recomputes the CRC over `type + data` for one chunk.
fn crc_of(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finish()
}

#[test]
fn slow_preview_consumer_cannot_block_publication_or_request_recording_readback() {
    let hub = hub();
    assert!(hub.cpu_preview_allowed());
    hub.set_recording();
    assert!(!hub.cpu_preview_allowed());
    let held = hub.slot.lock().unwrap();
    assert_eq!(hub.publish(image(32, 24, 9)), None);
    assert_eq!(hub.counters().publication_contention, 1);
    drop(held);
    assert!(hub.publish(image(32, 24, 10)).is_some());
}
