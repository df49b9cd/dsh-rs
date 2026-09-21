//! Live probe: does a real model's tool call actually run a real tool?
//!
//! Not part of the wire suite — it needs a running vocoderd AND a live provider,
//! which is exactly the combination the committed cells deliberately avoid.
//!
//! It exists because the tool seam's central claim is a *request shape*: the
//! host sends a `tools` array the provider accepts, the model answers with a
//! tool call, the executor runs it against a real filesystem, and the next
//! request carries the result in the form that dialect requires. Every one of
//! those is a claim about bytes crossing a real boundary, and the canned fixtures
//! assert only the two ends the host controls.
//!
//! **The failure this is built to catch.** `canonical_request` originally folded
//! `assistant/message` and `user/message` but not `tool/result`, so the second
//! request carried a call with nothing answering it. The turn's rows were still
//! correct and the unit test still passed; only a rebuilt *request* showed it.
//! A provider's own 400 is the strongest possible confirmation that the item is
//! now right.
//!
//! Run:
//!   VOCODER_BASE_URL=http://127.0.0.1:3199 \
//!   VOCODER_LIVE_API_KEY=... \
//!   cargo test --test live_tools -- --nocapture

use serde_json::{Value, json};

fn base_url() -> String {
    std::env::var("VOCODER_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:3080".into())
}

fn uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    static C: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{:x}-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        C.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

async fn rpc(method: &str, args: Value) -> Value {
    let body = json!({
        "type": "client-request",
        "rpcId": format!("probe-{}", uuid()),
        "method": method,
        "payload": { "args": args },
    });
    let res = reqwest::Client::new()
        .post(format!("{}/api/{}", base_url(), method))
        .json(&body)
        .send()
        .await
        .unwrap();
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["type"], "server-response", "bad envelope: {v}");
    v["result"].clone()
}

/// **The whole loop, against a real model**: offered tools, a real call, a real
/// file read, and a second request whose shape the provider accepts.
#[tokio::test]
async fn a_live_model_calls_a_tool_and_the_turn_continues() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    if std::env::var("VOCODER_LIVE_API_KEY").is_err() {
        eprintln!("SKIPPED: set VOCODER_LIVE_API_KEY to run the live tool probe");
        return;
    }
    // A workspace with one file, so the model has something real to read. The
    // path is under /tmp, which `workspace-write` grants.
    let workspace = std::path::PathBuf::from(format!("/tmp/vocoder-live-tools-{}", uuid()));
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("secret.txt"), "TANGERINE\n").expect("seed file");

    let created = rpc(
        "session/create",
        json!({ "request": { "cwd": workspace.to_string_lossy() } }),
    )
    .await;
    assert!(created["ok"].as_bool().unwrap(), "create: {created}");
    let session_id = created["value"]["sessionId"].as_str().unwrap().to_string();

    // One follow stream, held for the whole turn. Polling `session/follow`
    // once per snapshot would lose every event between polls — a previous
    // shape of this probe asserted against exactly that — so the rows this
    // test reads are the *live* items, just as a real client sees them.
    let url = format!("{}/api/remote.mux", base_url().replace("http://", "ws://"));
    let (mut socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect /api/remote.mux");
    let stream_id = format!("s-{}", uuid());
    socket
        .send(Message::Text(
            json!({
                "type": "open",
                "streamId": stream_id,
                "endpoint": "session/follow",
                "payload": { "args": { "request": {
                    "address": { "kind": "session", "sessionId": session_id },
                } } },
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    // Wait for the opening snapshot before prompting, so the prompt's own
    // event cannot race the open.
    {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let Ok(Some(Ok(msg))) = tokio::time::timeout_at(deadline, socket.next()).await else {
                panic!("no opening snapshot");
            };
            let Message::Text(t) = msg else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&t) else {
                continue;
            };
            if v["streamId"] == stream_id
                && v["value"].get("type").and_then(Value::as_str) == Some("snapshot")
            {
                break;
            }
        }
    }

    let prompted = rpc(
        "session/prompt",
        json!({ "request": {
            "sessionId": session_id,
            "requestId": format!("req-{}", uuid()),
            "mode": "queue",
            "content": [{ "type": "text", "text":
                "Read the file secret.txt in your workspace using the read tool, then reply with \
                 exactly the single word it contains and nothing else." }],
        } }),
    )
    .await;
    assert!(prompted["ok"].as_bool().unwrap(), "prompt: {prompted}");

    // Read the stream until the turn's durable end arrives. A tool-using turn
    // is several model calls and can take a while.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);
    let mut rows: Vec<Value> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(msg))) = tokio::time::timeout_at(deadline, socket.next()).await else {
            break;
        };
        let Message::Text(t) = msg else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&t) else {
            continue;
        };
        if v["streamId"] != stream_id {
            continue;
        }
        let value = &v["value"];
        if value.get("type").and_then(Value::as_str) != Some("event") {
            continue;
        }
        let Some(event) = value.get("event").cloned() else {
            continue;
        };
        let done =
            event.get("type").and_then(Value::as_str) == Some("turn/end");
        rows.push(event);
        if done {
            break;
        }
    }
    assert!(
        rows.iter()
            .any(|r| r.get("type").and_then(Value::as_str) == Some("turn/end")),
        "the turn settled: {}",
        rows.iter()
            .filter_map(|r| r.get("type").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let types: Vec<&str> = rows
        .iter()
        .filter_map(|r| r.get("type").and_then(Value::as_str))
        .collect();
    println!("ROW TYPES: {types:?}");
    for row in &rows {
        if matches!(
            row.get("type").and_then(Value::as_str),
            Some("tool/call") | Some("tool/result")
        ) {
            println!("ROW {}", serde_json::to_string(row).unwrap());
        }
    }

    assert!(
        types.contains(&"tool/call"),
        "the model called a tool: {types:?}"
    );
    assert!(
        types.contains(&"tool/result"),
        "the call was answered: {types:?}"
    );

    // The result is the real file's content, in the upstream envelope. This is
    // the assertion that proves the executor ran against the filesystem rather
    // than describing it.
    let result = rows
        .iter()
        .find(|r| r.get("type").and_then(Value::as_str) == Some("tool/result"))
        .expect("a tool result");
    let text = result
        .pointer("/data/message/content/0/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    println!("TOOL RESULT:\n{text}");
    assert!(
        text.contains("TANGERINE"),
        "the file's real content reached the model: {text:?}"
    );
    assert_eq!(
        result.pointer("/data/message/content/0/isError"),
        Some(&json!(false)),
        "the call succeeded: {result}"
    );

    // **The second request was accepted.** A `tool/result` that never reached
    // `canonical_request` would leave the call unanswered, and the provider would
    // reject the follow-up — so a completed turn whose final message is the
    // model's answer is itself the evidence, and the text names it.
    assert_eq!(
        rows.last().and_then(|r| r.pointer("/data/reason/kind")),
        Some(&json!("completed")),
        "the turn completed, so the follow-up request was accepted: {types:?}"
    );
    let final_text: String = rows
        .iter()
        .filter(|r| r.get("type").and_then(Value::as_str) == Some("assistant/message"))
        .filter_map(|r| r.pointer("/data/message/content"))
        .filter_map(Value::as_array)
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    println!("FINAL TEXT: {final_text:?}");
    assert!(
        final_text.contains("TANGERINE"),
        "the model answered from the file it read: {final_text:?}"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}
