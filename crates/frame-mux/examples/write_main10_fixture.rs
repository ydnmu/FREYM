//! Write the checked-in synthetic Main10 fixture through the production muxer.
//! Optional --incomplete-tail writes two pictures and leaves the session unfinished
//! so a copied/truncated tail can exercise recovery without opening any capture.
use std::num::NonZeroU32;
use std::path::Path;

use frame_clock::MediaTimestamp;
use frame_encode::{
    BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability, VideoColour,
};
use frame_mux::{ContainerWriter, MkvVideoWriter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let path = args
        .first()
        .ok_or("usage: write_main10_fixture <new.mkv> [--incomplete-tail]")?;
    let incomplete = match args.get(1).map(String::as_str) {
        None if args.len() == 1 => false,
        Some("--incomplete-tail") if args.len() == 2 => true,
        _ => return Err("usage: write_main10_fixture <new.mkv> [--incomplete-tail]".into()),
    };
    let bytes = include_bytes!("../tests/fixtures/synthetic-main10.hevc").to_vec();
    let stream = EncodedStreamInfo {
        capability: EncoderCapability {
            id: "synthetic-main10".into(),
            codec: "hevc".into(),
            hardware: false,
        },
        width: NonZeroU32::new(64).unwrap(),
        height: NonZeroU32::new(64).unwrap(),
        nominal_rate_num: NonZeroU32::new(1).unwrap(),
        nominal_rate_den: NonZeroU32::new(1).unwrap(),
        bitstream_format: BitstreamFormat::HevcAnnexB,
        colour: Some(VideoColour::BT2020_PQ_LIMITED),
        codec_config: bytes.clone(),
        allows_reordering: false,
    };
    let mut writer = MkvVideoWriter::create(Path::new(&path), &stream, MediaTimestamp(0))?;
    writer.write(EncodedPacket {
        track_id: 1,
        pts: MediaTimestamp(0),
        dts: MediaTimestamp(0),
        duration_ns: 1_000_000_000,
        keyframe: true,
        data: bytes.clone(),
    })?;
    if incomplete {
        writer.write(EncodedPacket {
            track_id: 1,
            pts: MediaTimestamp(1_000_000_000),
            dts: MediaTimestamp(1_000_000_000),
            duration_ns: 1_000_000_000,
            keyframe: true,
            data: bytes,
        })?;
        drop(writer);
    } else {
        writer.finalize()?;
    }
    Ok(())
}
