// Wire cells for the unary business endpoints. Runs against any host
// whose URL is in CONFORMANCE_BASE_URL. Each #[tokio::test] below is one
// cell; the runner collects per-cell pass/fail per host for diffing.

use serde_json::{Value, json};

fn base_url() -> String {
    std::env::var("CONFORMANCE_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:3080".into())
}

/// Signed dsh cookie ("k=v") minted by harness/runners/run.sh when running
/// the control host; ignored by vocoderd.
fn auth_cookie() -> Option<String> {
    let path = std::env::var("CONFORMANCE_COOKIE_FILE").ok()?;
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn rpc(method: &str, args: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>> {
    let method = method.to_string();
    Box::pin(async move {
        let body = json!({
            "type": "client-request",
            "rpcId": format!("cell-{}", method.replace('/', "-")),
            "method": method,
            "payload": { "args": args },
        });
        let client = reqwest::Client::new();
        let mut b = client.post(format!("{}/api/{method}", base_url())).json(&body);
        if let Some(c) = auth_cookie() {
            b = b.header("cookie", c);
        }
        let r = b.send().await.expect("rpc send");
        r.json::<Value>().await.expect("rpc json")
    })
}

fn value(resp: &Value) -> &Value { &resp["result"]["value"] }
fn error_code(resp: &Value) -> &str { resp["result"]["error"]["code"].as_str().unwrap_or("") }

async fn mkdir_temp() -> String {
    let dir = std::env::temp_dir().join(format!("vocoder-cells-{}", std::process::id()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    dir.to_string_lossy().to_string()
}

// ---------------------------------------------------------------- goals

#[tokio::test]
async fn goals_lifecycle_cell() {
    let agent = json!(format!("cell-agent-{}", std::process::id()));
    let r = rpc("goals/create", json!({"agentId": agent, "objective": "cell objective"})).await;
    assert_eq!(r["result"]["ok"], true, "{r}");
    let g = rpc("goals/get", json!({"agentId": agent})).await;
    assert_eq!(value(&g)["objective"], "cell objective");
    let c = rpc("goals/complete", json!({"agentId": agent})).await;
    assert_eq!(value(&c)["state"], "completed");
}

#[tokio::test]
async fn goals_complete_without_goal_errors_cell() {
    let agent = json!(format!("ghost-agent-{}", std::process::id()));
    let r = rpc("goals/complete", json!({"agentId": agent})).await;
    assert_eq!(r["result"]["ok"], false);
    assert_eq!(error_code(&r), "goal/not-found");
}

// ---------------------------------------------------------------- settings

// Every settings cell names a namespace the host actually *registers*: the
// control host refuses an unregistered one with `settings/rejected`, so an
// invented name would only ever exercise the refusal path. Each cell uses its
// own namespace because revisions are per-namespace, and the runner starts
// from a fresh home so the first write in a run is revision 1.

#[tokio::test]
async fn settings_roundtrip_and_conflict_cell() {
    let ns = "ui-theme";
    let d0 = rpc("settings/describe", json!({})).await;
    assert_eq!(d0["result"]["ok"], true);
    let described: Vec<&str> = value(&d0)["namespaces"]
        .as_array()
        .expect("namespaces array")
        .iter()
        .filter_map(|n| n["ns"].as_str())
        .collect();
    assert!(described.contains(&ns), "catalog must list {ns}: {described:?}");

    let u = rpc("settings/update", json!({"ns": ns, "patch": {"a": 1}})).await;
    assert_eq!(value(&u)["revision"], 1, "{u}");
    let conflict = rpc("settings/update", json!({"ns": ns, "patch": {"a": 2}, "expectedRevision": 99})).await;
    assert_eq!(error_code(&conflict), "settings/conflict");
    assert_eq!(conflict["result"]["error"]["details"]["actual"], 1);
    let ok = rpc("settings/update", json!({"ns": ns, "patch": {"a": 2}, "expectedRevision": 1})).await;
    assert_eq!(value(&ok)["revision"], 2, "{ok}");
}

/// An unregistered namespace is refused, not created — the control host's
/// behavior, and the reason the cells above cannot invent names.
#[tokio::test]
async fn settings_unregistered_namespace_is_rejected_cell() {
    let r = rpc(
        "settings/update",
        json!({"ns": "definitely-not-a-namespace", "patch": {"a": 1}}),
    )
    .await;
    assert_eq!(error_code(&r), "settings/rejected", "{r}");
}

#[tokio::test]
async fn settings_mutate_ops_cell() {
    let ns = "ui-chat";
    let seed = rpc("settings/update", json!({"ns": ns, "patch": {"x": {"y": 0}}})).await;
    assert_eq!(seed["result"]["ok"], true, "{seed}");
    let m = rpc("settings/mutate", json!({
        "ns": ns,
        "ops": [
            {"op": "set", "path": ["x", "y"], "value": 3},
            {"op": "unset", "path": ["x", "y"]},
        ],
    })).await;
    assert_eq!(value(&m)["value"]["x"], json!({}), "{m}");
}

// ---------------------------------------------------------------- workspace

#[tokio::test]
async fn workspace_create_idempotent_and_rename_cell() {
    let dir = mkdir_temp().await;
    let c1 = rpc("workspace/create", json!({"request": {"path": dir}})).await;
    assert_eq!(c1["result"]["ok"], true, "{c1}");
    let id = value(&c1)["workspace"]["workspaceId"].clone();
    let c2 = rpc("workspace/create", json!({"request": {"path": dir}})).await;
    assert_eq!(value(&c2)["created"], false);
    let blank = rpc("workspace/rename", json!({"request": {"workspaceId": id, "title": "   "}})).await;
    assert_eq!(error_code(&blank), "gateway/bad-request");
}

#[tokio::test]
async fn workspace_invalid_path_cell() {
    let r = rpc("workspace/create", json!({"request": {"path": "/no/such/absent-dir-hopefully"}})).await;
    assert_eq!(error_code(&r), "workspace/invalid-path");
}

#[tokio::test]
async fn workspace_delete_unknown_cell() {
    let r = rpc("workspace/delete", json!({"request": {"workspaceId": "ghost"}})).await;
    assert_eq!(error_code(&r), "workspace/not-found");
    assert_eq!(r["result"]["error"]["details"]["workspaceId"], "ghost");
}

// ---------------------------------------------------------------- session

#[tokio::test]
async fn session_create_list_rename_page_cell() {
    let dir = mkdir_temp().await;
    let c = rpc("session/create", json!({"request": {"cwd": dir}})).await;
    let sid = value(&c)["sessionId"].clone();
    assert!(sid.as_str().unwrap().starts_with("session-"));

    let l = rpc("session/list", json!({"request": {}})).await;
    let items = value(&l)["items"].as_array().unwrap();
    assert!(items.iter().any(|i| i["sessionId"] == sid));

    let rn = rpc("session/rename", json!({"request": {"sessionId": sid, "title": "wire cell"}})).await;
    assert_eq!(value(&rn)["title"], "wire cell");

    let p = rpc("session/prompt", json!({"request": {
        "sessionId": sid, "requestId": format!("req-{}", std::process::id()),
        "mode": "queue", "content": [{"type": "text", "text": "hello cell"}],
    }})).await;
    assert_eq!(p["result"]["ok"], true, "{p}");

    let pg = rpc("session/page", json!({"request": {
        "address": {"kind": "session", "sessionId": sid}, "throughSeq": 1000,
    }})).await;
    assert!(value(&pg)["records"].as_array().unwrap().len() >= 1, "{pg}");
}

#[tokio::test]
async fn session_unknown_is_not_found_cell() {
    let r = rpc("session/rename", json!({"request": {"sessionId": "ghost-session", "title": "t"}})).await;
    assert_eq!(error_code(&r), "session/not-found");
}

#[tokio::test]
async fn session_prompt_requires_content_cell() {
    let dir = mkdir_temp().await;
    let c = rpc("session/create", json!({"request": {"cwd": dir}})).await;
    let sid = value(&c)["sessionId"].clone();
    let p = rpc("session/prompt", json!({"request": {
        "sessionId": sid, "requestId": "r", "mode": "queue", "content": [],
    }})).await;
    assert_eq!(error_code(&p), "gateway/bad-request");
}

#[tokio::test]
async fn session_model_catalog_shape_cell() {
    let r = rpc("session/modelCatalog", json!({})).await;
    let v = value(&r);
    assert!(v["default"].is_object() && v["groups"].is_array()
        && v["failures"].is_array() && v["routableProviders"].is_array());
}

#[tokio::test]
async fn unknown_namespace_maps_to_gateway_internal_cell() {
    let r = rpc("nosuch/method", json!({})).await;
    assert_eq!(r["result"]["ok"], false);
    // The exact code is host-owned; we require only that it is one of the
    // gateway/* family (JS uses the same).
    let code = error_code(&r);
    assert!(code.starts_with("gateway/"), "unexpected code {code}");
}

#[tokio::test]
async fn malformed_envelope_is_bad_request_cell() {
    let client = reqwest::Client::new();
    let mut b = client
        .post(format!("{}/api/session/list", base_url()))
        .body("not json")
        .header("content-type", "application/json");
    if let Some(c) = auth_cookie() {
        b = b.header("cookie", c);
    }
    let r = b.send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["result"]["ok"], false);
    assert_eq!(error_code(&v), "gateway/bad-request");
}
