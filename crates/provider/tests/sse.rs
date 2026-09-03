use elefante_provider::sse::SseDecoder;

#[test]
fn sse_decoder_handles_split_crlf_and_multiline_data() {
    let mut decoder = SseDecoder::default();
    assert!(decoder.push(b"event: ping\r\nda").unwrap().is_empty());
    let frames = decoder
        .push(b"ta: {\"type\":\r\ndata: \"ping\"}\r\n\r\n")
        .unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event.as_deref(), Some("ping"));
    assert_eq!(frames[0].data, "{\"type\":\n\"ping\"}");
}

#[test]
fn sse_decoder_chooses_the_first_mixed_line_ending_boundary() {
    let mut decoder = SseDecoder::default();
    let frames = decoder
        .push(b"event: ping\r\ndata: first\r\n\r\nevent: ping\ndata: second\n\n")
        .unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].data, "first");
    assert_eq!(frames[1].data, "second");
}

#[test]
fn comment_keep_alives_are_not_frames() {
    let mut decoder = SseDecoder::default();
    assert!(
        decoder
            .push(b": OPENROUTER PROCESSING\n\n: OPENROUTER PROCESSING\n\n")
            .unwrap()
            .is_empty()
    );
    let frames = decoder
        .push(b": OPENROUTER PROCESSING\ndata: {\"ok\":true}\n\n")
        .unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].data, "{\"ok\":true}");
    assert!(decoder.finish().unwrap().is_none());
}
