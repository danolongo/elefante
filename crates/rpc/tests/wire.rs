use elefante_rpc::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::fmt::Debug;

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(wire: Value) -> T {
    let expected = wire.clone();
    let parsed: T = serde_json::from_value(wire).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), expected);
    assert_eq!(
        decode_frame::<T>(&encode_frame(&parsed).unwrap()).unwrap(),
        parsed
    );
    parsed
}

#[test]
fn every_request_method_round_trips_with_fixed_members() {
    assert!(
        serde_json::from_value::<Request>(
            json!({"method":"session.close","params":{"session_id":"s1"}})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<Request>(
            json!({"id":true,"method":"session.close","params":{"session_id":"s1"}})
        )
        .is_err()
    );
    for (method, params) in [
        ("session.create", json!({"cwd":"/tmp"})),
        (
            "session.create",
            json!({"cwd":"/tmp", "model":"m", "effort":"high", "max_rounds":3, "max_tokens":128}),
        ),
        (
            "session.prompt",
            json!({"session_id":"s1", "text":"hello\nworld"}),
        ),
        ("session.interrupt", json!({"session_id":"s1"})),
        ("session.attach", json!({"session_id":"s1"})),
        ("session.attach", json!({"session_id":"s1", "since":412})),
        ("session.close", json!({"session_id":"s1"})),
        (
            "session.resolve_approval",
            json!({"session_id":"s1", "approval_id":"a", "decision":"allow"}),
        ),
        (
            "session.resolve_approval",
            json!({"session_id":"s1", "approval_id":"a", "decision":"deny"}),
        ),
        ("key.set", json!({"api_key":"secret"})),
        ("memory.consent", json!({"answer":"learn"})),
        ("memory.consent", json!({"answer":"off"})),
        ("memory.list", json!({"cwd":"/tmp"})),
        ("memory.list", json!({"cwd":"/tmp","all":true})),
        ("memory.forget", json!({"cwd":"/tmp","id":14})),
        ("memory.approve", json!({"cwd":"/tmp","id":14})),
        ("memory.clear", json!({"cwd":"/tmp"})),
        ("memory.status", json!({"cwd":"/tmp"})),
    ] {
        round_trip::<Request>(json!({"id":17,"method":method,"params":params}));
    }
    assert!(
        serde_json::from_value::<Request>(json!({"id":1,"method":"session.list","params":{}}))
            .is_err()
    );
    assert!(
        serde_json::from_value::<Request>(
            json!({"id":1,"method":"session.prompt","params":{"session_id":"s"}})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<ApprovalDecision>(json!("cancelled")).is_err());
    assert!(
        !format!(
            "{:?}",
            KeySetParams {
                api_key: "secret".into()
            }
        )
        .contains("secret")
    );
}

fn usage() -> Value {
    json!({"input_tokens":1,"output_tokens":2,"reasoning_tokens":3,"cached_input_tokens":4,"cache_write_tokens":5,"cost":null})
}

fn memory_status() -> Value {
    json!({
        "enabled":true,
        "consent":"granted",
        "learning":"working",
        "extractor":"provider/model",
        "notes":{"active":12,"pending":1},
        "index":"ready",
        "spend":usage(),
        "problem":null
    })
}

fn memory_note(id: i64) -> Value {
    json!({
        "id":id,
        "kind":"convention",
        "status":"active",
        "statement":"Use pnpm.",
        "origin":"explicit"
    })
}

fn note(seq: u64, method: &str, params: Value) -> Value {
    let mut value = json!({"session_id":"s1","seq":seq,"turn_id":"t7","method":method});
    value["params"] = params;
    value
}

#[test]
fn every_notification_has_one_fixed_envelope() {
    for (method, params) in [
        ("turn.started", json!({})),
        ("reasoning.delta", json!({"text":"think"})),
        ("text.delta", json!({"text":"answer"})),
        (
            "tool.started",
            json!({"tool_call_id":"c1","name":"read","input_preview":"{}"}),
        ),
        (
            "tool.finished",
            json!({"tool_call_id":"c1","is_error":false,"output_preview":"ok","duration_ms":42}),
        ),
        (
            "approval.requested",
            json!({"approval_id":"a1","command":"echo ok","rule":"shell"}),
        ),
        (
            "approval.resolved",
            json!({"approval_id":"a1","approved":false}),
        ),
        (
            "turn.finished",
            json!({"stop_reason":"end_turn","usage":usage()}),
        ),
        ("session.error", json!({"message":"provider failed"})),
        ("memory.status", memory_status()),
        (
            "memory.learned",
            json!({"note":memory_note(14),"change":"added"}),
        ),
        (
            "memory.learned",
            json!({"note":memory_note(15),"change":"superseded","replaced":memory_note(14)}),
        ),
        ("memory.context", json!({"notes":[memory_note(14)]})),
    ] {
        round_trip::<Notification>(note(413, method, params));
    }
    let mut outside_turn = note(
        1,
        "approval.requested",
        json!({"approval_id":"a","command":"x","rule":"r"}),
    );
    outside_turn["turn_id"] = Value::Null;
    round_trip::<Notification>(outside_turn);
}

#[test]
fn unknown_notification_methods_deserialize_with_params_but_never_serialize() {
    let wire = note(
        414,
        "memory.future_event",
        json!({"nested":{"payload":true}}),
    );
    let parsed: Notification = serde_json::from_value(wire).unwrap();
    assert_eq!(
        parsed.event,
        NotificationEvent::Unknown {
            method: "memory.future_event".into()
        }
    );
    assert!(serde_json::to_value(parsed).is_err());
    assert!(
        serde_json::from_value::<Notification>(
            json!({"session_id":"s1","seq":414,"turn_id":"t7","method":"memory.future_event"})
        )
        .is_err()
    );
}

#[test]
fn stop_reasons_and_usage_preserve_other_objects_and_nullable_cost() {
    for reason in [
        json!("end_turn"),
        json!("tool_use"),
        json!("max_tokens"),
        json!("refusal"),
        json!("stop_sequence"),
        json!("interrupted"),
        json!({"other":"provider-specific"}),
    ] {
        round_trip::<StopReason>(reason);
    }
    assert!(serde_json::from_value::<StopReason>(json!("other")).is_err());
    let unknown_cost = round_trip::<Usage>(usage());
    let mut zero_cost = usage();
    zero_cost["cost"] = json!(0.0);
    assert_ne!(unknown_cost, round_trip::<Usage>(zero_cost));
    for field in [
        "input_tokens",
        "output_tokens",
        "reasoning_tokens",
        "cached_input_tokens",
        "cache_write_tokens",
    ] {
        let mut missing = usage();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<Usage>(missing).is_err());
    }
}

#[test]
fn method_results_and_all_application_errors_round_trip() {
    assert!(serde_json::from_value::<Response>(json!({"result":{}})).is_err());
    assert!(serde_json::from_value::<Response>(json!({"id":1})).is_err());
    round_trip::<CreateResult>(json!({"session_id":"s1","seq":0}));
    round_trip::<CreateResult>(json!({
        "session_id":"s1", "seq":0,
        "memory":{"consent":"unset","extractor":"provider/model"}
    }));
    round_trip::<MemoryConsentResult>(json!({"consent":"granted"}));
    round_trip::<MemoryStatus>(memory_status());
    round_trip::<MemoryListResult>(json!({"notes":[{
        "id":14,"kind":"convention","status":"active","statement":"Use pnpm.",
        "origin":"explicit","confidence":0.9,"verified":true,"reinforcements":2
    }]}));
    round_trip::<MemoryClearResult>(json!({"removed":3}));
    round_trip::<PromptResult>(json!({"turn_id":"t1"}));
    round_trip::<EmptyResult>(json!({}));
    round_trip::<Response>(json!({"id":"r1","result":{"session_id":"s1","seq":0}}));
    round_trip::<Response>(json!({"id":1,"result":{}}));
    for code in [
        UNKNOWN_SESSION,
        TURN_IN_PROGRESS,
        INVALID_SINCE,
        NO_API_KEY,
        UNKNOWN_APPROVAL,
        UNKNOWN_NOTE,
        MEMORY_OFF,
    ] {
        round_trip::<Response>(json!({"id":1,"error":{"code":code,"message":"message"}}));
    }
    round_trip::<Response>(
        json!({"id":null,"error":{"code":PARSE_ERROR,"message":"invalid","data":{"detail":"here"}}}),
    );
    assert!(
        serde_json::from_value::<Response>(
            json!({"id":1,"result":{},"error":{"code":1,"message":"x"}})
        )
        .is_err()
    );
    assert_eq!(
        Response::success(json!(1), EmptyResult {}).unwrap().payload,
        ResponsePayload::Result(json!({}))
    );
    assert_eq!(
        serde_json::to_value(Response::error(json!(1), UNKNOWN_SESSION, "gone")).unwrap(),
        json!({"id":1,"error":{"code":-32001,"message":"gone"}})
    );
}

#[test]
fn attach_uses_full_envelopes_and_one_tool_shape_everywhere() {
    round_trip::<AttachResult>(
        json!({"kind":"resumed","from":413,"events":[note(413,"text.delta",json!({"text":"continued"}))]}),
    );
    round_trip::<AttachResult>(
        json!({"kind":"resumed","from":413,"events":[],"memory":{"consent":"declined","extractor":"provider/model"}}),
    );
    let tools = json!([
        {"tool_call_id":"c1","name":"read","state":"running","is_error":false},
        {"tool_call_id":"c2","name":"grep","state":"ok","is_error":false},
        {"tool_call_id":"c3","name":"bash","state":"error","is_error":true},
        {"tool_call_id":"c4","name":"write","state":"cancelled","is_error":true}
    ]);
    let mut snapshot = json!({"session_id":"s1","seq":981,"cwd":"/tmp","model":"m","turns":[
        {"turn_id":"t1","prompt":"hi","text":"hello","tools":tools,"stop_reason":"end_turn","usage":usage()}
    ],"active_turn":{
        "turn_id":"t7","text_so_far":"answer","reasoning_so_far":"think","tools":tools,
        "pending_approval":{"approval_id":"a1","command":"echo x","rule":"shell"}
    }});
    round_trip::<AttachResult>(json!({"kind":"snapshot","seq":981,"snapshot":snapshot}));
    round_trip::<AttachResult>(json!({
        "kind":"snapshot", "seq":981, "snapshot":snapshot,
        "memory":{"consent":"off","extractor":null}
    }));
    snapshot["active_turn"]["pending_approval"] = Value::Null;
    round_trip::<SessionSnapshot>(snapshot.clone());
    snapshot["active_turn"] = Value::Null;
    round_trip::<SessionSnapshot>(snapshot);
}

#[test]
fn notification_rejects_missing_routing_ambiguous_ranges_and_discrete_coalescing() {
    let valid = note(1, "text.delta", json!({"text":"x"}));
    for field in ["session_id", "turn_id", "seq"] {
        let mut invalid = valid.clone();
        invalid.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<Notification>(invalid).is_err(),
            "missing {field}"
        );
    }
    for extra in [
        json!({"id":1}),
        json!({"seq":0}),
        json!({"seq_from":1,"seq_to":2}),
    ] {
        let mut invalid = valid.clone();
        invalid
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(serde_json::from_value::<Notification>(invalid).is_err());
    }
    for fields in [
        json!({"seq_from":2,"seq_to":1}),
        json!({"seq_from":0,"seq_to":1}),
        json!({"seq_from":1}),
        json!({"seq_to":1}),
    ] {
        assert!(serde_json::from_value::<Sequence>(fields).is_err());
    }
    let mut range = valid;
    range.as_object_mut().unwrap().remove("seq");
    range["seq_from"] = json!(1);
    range["seq_to"] = json!(10);
    round_trip::<Notification>(range.clone());
    range["method"] = json!("turn.started");
    range["params"] = json!({});
    assert!(serde_json::from_value::<Notification>(range).is_err());

    let mut unknown_range = note(1, "future.event", json!({"value":1}));
    unknown_range.as_object_mut().unwrap().remove("seq");
    unknown_range["seq_from"] = json!(1);
    unknown_range["seq_to"] = json!(2);
    assert!(serde_json::from_value::<Notification>(unknown_range).is_err());
}

