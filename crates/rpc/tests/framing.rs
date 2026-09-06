use elefante_rpc::*;
use serde_json::{Value, json};

#[test]
fn arbitrary_chunk_boundaries_preserve_unicode_and_escaped_newlines() {
    let values = [json!({"text":"hi\n🦀"}), json!({"id":2,"result":{}})];
    let bytes: Vec<_> = values
        .iter()
        .flat_map(|value| encode_frame(value).unwrap())
        .collect();
    let mut decoder = FrameDecoder::default();
    let mut received = Vec::new();
    for byte in bytes {
        for frame in decoder.push(&[byte]).unwrap() {
            received.push(decode_frame::<Value>(&frame).unwrap());
        }
    }
    decoder.finish().unwrap();
    assert_eq!(received, values);
    let frame = encode_frame(&values[0]).unwrap();
    assert_eq!(frame.last(), Some(&b'\n'));
    assert!(!frame[..frame.len() - 1].contains(&b'\n'));
}

#[test]
fn frame_limit_is_exact_and_overflow_is_terminal() {
    let value = Value::String("x".repeat(MAX_FRAME_BYTES - 2));
    let frame = encode_frame(&value).unwrap();
    assert_eq!(frame.len(), MAX_FRAME_BYTES + 1);
    assert_eq!(decode_frame::<Value>(&frame).unwrap(), value);
    let mut decoder = FrameDecoder::default();
    assert!(decoder.push(&frame[..MAX_FRAME_BYTES]).unwrap().is_empty());
    assert_eq!(decoder.push(b"\n").unwrap(), vec![frame]);
    decoder.finish().unwrap();
    assert!(matches!(
        encode_frame(&"x".repeat(MAX_FRAME_BYTES - 1)),
        Err(FrameError::TooLarge)
    ));
    let mut decoder = FrameDecoder::default();
    assert!(
        decoder
            .push(&vec![b' '; MAX_FRAME_BYTES])
            .unwrap()
            .is_empty()
    );
    assert!(matches!(decoder.push(b"x"), Err(FrameError::TooLarge)));
    assert!(matches!(decoder.push(b"{}\n"), Err(FrameError::TooLarge)));
    assert!(matches!(decoder.finish(), Err(FrameError::TooLarge)));
}

#[test]
fn malformed_utf8_json_unterminated_and_multiline_frames_fail() {
    for bytes in [
        b"\xff\n".as_slice(),
        b"not-json\n",
        b"\n",
        b"{}\n{}\n",
        b"{\n}\n",
    ] {
        assert!(decode_frame::<Value>(bytes).is_err());
    }
    assert!(matches!(
        decode_frame::<Value>(b"{}"),
        Err(FrameError::Incomplete)
    ));
    let mut decoder = FrameDecoder::default();
    decoder.push(b"{").unwrap();
    assert!(matches!(decoder.finish(), Err(FrameError::Incomplete)));
}
