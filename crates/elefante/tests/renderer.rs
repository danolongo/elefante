use elefante::client::StreamRenderer;
use elefante::wire::Notification;
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
    assert_eq!(idle.into_inner().len(), 0);
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

#[test]
fn a_tool_with_no_output_preview_ends_without_a_dangling_colon() {
    let mut renderer = StreamRenderer::new(Vec::new(), false);
    renderer
        .render(&note(
            "tool.finished",
            json!({"tool_call_id":"c1","is_error":false,"output_preview":"","duration_ms":2}),
        ))
        .unwrap();
    assert_eq!(renderer.into_inner(), b"[c1] done (2 ms)\n");
}

fn learned(status: &str, change: &str) -> Notification {
    note(
        "memory.learned",
        json!({"note":{"id":1,"kind":"convention","status":status,"statement":"Use pnpm.","origin":"explicit"},"change":change}),
    )
}

fn status(problem: Option<&str>) -> Notification {
    note(
        "memory.status",
        json!({"enabled":true,"consent":"granted","learning":"failing","extractor":null,"notes":{"active":0,"pending":0},"index":"ready","spend":{"input_tokens":0,"output_tokens":0,"reasoning_tokens":0,"cached_input_tokens":0,"cache_write_tokens":0,"cost":null},"problem":problem}),
    )
}

fn finished() -> Notification {
    note(
        "turn.finished",
        json!({"stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":0,"reasoning_tokens":0,"cached_input_tokens":0,"cache_write_tokens":0,"cost":null}}),
    )
}

fn memory_output(notices: bool, events: &[Notification]) -> String {
    let mut renderer = StreamRenderer::new(Vec::new(), false).with_memory_notices(notices);
    for event in events {
        renderer.render(event).unwrap();
    }
    String::from_utf8(renderer.into_inner()).unwrap()
}

#[test]
fn saved_notes_are_counted_once_per_turn_with_plurals_and_no_note_text() {
    let one = memory_output(true, &[learned("active", "added"), finished()]);
    assert_eq!(one, "memory: saved 1 note\n");
    let two = memory_output(
        true,
        &[
            learned("active", "added"),
            learned("active", "superseded"),
            finished(),
        ],
    );
    assert_eq!(two, "memory: saved 2 notes\n");
    assert!(!two.contains("pnpm"));
}

#[test]
fn pending_notes_are_not_counted_and_turns_without_saves_print_nothing() {
    assert_eq!(
        memory_output(true, &[learned("pending", "added"), finished()]),
        ""
    );
    assert_eq!(
        memory_output(
            true,
            &[
                learned("pending", "added"),
                learned("active", "added"),
                finished()
            ]
        ),
        "memory: saved 1 note\n"
    );
    assert_eq!(memory_output(true, &[finished()]), "");
}

#[test]
fn the_saved_count_resets_after_each_turn() {
    let out = memory_output(
        true,
        &[
            learned("active", "added"),
            finished(),
            learned("active", "added"),
            learned("active", "added"),
            finished(),
        ],
    );
    assert_eq!(out, "memory: saved 1 note\nmemory: saved 2 notes\n");
}

#[test]
fn a_repeated_problem_prints_once_and_recovery_prints_nothing() {
    let out = memory_output(
        true,
        &[
            status(Some("extractor unreachable")),
            status(Some("extractor unreachable")),
            status(None),
            status(Some("index is rebuilding")),
        ],
    );
    assert_eq!(
        out,
        "memory: extractor unreachable\nmemory: index is rebuilding\n"
    );
}

#[test]
fn memory_lines_are_silent_unless_enabled() {
    let events = [
        learned("active", "added"),
        status(Some("extractor unreachable")),
        finished(),
    ];
    assert_eq!(memory_output(false, &events), "");
}

#[test]
fn memory_events_leave_the_answer_stream_untouched() {
    let mut answer = StreamRenderer::new(Vec::new(), false);
    let mut diagnostics = StreamRenderer::new(Vec::new(), false).with_memory_notices(true);
    for event in [
        note("text.delta", json!({"text":"answer"})),
        learned("active", "added"),
        status(Some("extractor unreachable")),
        finished(),
    ] {
        if matches!(
            event.event,
            elefante::wire::NotificationEvent::TextDelta { .. }
        ) {
            answer.render(&event).unwrap();
        } else {
            diagnostics.render(&event).unwrap();
        }
    }
    answer.finish().unwrap();
    assert_eq!(answer.into_inner(), b"answer\n");
    let diagnostics = String::from_utf8(diagnostics.into_inner()).unwrap();
    assert_eq!(
        diagnostics,
        "memory: extractor unreachable\nmemory: saved 1 note\n"
    );
}
