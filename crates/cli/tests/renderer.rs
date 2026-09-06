use elefante_cli::StreamRenderer;
use elefante_rpc::Notification;
use serde_json::{Value, json};

fn note(method: &str, params: Value) -> Notification {
    let mut envelope = json!({"session_id":"s1","seq":1,"turn_id":"t1","method":method});
    envelope["params"] = params;
    serde_json::from_value(envelope).unwrap()
}

#[test]
fn transitions_have_one_newline_and_same_channel_deltas_concatenate() {
    let mut renderer = StreamRenderer::new(Vec::new(), false);
    for _ in 0..10 {
        renderer
            .render(&note("reasoning.delta", json!({"text":"a"})))
            .unwrap();
    }
    renderer
        .render(&note("text.delta", json!({"text":"answer"})))
        .unwrap();
    assert_eq!(renderer.into_inner(), b"aaaaaaaaaa\nanswer");
}

#[test]
fn transitions_after_a_newline_do_not_add_blank_lines() {
    let mut renderer = StreamRenderer::new(Vec::new(), false);
    renderer
        .render(&note("reasoning.delta", json!({"text":"thought\n"})))
        .unwrap();
    renderer
        .render(&note("text.delta", json!({"text":"answer\n"})))
        .unwrap();
    renderer.finish().unwrap();
    assert_eq!(renderer.into_inner(), b"thought\nanswer\n");
}

#[test]
fn unstyled_output_never_introduces_ansi() {
    let mut renderer = StreamRenderer::new(Vec::new(), false);
    renderer
        .render(&note("reasoning.delta", json!({"text":"thought"})))
        .unwrap();
    renderer
        .render(&note("session.error", json!({"message":"failure"})))
        .unwrap();
    assert_eq!(renderer.into_inner(), b"thought\nfailure\n");
}

#[test]
fn each_faint_span_is_reset_before_an_error() {
    let mut renderer = StreamRenderer::new(Vec::new(), true);
    renderer
        .render(&note("reasoning.delta", json!({"text":"thought"})))
        .unwrap();
    renderer
        .render(&note("reasoning.delta", json!({"text":"more"})))
        .unwrap();
    renderer
        .render(&note("session.error", json!({"message":"failure"})))
        .unwrap();
    assert_eq!(
        renderer.into_inner(),
        b"\x1b[2mthought\x1b[0m\x1b[2mmore\x1b[0m\nfailure\n"
    );
}

#[test]
fn finish_is_idempotent_and_does_not_create_output_on_idle_stream() {
    let mut idle = StreamRenderer::new(Vec::new(), false);
    idle.finish().unwrap();
    assert!(idle.into_inner().is_empty());
    let mut renderer = StreamRenderer::new(Vec::new(), false);
    renderer
        .render(&note("text.delta", json!({"text":"answer"})))
        .unwrap();
    renderer.finish().unwrap();
    renderer.finish().unwrap();
    assert_eq!(renderer.into_inner(), b"answer\n");
}

#[test]
fn tool_lifecycle_is_visible_without_breaking_reasoning_deltas() {
    let mut renderer = StreamRenderer::new(Vec::new(), false);
    renderer
        .render(&note("reasoning.delta", json!({"text":"thought"})))
        .unwrap();
    renderer
        .render(&note(
            "tool.started",
            json!({"tool_call_id":"c1","name":"read","input_preview":"note.txt"}),
        ))
        .unwrap();
    renderer.render(&note("tool.finished", json!({"tool_call_id":"c1","is_error":false,"output_preview":"contents","duration_ms":2}))).unwrap();
    assert_eq!(
        renderer.into_inner(),
        b"thought\n[read] note.txt\n[c1] done (2 ms): contents\n"
    );
}