fn delta(seq: u64, reasoning: bool, text: &str) -> Notification {
    Notification::new(
        "s1",
        seq,
        Some("t1".into()),
        if reasoning {
            NotificationEvent::ReasoningDelta { text: text.into() }
        } else {
            NotificationEvent::TextDelta { text: text.into() }
        },
    )
}

#[test]
fn coalescing_is_lossless_and_preserves_channel_session_turn_and_adjacency() {
    let mut reasoning = delta(1, true, "think ");
    assert!(reasoning.coalesce(&delta(2, true, "more")));
    assert!(!reasoning.coalesce(&delta(3, false, "answer ")));
    let mut answer = delta(3, false, "answer ");
    assert!(answer.coalesce(&delta(4, false, "here")));
    assert_eq!(
        reasoning.event,
        NotificationEvent::ReasoningDelta {
            text: "think more".into()
        }
    );
    assert_eq!(
        answer.event,
        NotificationEvent::TextDelta {
            text: "answer here".into()
        }
    );
    assert_eq!(
        answer.sequence,
        Sequence::Coalesced {
            seq_from: 3,
            seq_to: 4
        }
    );
    assert!(!answer.coalesce(&delta(6, false, "gap")));
    let mut wrong_session = delta(5, false, "s");
    wrong_session.session_id = "s2".into();
    assert!(!answer.coalesce(&wrong_session));
    let mut wrong_turn = delta(5, false, "t");
    wrong_turn.turn_id = Some("t2".into());
    assert!(!answer.coalesce(&wrong_turn));
    assert!(!answer.coalesce(&Notification::new(
        "s1",
        5,
        Some("t1".into()),
        NotificationEvent::TurnStarted {}
    )));
    round_trip::<Notification>(serde_json::to_value(answer).unwrap());
}

#[test]
fn previews_are_byte_capped_including_exact_marker_without_splitting_utf8() {
    for value in [
        "x".repeat(PREVIEW_BYTES + 1),
        "🦀".repeat(PREVIEW_BYTES),
        "é".repeat(PREVIEW_BYTES),
    ] {
        let preview = truncate_preview(&value);
        assert!(preview.len() <= PREVIEW_BYTES);
        assert!(preview.ends_with(TRUNCATION_MARKER));
        assert!(value.starts_with(preview.strip_suffix(TRUNCATION_MARKER).unwrap()));
    }
    for value in [String::new(), "hello".into(), "x".repeat(PREVIEW_BYTES)] {
        assert_eq!(truncate_preview(&value), value);
    }
    assert_eq!(TRUNCATION_MARKER, "\n…");
}

#[test]
fn coalescing_refuses_a_frame_overflow_without_mutation() {
    let mut current = delta(1, false, "small");
    let original = current.clone();
    let escaped_text = "\u{0000}".repeat(MAX_FRAME_BYTES / 6);
    assert!(!current.coalesce(&delta(2, false, &escaped_text)));
    assert_eq!(current, original);
}
