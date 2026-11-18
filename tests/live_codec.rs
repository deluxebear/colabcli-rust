//! Replay only sanitized, fixed probe output derived from an owned Colab CPU.
use base64::{Engine, engine::general_purpose::STANDARD};
use colabcli_rust::jupyter::codec::{self, Frame};
use serde_json::Value;

#[test]
fn replay_real_colab_default_text_and_non_utf8_empty_buffer_samples() {
    let fixture: Value =
        serde_json::from_str(include_str!("../research/live-codec-samples.json")).unwrap();
    let samples = fixture["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 2);
    let mut kinds = Vec::new();
    for sample in samples {
        let raw = STANDARD
            .decode(sample["wire_base64"].as_str().unwrap())
            .unwrap();
        let kind = sample["wire_type"].as_str().unwrap();
        kinds.push(kind);
        let message = match kind {
            "text" => codec::decode_text(std::str::from_utf8(&raw).unwrap()).unwrap(),
            "binary" => codec::decode_binary(&raw).unwrap(),
            other => panic!("unexpected captured frame type: {other}"),
        };
        assert_eq!(message.envelope, sample["expected_envelope"]);
        assert_eq!(message.channel(), "iopub");
        match kind {
            "text" => {
                assert_eq!(message.kind(), "stream");
                assert_eq!(message.content()["text"], "COLABCLI_CODEC_TEXT\n");
                assert!(message.buffers.is_empty());
            }
            "binary" => {
                assert_eq!(message.kind(), "display_data");
                assert_eq!(
                    message.content()["data"]["text/plain"],
                    "COLABCLI_CODEC_BINARY"
                );
                assert_eq!(message.buffers, vec![vec![0, 255, 128, 65], vec![]]);
            }
            _ => unreachable!(),
        }
        let buffers: Vec<String> = message.buffers.iter().map(|b| STANDARD.encode(b)).collect();
        assert_eq!(
            serde_json::to_value(buffers).unwrap(),
            sample["buffers_base64"]
        );
        let roundtrip = match codec::encode(&message).unwrap() {
            Frame::Text(s) => codec::decode_text(&s).unwrap(),
            Frame::Binary(b) => codec::decode_binary(&b).unwrap(),
        };
        assert_eq!(roundtrip, message);
    }
    kinds.sort_unstable();
    assert_eq!(kinds, vec!["binary", "text"]);
}
