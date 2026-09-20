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
///
/// `agentTeams` is deliberately absent. Upstream's agent-team Remote lives in
/// an experimental profile layer that the default web profile does not
/// compose, so the control answers HTTP 404 for the whole namespace. A
/// generated cell here would compare the candidate against a host that has no
/// such endpoint, which measures nothing. The candidate's behavior for it is
/// covered by unit tests in `machines/agent_teams.rs` instead.
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
    "llm",
    "subagents",
    "fileUploads",
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

/// An envelope whose `payload` carries **no `args` field** is refused by both
/// hosts, in *different codes*, which this cell records rather than papers over.
///
/// - **control (dsh)**: `gateway/internal` "Remote payload must contain exactly
///   one plain-object args field" — its payload reader rejects the absent key
///   before the descriptor is consulted.
/// - **candidate (vocoderd)**: `gateway/arguments-invalid` — a non-object (or
///   absent) `args` cannot satisfy a descriptor that requires args, so it reads
///   as a *missing required arg*.
///
/// Both refuse; the code differs because each host reaches the judgement by a
/// different route. The cell asserts the invariant both meet — a typed
/// `gateway/*` failure, never a 5xx and never a silent success — so a candidate
/// that started accepting an args-less envelope would fail here.
#[tokio::test]
async fn an_absent_args_field_is_refused_by_both_hosts() {
    for payload in [json!({}), json!({ "args": null }), json!({ "args": "not-an-object" })] {
        let (status, v) = post_raw(
            "subagents/list",
            &json!({
                "type": "client-request",
                "rpcId": format!("cell-{}", uuid()),
                "method": "subagents/list",
                "payload": payload,
            }),
        )
        .await;
        assert!(status < 500, "never a server fault ({status}): {v}");
        assert_eq!(v["result"]["ok"], false, "payload {payload}: {v}");
        let code = v["result"]["error"]["code"].as_str().unwrap_or_default();
        assert!(
            code.starts_with("gateway/"),
            "payload {payload}: expected a gateway/* refusal, got {code}: {v}"
        );
    }
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

// -- `subagents` and `llm`: the behaviors mechanical cells cannot see --------
//
// The generated cells above check only that each endpoint answers a typed
// envelope. These check the *contract*, and every assertion below was read off
// the running control rather than inferred from the schema — three module doc
// comments in `machines/subagents.rs` had asserted control behavior that the
// control contradicted, which is why these exist.

/// The catalog is a **successful** call for a parent that does not resolve;
/// `parentAvailable` is a required field and carries the distinction. Upstream
/// computes it in `catalogView` and its own suite asserts this shape.
#[tokio::test]
async fn subagents_catalog_reports_an_unknown_parent_without_failing() {
    let v = call(
        "subagents/list",
        json!({ "parentSessionId": "cell-no-such-parent" }),
    )
    .await;
    assert_eq!(v["result"]["ok"], true, "{v}");
    assert_eq!(v["result"]["value"]["entries"], json!([]), "{v}");
    assert_eq!(v["result"]["value"]["parentAvailable"], false, "{v}");
}

/// `parentAvailable` is `true` for a parent this host can see. A *candidate*
/// bug hid here: the machine cached the sessions walk forever, so a session
/// created moments earlier read as absent. The control answered `true`.
#[tokio::test]
async fn subagents_catalog_sees_a_parent_created_this_session() {
    let created = call("session/create", json!({ "request": { "cwd": "/tmp" } })).await;
    let sid = created["result"]["value"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/create gave no id: {created}"))
        .to_string();
    let v = call("subagents/list", json!({ "parentSessionId": sid })).await;
    assert_eq!(v["result"]["ok"], true, "{v}");
    assert_eq!(
        v["result"]["value"]["parentAvailable"], true,
        "a session created in this process must be visible: {v}"
    );
}

/// Three boundary layers, each with its own code. `list` is the cheapest
/// endpoint to exercise them on: one arg, three ways to get it wrong.
#[tokio::test]
async fn subagents_boundary_layers_answer_distinct_codes() {
    // 1. args vs the descriptor → arguments-invalid.
    let missing = call("subagents/list", json!({})).await;
    assert_eq!(
        missing["result"]["error"]["code"], "gateway/arguments-invalid",
        "{missing}"
    );
    let extra = call(
        "subagents/list",
        json!({ "parentSessionId": "p", "zzz": 1 }),
    )
    .await;
    assert_eq!(
        extra["result"]["error"]["code"], "gateway/arguments-invalid",
        "{extra}"
    );

    // 2. arg value vs its codec → input-invalid, naming the field.
    let wrong_type = call("subagents/list", json!({ "parentSessionId": 7 })).await;
    assert_eq!(
        wrong_type["result"]["error"]["code"], "gateway/input-invalid",
        "{wrong_type}"
    );
    assert_eq!(
        wrong_type["result"]["error"]["details"]["field"], "parentSessionId",
        "{wrong_type}"
    );

    // 3. a required id that is empty → bad-request with details.issues.
    let empty = call("subagents/list", json!({ "parentSessionId": "" })).await;
    assert_eq!(
        empty["result"]["error"]["code"], "gateway/bad-request",
        "{empty}"
    );
    assert!(
        empty["result"]["error"]["details"]["issues"].is_array(),
        "details.issues expected: {empty}"
    );
}

/// `prompt`'s `request` is validated as a whole: the control names the *arg*
/// (`request`) and never the failing inner member.
#[tokio::test]
async fn subagents_prompt_request_fails_as_one_wire_field() {
    for overrides in [
        json!({ "mode": "one-shot" }),
        json!({ "delivery": "later" }),
        json!({ "content": "not-an-array" }),
        json!({ "requestId": 7 }),
    ] {
        let mut request = json!({
            "requestId": "cell-r",
            "parentSessionId": "cell-p",
            "childSessionId": "cell-c",
            "mode": "continuable",
            "delivery": "queue",
            "content": [{ "type": "text", "text": "hi" }],
        });
        for (k, val) in overrides.as_object().unwrap() {
            request[k] = val.clone();
        }
        let v = call("subagents/prompt", json!({ "request": request })).await;
        assert_eq!(
            v["result"]["error"]["code"], "gateway/input-invalid",
            "{overrides} → {v}"
        );
        assert_eq!(
            v["result"]["error"]["details"]["field"], "request",
            "{overrides} → {v}"
        );
    }
}

/// An unexpected key *inside* `request` is tolerated — the control passes it
/// through to domain logic, so a stricter candidate would diverge. The
/// observable proof is that the answer is a *domain* code, not a boundary one.
#[tokio::test]
async fn subagents_prompt_tolerates_an_unknown_nested_key() {
    let v = call(
        "subagents/prompt",
        json!({ "request": {
            "requestId": "cell-r",
            "parentSessionId": "cell-p",
            "childSessionId": "cell-c",
            "mode": "continuable",
            "delivery": "queue",
            "content": [{ "type": "text", "text": "hi" }],
            "smuggled": true,
        }}),
    )
    .await;
    let code = v["result"]["error"]["code"].as_str().unwrap_or("");
    assert!(
        !code.starts_with("gateway/"),
        "a nested extra key reaches domain logic, not the boundary: {v}"
    );
}

/// The parent is admitted **before** the child is considered. Both hosts must
/// answer the parent refusal for an unknown parent even when the child is also
/// unknown — checking the child first reverses the two answers.
#[tokio::test]
async fn subagents_prompt_checks_the_parent_before_the_child() {
    let v = call(
        "subagents/prompt",
        json!({ "request": {
            "requestId": "cell-r",
            "parentSessionId": "cell-no-parent",
            "childSessionId": "cell-no-child",
            "mode": "continuable",
            "delivery": "queue",
            "content": [{ "type": "text", "text": "hi" }],
        }}),
    )
    .await;
    assert_eq!(
        v["result"]["error"]["code"], "subagent/parent-unavailable",
        "{v}"
    );
    assert_eq!(
        v["result"]["error"]["details"]["parentSessionId"], "cell-no-parent",
        "{v}"
    );
}

/// An interrupt for sessions that do not exist is **accepted**: upstream's
/// `interrupt` is a no-op with no continuation service mounted, and its
/// contract states absent targets are accepted. Refusing would be stricter
/// than the host this mirrors.
#[tokio::test]
async fn subagents_interrupt_accepts_an_absent_target() {
    let v = call(
        "subagents/interruptByParent",
        json!({
            "parentSessionId": "cell-no-parent",
            "childSessionId": "cell-no-child",
            "mode": "continuable",
        }),
    )
    .await;
    assert_eq!(v["result"]["ok"], true, "{v}");
    assert_eq!(v["result"]["value"]["accepted"], true, "{v}");
}

/// `mode` is `const "continuable"` on the interrupt endpoint too: absent is a
/// missing arg, present-but-wrong is a boundary failure.
#[tokio::test]
async fn subagents_interrupt_requires_the_continuable_mode() {
    let absent = call(
        "subagents/interruptByParent",
        json!({ "parentSessionId": "p", "childSessionId": "c" }),
    )
    .await;
    assert_eq!(
        absent["result"]["error"]["code"], "gateway/arguments-invalid",
        "{absent}"
    );

    let wrong = call(
        "subagents/interruptByParent",
        json!({ "parentSessionId": "p", "childSessionId": "c", "mode": "one-shot" }),
    )
    .await;
    assert_eq!(
        wrong["result"]["error"]["code"], "gateway/input-invalid",
        "{wrong}"
    );
    assert_eq!(
        wrong["result"]["error"]["details"]["field"], "mode",
        "{wrong}"
    );
}

/// The model catalog the client's picker renders. `session/modelCatalog` and
/// `llm/listProviders` are both called during boot, so they must agree on the
/// provider set — the session machine delegates to the llm machine for exactly
/// that reason.
#[tokio::test]
async fn llm_providers_agree_with_the_session_model_catalog() {
    let providers = call("llm/listProviders", json!({})).await;
    assert_eq!(providers["result"]["ok"], true, "{providers}");
    let providers = providers["result"]["value"].as_array().unwrap();
    assert_eq!(providers.len(), 1, "one composed route: {providers:?}");
    let provider_id = providers[0]["id"].as_str().unwrap();

    // `modelCatalog` takes **no** args: the control answers
    // `arguments-invalid` for an unexpected `sessionId`, and the spec's
    // parameter list for it is empty.
    let catalog = call("session/modelCatalog", json!({})).await;
    assert_eq!(catalog["result"]["ok"], true, "{catalog}");
    let default_provider = catalog["result"]["value"]["default"]["provider"]
        .as_str()
        .unwrap();
    assert_eq!(
        default_provider, provider_id,
        "the catalog's default provider must be a listed route: {catalog}"
    );
    assert_eq!(
        catalog["result"]["value"]["routableProviders"][0], provider_id,
        "{catalog}"
    );
}

/// `listConfigurableProviders` names the route and the settings namespace that
/// configures it. Its `declared` field is optional and marks a route that is
/// declarable but not yet activated — upstream's `llm-pi-ai` routes carry it.
#[tokio::test]
async fn llm_configurable_providers_name_a_settings_namespace() {
    let v = call("llm/listConfigurableProviders", json!({})).await;
    assert_eq!(v["result"]["ok"], true, "{v}");
    let rows = v["result"]["value"].as_array().unwrap();
    assert!(!rows.is_empty(), "{v}");
    for row in rows {
        assert!(row["provider"].is_string(), "{row}");
        assert!(row["settingsNs"].is_string(), "{row}");
        assert!(row["settingsPath"].is_array(), "{row}");
    }
    // The route this host composes is among them.
    let ours = rows
        .iter()
        .find(|r| r["provider"] == "deepseek-official")
        .unwrap_or_else(|| panic!("deepseek-official not listed: {v}"));
    assert_eq!(ours["settingsNs"], "llm-deepseek", "{ours}");
}

/// `discoverModels` is a *network* call upstream and this host registers no
/// discovery, so it refuses with a typed code rather than an empty list:
/// "this host cannot discover" and "that provider has no models" differ.
#[tokio::test]
async fn llm_discover_models_is_refused_typed() {
    let v = call(
        "llm/discoverModels",
        json!({ "settingsNs": "llm-deepseek", "request": {} }),
    )
    .await;
    assert_eq!(v["result"]["ok"], false, "{v}");
    assert_eq!(
        v["result"]["error"]["code"], "llm/model-discovery-rejected",
        "{v}"
    );
}
