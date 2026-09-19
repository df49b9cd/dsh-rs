// Per-endpoint unary RPC conformance cells generated mechanically from
// spec/typert/remote.json. For every unary endpoint on live namespaces we
// exercise:
//   1. malformed envelope     → gateway/bad-request
//   2. unknown method         → gateway/bad-request (or a namespaced error)
//   3. empty-args call        → well-formed server-response envelope
//      (no timeout, no HTML error page; result may be ok or a RemoteError)
// Stream endpoints (mode:"stream") are covered by streams_spec.rs instead.
//
// Run against either host: CONFORMANCE_BASE_URL=http://127.0.0.1:PORT.

use serde_json::json;

fn base_url() -> String {
    std::env::var("CONFORMANCE_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:3080".into())
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

/// Namespaces the candidate host implements today. Generated cells skip
/// anything outside this set; as machines land, extend the set.
const LIVE_NAMESPACES: &[&str] = &[
    "goals",
    "session",
    "workspace",
    "settings",
    "workspaceFiles",
    "directoryPicker",
    "credentials",
    "skills",
    "fileReferences",
    "commands",
    "agentPresets",
    "messageFeedback",
    "sessionFeedback",
    "sessionReferenceResolver",
    "pluginInventory",
];

struct Endpoint {
    namespace: String,
    method: String,
    mode: String,
}

fn load_endpoints() -> Vec<Endpoint> {
    // spec/ is resolved relative to the workspace root; the test binary's
    // cwd is conformance/wire, hence ../../spec.
    let text = std::fs::read_to_string("../../spec/typert/remote.json")
        .expect("spec/typert/remote.json readable");
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    doc["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| Endpoint {
            namespace: e["namespace"].as_str().unwrap().to_string(),
            method: e["method"].as_str().unwrap().to_string(),
            mode: e["mode"].as_str().unwrap_or("unary").to_string(),
        })
        .collect()
}

