//! Phase 0 S9: maintainer-only, billable wire-format probe; no product backend changes.
//! With `OPENROUTER_API_KEY` already exported by an external secret manager:
//! `cargo test -p elefante-provider --test s9_web_plugin -- --ignored --nocapture`
//! Optionally export `ELEFANTE_SEARCH_MODEL` to compare candidates (one call per run).
//! Never loads credential files. Raw responses, headers and transport errors are not logged.

use std::{collections::BTreeSet, time::Duration};

use futures_util::StreamExt;
use serde_json::{Value, json};

#[tokio::test]
#[ignore = "maintainer only: credentials, network, and one billable Exa + model request"]
async fn live_s9_exa_web_plugin() {
    let key = std::env::var("OPENROUTER_API_KEY").expect("export OPENROUTER_API_KEY externally");
    assert!(!key.trim().is_empty(), "OPENROUTER_API_KEY is empty");
    let model =
        std::env::var("ELEFANTE_SEARCH_MODEL").unwrap_or_else(|_| "openai/gpt-4.1-mini".to_owned());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| panic!("could not build HTTP client"));
    let response = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(&key)
        .header("Accept", "text/event-stream")
        .json(&json!({
            "model": model,
            "stream": true,
            "max_tokens": 256,
            "usage": {"include": true},
            "plugins": [{"id": "web", "engine": "exa", "max_results": 3}],
            "messages": [{"role": "user", "content":
                "Find the official Rust release announcement for Rust 1.90.0. Give its release date in one sentence and cite the source with a plain Markdown link."}]
        }))
        .send()
        .await
        .unwrap_or_else(|_| panic!("request failed; transport details suppressed"));
    assert!(
        response.status().is_success(),
        "HTTP status {} (body suppressed)",
        response.status().as_u16()
    );

    let mut wire = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap_or_else(|_| panic!("stream read failed; details suppressed"));
        assert!(
            wire.len() + chunk.len() <= 2 * 1024 * 1024,
            "S9 response exceeds 2 MiB cap"
        );
        wire.extend_from_slice(&chunk);
    }
    let wire = String::from_utf8(wire).expect("SSE response must be UTF-8");
    let mut shapes = BTreeSet::new();
    let mut citations = BTreeSet::new();
    let mut answer = String::new();
    let mut usage = Value::Null;
    let mut events = 0;
    let mut done = false;
    for line in wire.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            done = true;
            continue;
        }
        let event: Value = serde_json::from_str(data)
            .unwrap_or_else(|_| panic!("unrecognized SSE data; body suppressed"));
        assert!(
            event.get("error").is_none(),
            "upstream stream error; body suppressed"
        );
        events += 1;
        shapes.insert(shape(&event, 0).to_string());
        collect_citations(&event, &mut citations);
        if let Some(choices) = event["choices"].as_array() {
            for choice in choices {
                if let Some(text) = choice["delta"]["content"].as_str() {
                    answer.push_str(text);
                }
            }
        }
        if event["usage"].is_object() {
            usage = event["usage"].clone();
        }
    }
    let report = json!({
        "model": model, "engine": "exa", "max_results": 3,
        "events": events, "done": done, "event_shapes": shapes,
        "annotation_urls": citations, "plain_text_answer": answer,
        "usage": numeric_usage(&usage),
        "cost_note": "usage.cost is reported verbatim as a number; confirm search surcharge inclusion in OpenRouter Activity before choosing a budget/default"
    }).to_string();
    print_checked(&report, &key);
    assert!(events > 0 && done, "missing SSE events or DONE marker");
    assert!(!answer.is_empty(), "no plain-text answer");
    assert!(
        answer.contains("https://"),
        "no plain-text citation URL (inspect annotations separately)"
    );
    assert!(
        !citations.is_empty(),
        "no URL citation annotations; stream contract needs investigation"
    );
    assert!(
        usage.get("cost").and_then(Value::as_f64).is_some(),
        "no numeric usage.cost; inspect Activity for billing"
    );
}

fn numeric_usage(usage: &Value) -> Value {
    json!({
        "prompt_tokens": usage.get("prompt_tokens").and_then(Value::as_u64),
        "completion_tokens": usage.get("completion_tokens").and_then(Value::as_u64),
        "cost": usage.get("cost").and_then(Value::as_f64),
        "web_search_cost": usage.pointer("/cost_details/upstream_inference_web_search_cost").and_then(Value::as_f64)
    })
}

fn print_checked(report: &str, key: &str) {
    // Check before printing: even an upstream echo cannot put the real key in logs.
    assert!(
        !report.contains(key),
        "credential detected in report; output suppressed"
    );
    assert!(
        !report.contains("sk-or-v1-"),
        "credential-shaped report; output suppressed"
    );
    println!("{report}");
}

// Preserve field names/types, never upstream string values, to expose annotation placement.
fn shape(value: &Value, depth: usize) -> Value {
    if depth >= 8 {
        return json!("depth-limit");
    }
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), shape(value, depth + 1)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .first()
                .map(|v| shape(v, depth + 1))
                .into_iter()
                .collect(),
        ),
        Value::String(_) => json!("string"),
        Value::Number(_) => json!("number"),
        Value::Bool(_) => json!("boolean"),
        Value::Null => Value::Null,
    }
}

fn collect_citations(value: &Value, urls: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => {
            if let Some(url) = value.pointer("/url_citation/url").and_then(Value::as_str)
                && let Ok(mut parsed) = reqwest::Url::parse(url)
                && matches!(parsed.scheme(), "https" | "http")
                && parsed.host_str().is_some()
                && parsed.username().is_empty()
                && parsed.password().is_none()
            {
                parsed.set_query(None);
                parsed.set_fragment(None);
                urls.insert(parsed.to_string());
            }
            for child in fields.values() {
                collect_citations(child, urls);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_citations(child, urls);
            }
        }
        _ => {}
    }
}
