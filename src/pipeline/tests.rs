mod tof;

use super::{
    Pipeline, PipelineConfig,
    builtins::ToRgb,
    frame::{Frame, FrameIdentity, Layout, PixelFormat},
    module::Module,
};
use crate::v4l::{CaptureSpec, CapturedFrame};
use image::{ImageFormat, load_from_memory_with_format};
use serde_json::json;

const JPEG_OUTPUT: &str = "[output]\nid = 'jpeg'\nkind = 'jpeg_encode'\n";
const TO_RGB: &str = "[[modules]]\nid = 'convert'\nkind = 'to_rgb'\n";

fn config(declarations: &str) -> PipelineConfig {
    toml::from_str(declarations).expect("valid TOML schema")
}

fn capture_spec(fourcc: &str, width: u32, stride_bytes: u32) -> CaptureSpec {
    CaptureSpec {
        fourcc: fourcc.to_owned(),
        width,
        height: 2,
        stride_bytes,
        size_bytes: 2 * stride_bytes,
        interval: None,
    }
}

fn captured(bytes: &[u8]) -> CapturedFrame {
    CapturedFrame {
        bytes: bytes.to_vec(),
        sequence: 42,
    }
}

#[test]
fn yuyv_skips_row_padding_and_preserves_identity() {
    let identity = FrameIdentity {
        session_id: 7,
        sequence: 42,
    };
    let mut converter = ToRgb {};
    let layout = Layout {
        format: PixelFormat::Yuyv,
        width: 2,
        height: 2,
        stride_bytes: 8,
    };
    let output = converter.output_layout(layout).unwrap();
    let frame = converter
        .process(
            Frame {
                identity,
                layout,
                plane: vec![16, 128, 235, 128, 255, 255, 255, 255, 81, 90, 81, 240],
                metadata: Default::default(),
            },
            output,
        )
        .unwrap()
        .unwrap();
    assert_eq!(frame.identity, identity);
    assert_eq!(frame.layout.stride_bytes, 6);
    assert_eq!(frame.plane, [0, 0, 0, 255, 255, 255, 255, 0, 0, 255, 0, 0]);
}

#[test]
fn grey_conversion_resize_and_encode_follow_config_order() {
    let config = config(&format!(
        "{TO_RGB}[[modules]]\nid = 'resize'\nkind = 'resize_nearest'\nwidth = 1\nheight = 2\n{JPEG_OUTPUT}quality = 100\n"
    ));
    Pipeline::validate(&config, "GREY").unwrap();
    let mut pipeline = Pipeline::build(&config, &capture_spec("GREY", 2, 3)).unwrap();
    let (frame, _) = pipeline
        .process_frame(9, captured(&[0, 255, 127, 255, 0]))
        .unwrap()
        .unwrap();
    assert_eq!(
        frame.identity,
        FrameIdentity {
            session_id: 9,
            sequence: 42
        }
    );
    assert_eq!(frame.layout.format, PixelFormat::Jpeg);
    assert!(frame.plane.starts_with(&[0xff, 0xd8]));
    assert!(frame.plane.ends_with(&[0xff, 0xd9]));
    let decoded = load_from_memory_with_format(&frame.plane, ImageFormat::Jpeg)
        .unwrap()
        .to_rgb8();
    assert_eq!(decoded.dimensions(), (1, 2));
    assert!(decoded.get_pixel(0, 0).0.iter().all(|&sample| sample < 6));
    assert!(decoded.get_pixel(0, 1).0.iter().all(|&sample| sample > 249));
}

#[test]
fn rgb3_encoder_accepts_padded_rows_without_a_converter() {
    let mut pipeline = Pipeline::build(&config(JPEG_OUTPUT), &capture_spec("RGB3", 1, 5)).unwrap();
    let jpeg = pipeline
        .process(1, captured(&[0, 0, 0, 255, 0, 255, 255, 255]))
        .unwrap()
        .unwrap();
    let decoded = load_from_memory_with_format(&jpeg.jpeg, ImageFormat::Jpeg)
        .unwrap()
        .to_rgb8();
    assert_eq!(decoded.dimensions(), (1, 2));
    assert!(decoded.get_pixel(0, 0).0.iter().all(|&sample| sample < 15));
    assert!(decoded.get_pixel(0, 1).0.iter().all(|&sample| sample > 240));
}

