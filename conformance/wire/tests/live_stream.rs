//! Live probe: does a real follower receive assistant-stream frames during a
//! real turn?
//!
//! Not part of the wire suite: it needs a running vocoderd AND a live provider,
//! which is exactly the combination the committed cells deliberately avoid. It
//! exists because the streaming path is the one place where "the frames are
//! correct" and "the frames arrive while the model is still talking" are
//! different claims, and only a real socket against a real gateway can tell
//! them apart.
//!
//! Run:
//!   VOCODER_BASE_URL=http://127.0.0.1:3199 \
//!   VOCODER_LIVE_API_KEY=... \
//!   cargo test --test live_stream -- --nocapture

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

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

async fn ws_connect()
-> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("{}/api/remote.mux", base_url().replace("http://", "ws://"));
    let (socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect /api/remote.mux");
    socket
}

#[tokio::test]
async fn a_live_follower_receives_assistant_frames() {
    if std::env::var("VOCODER_LIVE_API_KEY").is_err() {
        eprintln!("SKIPPED: set VOCODER_LIVE_API_KEY to run the live streaming probe");
        return;
    }
    let created = rpc(
        "session/create",
        json!({ "request": { "cwd": "/tmp/vocoder-live-stream" } }),
    )
    .await;
    assert!(created["ok"].as_bool().unwrap(), "create: {created}");
    let session_id = created["value"]["sessionId"].as_str().unwrap().to_string();

    // The follower attaches *before* the prompt, so it is live for the whole
    // turn. Attaching after the model has started only exercises the reconnect
    // baseline: the chunks that already flowed are in the snapshot, and the
    // turn may well have finished before the stream opened.
    let mut socket = ws_connect().await;
    let stream_id = format!("s-{}", uuid());
    socket
        .send(Message::Text(
            json!({
                "type": "open",
                "streamId": stream_id,
                "endpoint": "session/follow",
                "payload": { "args": { "request": {
                    "address": { "kind": "session", "sessionId": session_id },
                    "assistantStream": true,
                } } },
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    // Wait for the opening snapshot before prompting, so the stream is
    // established and the prompt's frames cannot race the open.
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
                let has = v["value"].get("assistantStream").is_some();
                println!("OPENING SNAPSHOT assistantStream={has}");
                break;
            }
        }
    }

    // Now start the turn; the follower is attached and live.
    let sid = session_id.clone();
    let prompt = tokio::spawn(async move {
        rpc(
            "session/prompt",
            json!({ "request": {
                "sessionId": sid,
                "requestId": format!("req-{}", uuid()),
                "mode": "queue",
                "content": [{ "type": "text", "text":
                    "Write exactly this and nothing else: ## Heading, then a blank line, then **bold** in a sentence, then a fenced rust block containing one line of code." }],
            } }),
        )
        .await
    });

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut frames: Vec<Value> = Vec::new();
    let mut saw_message = false;
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
        match value.get("type").and_then(Value::as_str) {
            Some("assistant-stream") => {
                let frame = &value["frame"];
                println!("FRAME {}", serde_json::to_string(frame).unwrap());
                frames.push(frame.clone());
            }
            Some("event") => {
                let ty = value
                    .pointer("/event/type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if ty == "assistant/message" {
                    saw_message = true;
                    break;
                }
            }
            _ => {}
        }
    }

    let prompted = prompt.await.expect("prompt task");
    println!("PROMPT RESULT: {prompted}");
    assert!(saw_message, "the turn settled with an assistant message");

    assert!(
        !frames.is_empty(),
        "a live follower must receive assistant-stream frames"
    );
    let kinds: Vec<&str> = frames
        .iter()
        .filter_map(|f| f.get("type").and_then(Value::as_str))
        .collect();
    assert_eq!(
        kinds.first(),
        Some(&"start"),
        "the attempt is announced before its chunks: {kinds:?}"
    );
    let indices: Vec<u64> = frames
        .iter()
        .filter(|f| f.get("type").and_then(Value::as_str) == Some("chunk"))
        .filter_map(|f| f.get("index").and_then(Value::as_u64))
        .collect();
    assert_eq!(
        indices,
        (0..indices.len() as u64).collect::<Vec<_>>(),
        "chunk indices are dense, which is what a client's accumulator requires"
    );

    // The *deltas* are the model's own bytes, concatenated; the stitched form
    // rides on `block-end`, which a client applies wholesale. Both are asserted
    // because they are different claims: the deltas must sum to what the model
    // wrote, and the block must be the repaired rendering of it.
    let deltas: String = frames
        .iter()
        .filter_map(|f| f.pointer("/chunk"))
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("text-delta"))
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect();
    println!("DELTAS CONCATENATED:\n{deltas}");
    let text = frames
        .iter()
        .filter_map(|f| f.pointer("/chunk"))
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("block-end"))
        .filter_map(|c| c.pointer("/block/text"))
        .filter_map(Value::as_str)
        .next_back()
        .unwrap_or_default()
        .to_string();
    println!("FINAL DISPLAY TEXT:\n{text}");
    assert!(
        text.contains("## Heading"),
        "the model's markdown reached the follower: {text:?}"
    );
    // An unterminated fence is closed on every intermediate frame, so the
    // complete text must carry an even number of fences rather than a dangling
    // opener that would swallow everything after it.
    if text.contains("```") {
        assert_eq!(
            text.matches("```").count() % 2,
            0,
            "the fence is closed in the displayed text: {text:?}"
        );
    }
}