/// Signed dsh cookie ("k=v") minted by harness/runners/run.sh when running
/// the control host; ignored by vocoderd.
///
/// Every request must carry it: the control host gates the whole `/api`
/// surface behind browser auth, so a cell that omits it gets an HTML redirect
/// rather than a `server-response` envelope. (This suite grew from vocoderd,
/// which is loopback-trusted, so the omission was invisible until the control
/// host was actually run.)
fn auth_cookie() -> Option<String> {
    let path = std::env::var("CONFORMANCE_COOKIE_FILE").ok()?;
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

async fn post_raw(path: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
    let mut req = reqwest::Client::new()
        .post(format!("{}/api/{}", base_url(), path))
        .json(body)
        // A per-request deadline, because one endpoint on the control blocks
        // forever: `directoryPicker/pick` opens a native dialog and waits for a
        // human, so a matrix cell that reaches it hangs the whole run instead
        // of reporting. Without this the suite cannot complete against the
        // oracle at all — the failure mode is a silent 10-minute timeout, not a
        // red cell, which is exactly the kind of thing a parity check must not
        // have.
        .timeout(std::time::Duration::from_secs(15));
    if let Some(c) = auth_cookie() {
        req = req.header("cookie", c);
    }
    let res = match req.send().await {
        Ok(res) => res,
        // A timeout or transport failure is reported as such rather than
        // panicking: the cell then fails with a readable cause.
        Err(e) => {
            return (
                0,
                serde_json::json!({
                    "__transportError": e.is_timeout().then_some("timeout").unwrap_or("send"),
                    "__message": e.to_string(),
                }),
            );
        }
    };
    let status = res.status().as_u16();
    let v: serde_json::Value = res
        .json()
        .await
        .unwrap_or_else(|_| serde_json::json!({"__nonJson": true}));
    (status, v)
}

async fn call(method: &str, args: serde_json::Value) -> serde_json::Value {
    let (_, v) = post_raw(
        method,
        &json!({
            "type": "client-request",
            "rpcId": format!("cell-{}", uuid()),
            "method": method,
            "payload": { "args": args },
        }),
    )
    .await;
    v
}

#[tokio::test]
async fn malformed_envelope_yields_bad_request() {
    let (_, v) = post_raw("session/list", &json!({"not": "an envelope"})).await;
    // Gateway error: a well-formed failure envelope.
    assert_eq!(v["type"], "server-response", "{v}");
    assert_eq!(v["result"]["ok"], false, "{v}");
    assert_eq!(v["result"]["error"]["code"], "gateway/bad-request", "{v}");
}

/// An unknown method is refused cleanly — and the two hosts refuse it in
/// *different shapes*, which this cell records rather than papers over.
///
/// - **control (dsh)** answers HTTP 404 with a `text/plain` body: its router
///   has no `/api/<ns>/<method>` route for an unregistered method.
/// - **candidate (vocoderd)** answers HTTP 200 with a typed envelope
///   (`gateway/bad-request`, or `gateway/internal` for an unknown namespace):
///   it registers one catch-all `/api/{*endpoint}` route, so every path
///   reaches the gateway and is judged there.
///
/// The candidate's shape is strictly more informative — a client can branch on
/// a code instead of on a status line — but it is a real divergence, so the
/// cell asserts the invariant both satisfy: never a 5xx, never an HTML error
/// page, and never a silent success.
#[tokio::test]
async fn unknown_method_is_refused_cleanly() {
    let (status, v) = post_raw(
        "session/noSuchMethod",
        &json!({
            "type": "client-request",
            "rpcId": format!("cell-{}", uuid()),
            "method": "session/noSuchMethod",
            "payload": { "args": {} },
        }),
    )
    .await;

    assert!(
        status < 500,
        "must not be a server fault (got {status}): {v}"
    );
    if v["type"] == "server-response" {
        // The candidate: a typed failure with a code.
        assert_eq!(v["result"]["ok"], false, "{v}");
        let code = v["result"]["error"]["code"].as_str().unwrap_or_default();
        assert!(
            code.starts_with("gateway/") || code.starts_with("session/"),
            "unexpected code {code}: {v}"
        );
    } else {
        // The control: a plain 404 whose body is not the JSON envelope.
        assert_eq!(status, 404, "control answers a bare 404: {v}");
        assert_eq!(v["__nonJson"], true, "{v}");
    }
}

/// The generated per-endpoint matrix: for every unary endpoint in a live
/// namespace, the envelope round-trips with ok-or-typed-error (never a
/// 5xx, never HTML, never a hang).
/// One endpoint is exempt, and the reason is worth stating: `directoryPicker/pick`
/// on the control opens a **native file dialog** and blocks until a human
/// answers it. There is no timeout inside the host and no way for a headless
/// cell to dismiss it, so the honest report is that this endpoint cannot be
/// exercised black-box on the control — not that it passed. The candidate
/// answers it immediately with `directory-picker/unavailable`, which is the
/// correct typed refusal for a host with no native picker.
#[tokio::test]
async fn every_unary_endpoint_answers_typed_envelope() {
    let endpoints = load_endpoints();
    let mut failures: Vec<String> = Vec::new();
    let mut covered = 0usize;
    let mut blocked: Vec<String> = Vec::new();
    for ep in endpoints
        .iter()
        .filter(|e| LIVE_NAMESPACES.contains(&e.namespace.as_str()) && e.mode != "stream")
    {
        covered += 1;
        let method = format!("{}/{}", ep.namespace, ep.method);
        let v = call(&method, json!({})).await;
        // A blocked endpoint is recorded, not failed: see the doc comment.
        if v["__transportError"].is_string() && method == "directoryPicker/pick" {
            blocked.push(method);
            continue;
        }
        if let Some(kind) = v["__transportError"].as_str() {
            failures.push(format!("{method}: transport {kind}: {v}"));
            continue;
        }
        if v["type"] != "server-response" {
            failures.push(format!("{method}: not a server-response: {v}"));
            continue;
        }
        let result = &v["result"];
        match result["ok"].as_bool() {
            Some(true) => {
                if !result.get("value").is_some() {
                    failures.push(format!("{method}: ok without value: {v}"));
                }
            }
            Some(false) => {
                let code = result["error"]["code"].as_str().unwrap_or_default();
                if code.is_empty() {
                    failures.push(format!("{method}: error without code: {v}"));
                }
                if result["error"]["message"]
                    .as_str()
                    .is_none_or(str::is_empty)
                {
                    failures.push(format!("{method}: error without message: {v}"));
                }
            }
            None => failures.push(format!("{method}: result lacks ok: {v}")),
        }
    }
    assert!(
        covered >= 61,
        "expected ≥61 live unary endpoints, saw {covered}"
    );
    if !blocked.is_empty() {
        eprintln!(
            "blocked on this host (not exercised): {}",
            blocked.join(", ")
        );
    }
    assert!(
        failures.is_empty(),
        "cell failures:\n{}",
        failures.join("\n")
    );
}

/// Error-detail shape: known conflict cases carry structured details.
#[tokio::test]
async fn error_detail_shape_session_not_found() {
    // Prompt an unknown session → session/not-found + details.sessionId.
    // The payload must satisfy the spec's prompt schema (required: requestId,
    // sessionId, mode, content; `mode` is `"queue" | "steer"`): a payload the
    // gateway rejects is `gateway/input-invalid` before the session is
    // consulted, which is a different cell than this one.
    let v = call(
        "session/prompt",
        json!({ "request": {
            "sessionId": format!("missing-{}", uuid()),
            "requestId": "r1",
            "mode": "queue",
            "content": [{ "type": "text", "text": "hi" }],
        } }),
    )
    .await;
    assert_eq!(v["result"]["ok"], false, "{v}");
    assert_eq!(v["result"]["error"]["code"], "session/not-found", "{v}");
    assert!(
        v["result"]["error"]["details"]["sessionId"].is_string(),
        "details.sessionId expected: {v}"
    );
}

#[tokio::test]
async fn settings_conflict_details() {
    // Seed a registered namespace, then write with a stale expectedRevision →
    // settings/conflict with {ns, expected, actual}. The namespace must be one
    // the host registers: the control refuses an invented one with
    // `settings/rejected` before any revision is consulted.
    let ns = "locale";
    let v1 = call("settings/update", json!({ "ns": ns, "patch": { "k": 1 } })).await;
    assert_eq!(v1["result"]["ok"], true, "{v1}");
    let v2 = call(
        "settings/update",
        json!({ "ns": ns, "patch": { "k": 2 }, "expectedRevision": 99 }),
    )
    .await;
    assert_eq!(v2["result"]["error"]["code"], "settings/conflict", "{v2}");
    assert_eq!(
        v2["result"]["error"]["details"]["ns"],
        serde_json::json!(ns)
    );
    assert!(v2["result"]["error"]["details"]["actual"].is_number());
}
