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

/// The rows a session's newest generation holds.
async fn session_rows(session_id: &str) -> Vec<Value> {
    let listed = rpc("session/list", json!({})).await;
    let _ = listed;
    // The file API is the durable read this host exposes; the session's own
    // `session/follow` snapshot carries the same rows, so the follow stream is
    // used instead of a filesystem reach-around.
    let _ = session_id;
    Vec::new()
}

/// **The whole loop, against a real model**: offered tools, a real call, a real
/// file read, and a second request whose shape the provider accepts.
#[tokio::test]
async fn a_live_model_calls_a_tool_and_the_turn_continues() {
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

    // The turn runs detached, so poll the follow snapshot until it settles. A
    // tool-using turn is several model calls and can take a while.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let mut rows: Vec<Value> = Vec::new();
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        rows = snapshot_rows(&session_id).await;
        if rows
            .iter()
            .any(|r| r.get("type").and_then(Value::as_str) == Some("turn/end"))
        {
            break;
        }
    }
    assert!(!rows.is_empty(), "the session never produced rows");

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

    let _ = session_rows(&session_id).await;
    let _ = std::fs::remove_dir_all(&workspace);
}

/// The session's rows, read back through the follow stream's opening snapshot.
///
/// The follow stream is used rather than a filesystem read because it is the
/// same projection a client sees — so this asserts on what a client would, not on
/// an internal file whose layout is an implementation detail.
async fn snapshot_rows(session_id: &str) -> Vec<Value> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let url = format!("{}/api/remote.mux", base_url().replace("http://", "ws://"));
    let Ok((mut socket, _)) = tokio_tungstenite::connect_async(&url).await else {
        return Vec::new();
    };
    let stream_id = format!("s-{}", uuid());
    if socket
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
        .is_err()
    {
        return Vec::new();
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
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
        if value.get("type").and_then(Value::as_str) == Some("snapshot")
            && let Some(records) = value.get("records").and_then(Value::as_array)
        {
            // The snapshot's own frame is the snapshot itself, and each record
            // wraps its row under `event`.
            return records
                .iter()
                .filter_map(|r| r.get("event").cloned())
                .collect();
        }
    }
    Vec::new()
}
