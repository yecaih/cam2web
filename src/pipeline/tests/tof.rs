use super::{Frame, FrameIdentity, Layout, Module, Pipeline, PixelFormat, config};
use crate::pipeline::{
    builtins::HimaxTofMetadata,
    metadata::{FrameMetadata, TofChannel},
};
use crate::v4l::{CaptureSpec, CapturedFrame};
use image::{ImageFormat, load_from_memory_with_format};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 481;
const STRIDE_BYTES: u32 = 1344;
const METADATA: &str = "[[modules]]\nid = 'metadata'\nkind = 'himax_tof_metadata'\n";
const PAIR_AND_JPEG: &str = "[[modules]]\nid = 'tof'\nkind = 'himax_tof_pair'\n[output]\nid = 'jpeg'\nkind = 'jpeg_encode'\nquality = 100\n";

fn tof_chain() -> super::super::PipelineConfig {
    config(&format!("{METADATA}{PAIR_AND_JPEG}"))
}

fn spec() -> CaptureSpec {
    CaptureSpec {
        fourcc: "RGGB".into(),
        width: WIDTH,
        height: HEIGHT,
        stride_bytes: STRIDE_BYTES,
        size_bytes: HEIGHT * STRIDE_BYTES,
        interval: None,
    }
}

fn captured(sequence: u32, counter: u16, flag: u8) -> CapturedFrame {
    let mut bytes = vec![0xff; (STRIDE_BYTES * (HEIGHT - 1) + WIDTH) as usize];
    let mut rows = bytes.chunks_mut(STRIDE_BYTES as usize);
    for row in rows.by_ref().take((HEIGHT - 1) as usize) {
        for (col_index, sample) in row[..WIDTH as usize].chunks_exact_mut(2).enumerate() {
            let word = match (flag, col_index) {
                (1, 0..320) => 256_u16 << 2,
                (1, _) => 2048_u16 << 2,
                _ => 255_u16 << 4,
            };
            sample.copy_from_slice(&word.to_le_bytes());
        }
    }
    let embedded = rows.next().unwrap();
    embedded[0] = 0xab;
    embedded[1] = flag;
    embedded[2..4].copy_from_slice(&counter.to_le_bytes());
    embedded[4..6].copy_from_slice(&[25, 10]);
    embedded[6..8].copy_from_slice(&2025_u16.to_le_bytes());
    embedded[8..12].copy_from_slice(&[16, 8, 14, 0xcd]);
    embedded[12..16].copy_from_slice(&185_658_u32.to_le_bytes());
    embedded[16..32].fill(0x10);
    embedded[32..64].fill(flag);
    CapturedFrame { bytes, sequence }
}

fn decode(jpeg: &[u8]) -> image::GrayImage {
    load_from_memory_with_format(jpeg, ImageFormat::Jpeg)
        .unwrap()
        .to_luma8()
}

#[test]
fn metadata_stage_exposes_the_sensor_geometry_and_excludes_the_embedded_line() {
    let mut parser = HimaxTofMetadata {};
    let input = Layout {
        format: PixelFormat::Rggb,
        width: WIDTH,
        height: HEIGHT,
        stride_bytes: STRIDE_BYTES,
    };
    let output = parser.output_layout(input).unwrap();
    assert_eq!(output.format, PixelFormat::HimaxTof16);
    assert_eq!(
        (output.width, output.height, output.stride_bytes),
        (640, 480, STRIDE_BYTES)
    );
    let frame = Frame {
        identity: FrameIdentity {
            session_id: 1,
            sequence: 8,
        },
        layout: input,
        plane: captured(8, 123, 1).bytes,
        metadata: FrameMetadata::None,
    };
    let frame = parser.process(frame, output).unwrap().unwrap();
    assert_eq!(frame.plane.len(), (479 * STRIDE_BYTES + WIDTH) as usize);
    assert!(
        matches!(frame.metadata, FrameMetadata::TofSample(TofChannel::Depth, metadata) if metadata.sensor_frame_count == 123)
    );
}