#[test]
fn rejects_incompatible_order_and_invalid_module_options() {
    let resize = "[[modules]]\nid = 'resize'\nkind = 'resize_nearest'\nwidth = 1\nheight = 2\n";
    for (declarations, expected_error) in [
        (format!("{resize}{TO_RGB}{JPEG_OUTPUT}"), "expected RGB3"),
        (
            format!("{TO_RGB}{TO_RGB}{JPEG_OUTPUT}"),
            "duplicate module ID",
        ),
        (
            format!("{TO_RGB}typo = true\n{JPEG_OUTPUT}"),
            "unknown field",
        ),
        (format!("{TO_RGB}{JPEG_OUTPUT}quality = 0\n"), "1..=100"),
        (
            format!(
                "{TO_RGB}{}{JPEG_OUTPUT}",
                resize.replace("width = 1", "width = 0")
            ),
            "zero-sized",
        ),
        (JPEG_OUTPUT.replace("jpeg_encode", "to_rgb"), "unknown kind"),
    ] {
        let err = Pipeline::validate(&config(&declarations), "GREY").expect_err("invalid chain");
        assert!(format!("{err:#}").contains(expected_error), "{err:#}");
    }
    assert!(Pipeline::validate(&config(JPEG_OUTPUT), "RGGB").is_err());
}

#[test]
fn build_revalidates_format_and_negotiated_plane_layout() {
    let jpeg = config(JPEG_OUTPUT);
    Pipeline::validate(&jpeg, "RGB3").unwrap();
    assert!(Pipeline::build(&jpeg, &capture_spec("GREY", 2, 2)).is_ok());
    assert!(Pipeline::build(&jpeg, &capture_spec("RGGB", 2, 2)).is_err());
    let converter = config(&format!("{TO_RGB}{JPEG_OUTPUT}"));
    for spec in [
        capture_spec("YUYV", 3, 6),
        capture_spec("YUYV", 2, 3),
        capture_spec("GREY", 0, 1),
    ] {
        assert!(Pipeline::build(&converter, &spec).is_err());
    }
    let mut spec = capture_spec("YUYV", 2, 6);
    spec.size_bytes = 9;
    assert!(Pipeline::build(&converter, &spec).is_err());
    assert!(Pipeline::build(&converter, &capture_spec("GREY", 65_536, 65_536)).is_err());
}

#[test]
fn rejects_capture_length_outside_negotiated_bounds() {
    let mut pipeline = Pipeline::build(
        &config(&format!("{TO_RGB}{JPEG_OUTPUT}")),
        &capture_spec("YUYV", 2, 6),
    )
    .unwrap();
    for bytes in [&[0; 9][..], &[0; 13][..]] {
        let err = pipeline
            .process(23, captured(bytes))
            .unwrap_err()
            .to_string();
        assert!(err.contains("session_id: 23, sequence: 42"), "{err}");
        assert!(err.contains("expected 10..=12"), "{err}");
    }
    assert!(pipeline.process(23, captured(&[0; 10])).is_ok());
}

#[test]
fn jpeg_quality_updates_at_frame_boundary_without_changing_layout() {
    let mut pipeline = Pipeline::build(&config(JPEG_OUTPUT), &capture_spec("RGB3", 1, 3)).unwrap();
    let initial = &pipeline.parameters()[0];
    assert_eq!(initial.instance_id, "jpeg");
    assert_eq!(initial.parameter_id, "quality");
    assert_eq!(initial.value, json!(80));
    let plane = [13, 221, 54, 214, 1, 79];
    let before = pipeline.process(1, captured(&plane)).unwrap().unwrap();
    pipeline
        .set_parameter("jpeg", "quality", json!(20))
        .unwrap();
    for invalid in [json!(0), json!(101), json!(1.5), json!("90")] {
        assert!(pipeline.set_parameter("jpeg", "quality", invalid).is_err());
    }
    assert!(
        pipeline
            .set_parameter("absent", "quality", json!(30))
            .is_err()
    );
    assert!(pipeline.set_parameter("jpeg", "width", json!(30)).is_err());
    assert_eq!(pipeline.parameters()[0].value, json!(20));
    let after = pipeline.process(1, captured(&plane)).unwrap().unwrap();
    assert_ne!(before.jpeg, after.jpeg);
    assert_eq!(
        load_from_memory_with_format(&after.jpeg, ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8()
            .dimensions(),
        (1, 2)
    );
}
