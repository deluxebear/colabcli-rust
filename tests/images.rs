use base64::{Engine, engine::general_purpose::STANDARD};
use colabcli_rust::execution::images::write_kitty_png;
use std::io::{self, Write};

#[test]
fn kitty_packets_preserve_bytes_with_valid_chunk_flags_and_no_terminal_responses() {
    // Synthetic wire payloads test framing, not PNG pixel decoding.
    for length in [1, 3072, 3073, 20000] {
        let input: Vec<_> = (0..length).map(|i| (i % 256) as u8).collect();
        let mut wire = Vec::new();
        write_kitty_png(&mut wire, &input).unwrap();
        let wire = String::from_utf8(wire).unwrap();
        let packets: Vec<_> = wire
            .trim_matches('\n')
            .split("\x1b\\")
            .filter(|s| !s.is_empty())
            .collect();
        let mut joined = String::new();
        for (index, packet) in packets.iter().enumerate() {
            let (control, payload) = packet
                .strip_prefix("\x1b_G")
                .unwrap()
                .split_once(';')
                .unwrap();
            assert!(payload.len() <= 4096);
            if index + 1 < packets.len() {
                assert_eq!(payload.len() % 4, 0);
                assert!(control.contains("m=1"));
            } else {
                assert!(control.contains("m=0"));
            }
            if index == 0 {
                assert!(control.contains("a=T,f=100,q=2"));
            } else {
                assert!(control == "m=1" || control == "m=0");
            }
            joined.push_str(payload);
        }
        assert_eq!(STANDARD.decode(joined).unwrap(), input);
    }
    let mut wire = Vec::new();
    write_kitty_png(&mut wire, b"").unwrap();
    assert!(wire.is_empty());
}

#[test]
fn kitty_partial_write_failure_is_returned_without_replaying_a_packet() {
    struct Broken {
        bytes: Vec<u8>,
    }
    impl Write for Broken {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let remaining = 20usize.saturating_sub(self.bytes.len());
            if remaining == 0 {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let count = remaining.min(bytes.len());
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Broken { bytes: Vec::new() };
    assert_eq!(
        write_kitty_png(&mut writer, &[7; 10000])
            .unwrap_err()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(writer.bytes.len(), 20);
    assert_eq!(writer.bytes.iter().filter(|b| **b == 0x1b).count(), 1);
}

#[test]
fn fixed_upstream_image_samples_preserve_transmitted_content_and_artifact_contract() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../research/image-output-fixtures.json")).unwrap();
    for case in fixtures["kitty_cases"].as_array().unwrap() {
        let input = STANDARD
            .decode(case["input_base64"].as_str().unwrap())
            .unwrap();
        let mut actual = Vec::new();
        // The real CLI TTY gate is covered by the PTY test, not this wire helper.
        if case["tty"] == true {
            write_kitty_png(&mut actual, &input).unwrap();
        }
        if case["tty"] == false {
            assert!(actual.is_empty());
            assert_eq!(case["official_stdout"], "");
            continue;
        }
        let actual = String::from_utf8(actual).unwrap();
        let joined: String = actual
            .trim_matches('\n')
            .split("\x1b\\")
            .filter(|s| !s.is_empty())
            .map(|packet| packet.split_once(';').unwrap().1)
            .collect();
        let original = case["official_stdout"].as_str().unwrap().trim_matches('\n');
        let upstream = original
            .strip_suffix("\x1b\\")
            .unwrap()
            .split_once(';')
            .unwrap()
            .1;
        assert_eq!(
            STANDARD.decode(joined).unwrap(),
            STANDARD.decode(upstream).unwrap()
        );
    }
    for case in fixtures["artifact_cases"].as_array().unwrap() {
        assert!(
            case["official_stdout"]
                .as_str()
                .unwrap()
                .contains("[Image saved to: <IMAGE_FILE>]")
        );
        if case["explicit"] == false {
            assert_eq!(case["saved_mode"], "0o600");
        }
        if case["mime"] == "image/jpeg" {
            assert_eq!(case["saved_extension"], ".jpeg");
        }
    }
}

fn jpeg(width: u32, height: u32, gray: bool) -> Vec<u8> {
    let channels = if gray { 1 } else { 3 };
    let pixels: Vec<u8> = (0..width as usize * height as usize * channels)
        .map(|index| ((index * 31) % 251) as u8)
        .collect();
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90)
        .encode(
            &pixels,
            width,
            height,
            if gray {
                image::ExtendedColorType::L8
            } else {
                image::ExtendedColorType::Rgb8
            },
        )
        .unwrap();
    bytes
}
#[test]
fn jpeg_terminal_projection_is_a_real_png_with_the_same_decoded_pixels() {
    for gray in [false, true] {
        let bytes = jpeg(17, 9, gray);
        let original = bytes.clone();
        let png = colabcli_rust::execution::images::jpeg_to_png(&bytes).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(bytes, original);
        let jpeg = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).unwrap();
        let png = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
        assert_eq!((png.width(), png.height()), (17, 9));
        assert_eq!(png.to_rgba8(), jpeg.to_rgba8());
    }
}
#[test]
fn jpeg_projection_rejects_bad_data_oversized_input_and_truncated_images() {
    let bytes = jpeg(2, 2, false);
    for bad in [
        vec![],
        b"private-invalid-image-body".to_vec(),
        bytes[..bytes.len() / 2].to_vec(),
        vec![0; 8 * 1024 * 1024 + 1],
    ] {
        let error = colabcli_rust::execution::images::jpeg_to_png(&bad)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("private-invalid-image-body"));
    }
}
#[test]
fn jpeg_projection_checks_advertised_dimensions_before_decoding_pixels() {
    for (width, height, pixel_limit) in [(8193_u16, 1_u16, false), (8192, 8192, true)] {
        let mut bytes = jpeg(1, 1, false);
        let sof = bytes
            .windows(2)
            .position(|pair| pair == [0xff, 0xc0])
            .unwrap();
        bytes[sof + 5..sof + 7].copy_from_slice(&height.to_be_bytes());
        bytes[sof + 7..sof + 9].copy_from_slice(&width.to_be_bytes());
        let error = colabcli_rust::execution::images::jpeg_to_png(&bytes)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(if pixel_limit {
                "像素数或解码大小超过"
            } else {
                "尺寸或编码无效"
            }),
            "{error}"
        );
    }
}