#[test]
fn pairs_in_both_orders_and_preserves_depth_left_nir_right() {
    for (first, second) in [(0, 1), (1, 0)] {
        let mut pipeline = Pipeline::build(&tof_chain(), &spec()).unwrap();
        assert!(
            pipeline
                .process(9, captured(u32::MAX, u16::MAX, first))
                .unwrap()
                .is_none()
        );
        let processed = pipeline
            .process(9, captured(0, 0, second))
            .unwrap()
            .unwrap();
        let pair = processed.tof_metadata.unwrap();
        let (depth, nir) = (pair.depth.unwrap(), pair.nir.unwrap());
        assert_eq!(
            depth.sensor_frame_count,
            if first == 1 { u16::MAX } else { 0 }
        );
        assert_eq!(
            nir.sensor_frame_count,
            if first == 0 { u16::MAX } else { 0 }
        );
        assert_eq!(
            depth.capture_sequence,
            if first == 1 { u32::MAX } else { 0 }
        );
        assert_eq!((depth.frame_flag, nir.frame_flag), (1, 0));
        assert_eq!((depth.dummy_byte, depth.padding_byte), (0xab, 0xcd));
        assert_eq!(depth.reserved_bytes, [0x10; 16]);
        assert_eq!(depth.undocumented_bytes, [1; 32]);
        assert_eq!(nir.undocumented_bytes, [0; 32]);
        let clock = depth.sensor_clock;
        assert_eq!((clock.year, clock.month, clock.day), (2025, 10, 25));
        assert_eq!(
            (clock.hour, clock.minute, clock.second, clock.microsecond),
            (14, 8, 16, 185_658)
        );
        let decoded = decode(&processed.jpeg);
        assert_eq!(decoded.dimensions(), (1280, 480));
        for y in [20, 470] {
            assert!(decoded.get_pixel(20, y)[0] < 4);
            assert!(decoded.get_pixel(620, y)[0] > 251);
            assert!(decoded.get_pixel(1000, y)[0].abs_diff(170) < 4);
        }
    }
}

#[test]
fn renders_single_channel_sensors_with_a_blank_half() {
    // 单通道 depth 用百分位归一化（亮部 255），单通道 nir 用对数 LUT（满量程 170）。
    for (flag, blank_x, lit_x, lit_level) in [(1u8, 1000, 600, 255), (0, 20, 1000, 170)] {
        let mut pipeline = Pipeline::build(&tof_chain(), &spec()).unwrap();
        assert!(pipeline.process(1, captured(0, 0, flag)).unwrap().is_none());
        for sequence in 1..3 {
            let processed = pipeline
                .process(
                    1,
                    captured(sequence, u16::try_from(sequence).unwrap(), flag),
                )
                .unwrap()
                .unwrap();
            let pair = processed.tof_metadata.unwrap();
            assert_eq!(pair.depth.is_some(), flag == 1);
            assert_eq!(pair.nir.is_some(), flag == 0);
            let decoded = decode(&processed.jpeg);
            assert_eq!(decoded.dimensions(), (1280, 480));
            assert!(decoded.get_pixel(blank_x, 20)[0] < 4);
            assert!(decoded.get_pixel(lit_x, 20)[0].abs_diff(lit_level) < 4);
        }
    }
}

#[test]
fn renders_unpaired_halves_instead_of_dropping_them() {
    let mut pipeline = Pipeline::build(&tof_chain(), &spec()).unwrap();
    assert!(pipeline.process(1, captured(1, 1, 0)).unwrap().is_none());
    for (session, sequence, counter, flag, has_depth) in [
        (1, 3, 2, 1, false), // 用户态漏了一个 V4L2 buffer
        (1, 4, 4, 0, true),  // 传感器跳了一帧
        (2, 5, 5, 1, false), // 换了采集会话
        (2, 6, 6, 1, true),  // 同一通道连出两帧
    ] {
        let processed = pipeline
            .process(session, captured(sequence, counter, flag))
            .unwrap()
            .unwrap();
        let pair = processed.tof_metadata.unwrap();
        assert_eq!(
            (pair.depth.is_some(), pair.nir.is_some()),
            (has_depth, !has_depth)
        );
        let decoded = decode(&processed.jpeg);
        assert_eq!(decoded.dimensions(), (1280, 480));
        assert!(decoded.get_pixel(if has_depth { 1000 } else { 20 }, 20)[0] < 4);
    }
    let pair = pipeline
        .process(2, captured(7, 7, 0))
        .unwrap()
        .unwrap()
        .tof_metadata
        .unwrap();
    assert!(pair.depth.is_some() && pair.nir.is_some());
}

#[test]
fn rejects_wrong_layout_and_unknown_frame_flag() {
    assert!(Pipeline::validate(&config(PAIR_AND_JPEG), "RGGB").is_err());
    let config = tof_chain();
    let mut invalid = spec();
    invalid.height -= 1;
    assert!(Pipeline::build(&config, &invalid).is_err());
    let mut pipeline = Pipeline::build(&config, &spec()).unwrap();
    let error = pipeline.process(1, captured(1, 1, 2)).unwrap_err();
    assert!(format!("{error:#}").contains("unknown Himax ToF embedded frame flag"));
}
