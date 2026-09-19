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
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn rpc(
    method: &str,
    args: Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>> {
    let method = method.to_string();
    Box::pin(async move {
        let body = json!({
            "type": "client-request",
            "rpcId": format!("cell-{}", method.replace('/', "-")),
            "method": method,
            "payload": { "args": args },
        });
        let client = reqwest::Client::new();
        let mut b = client
            .post(format!("{}/api/{method}", base_url()))
            .json(&body);
        if let Some(c) = auth_cookie() {
            b = b.header("cookie", c);
        }
        let r = b.send().await.expect("rpc send");
        r.json::<Value>().await.expect("rpc json")
    })
}

fn value(resp: &Value) -> &Value {
    &resp["result"]["value"]
}
fn error_code(resp: &Value) -> &str {
    resp["result"]["error"]["code"].as_str().unwrap_or("")
}

async fn mkdir_temp() -> String {
    let dir = std::env::temp_dir().join(format!("vocoder-cells-{}", std::process::id()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    dir.to_string_lossy().to_string()
}

// ---------------------------------------------------------------- goals

/// Every goals endpoint addresses an *agent*, which is a live session. The
/// cells below therefore create one first: the control resolves the `agentId`
/// lookup before the remote runs, so an invented id answers
/// `session/not-found` and never reaches the goal logic at all. (These cells
/// originally passed a synthetic id and asserted on the candidate's shape,
/// which accepts one — see the note on `goals/create`'s args below.)
async fn session_for_agent() -> Value {
    let dir = mkdir_temp().await;
    let c = rpc("session/create", json!({"request": {"cwd": dir}})).await;
    value(&c)["sessionId"].clone()
}

/// `goals/create` takes its objective under `request` — the spec says so
/// (`parameters[1].wire == "request"`), and the control enforces it. A cell
/// passing `objective` as a bare arg is refused with
/// `gateway/arguments-invalid` before any goal exists, which is a different
/// assertion than the one this cell claims to make.
///
/// `create` answers a `{ref: {id, revision}}` and every other goal endpoint a
/// full `GoalView` whose lifecycle field is `phase` — values `active`,
/// `paused`, `blocked`, `complete`. Both hosts now agree on that; the candidate
/// previously answered `{accepted: bool}` and `{state: "completed"}`, which no
/// client reading `GoalView` could interpret. That divergence was invisible
/// while this cell passed a synthetic agent id the control rejects.
#[tokio::test]
async fn goals_lifecycle_cell() {
    let agent = session_for_agent().await;
    let r = rpc(
        "goals/create",
        json!({"agentId": agent, "request": {"objective": "cell objective"}}),
    )
    .await;
    assert_eq!(r["result"]["ok"], true, "{r}");
    let reference = value(&r)["ref"].clone();
    assert!(reference["id"].is_string(), "create answers a ref: {r}");
    assert_eq!(reference["revision"], 1, "{r}");

    let g = rpc("goals/get", json!({"agentId": agent})).await;
    assert_eq!(value(&g)["objective"], "cell objective", "{g}");
    assert_eq!(value(&g)["phase"], "active", "{g}");

    // `complete` addresses the goal by a `ref` carrying **both** `id` and
    // `revision` — the schema marks both required, and the control enforces it
    // with `gateway/input-invalid` on a bare id. `complete` answers the same
    // view, with the terminal phase.
    let id = value(&g)["id"].clone();
    let revision = value(&g)["revision"].clone();
    let c = rpc(
        "goals/complete",
        json!({"agentId": agent, "ref": {"id": id, "revision": revision}}),
    )
    .await;
    assert_eq!(value(&c)["phase"], "complete", "{c}");
}

/// An agent with no goal answers `null` — a *successful* read of an absent
/// goal, not an error.
#[tokio::test]
async fn goals_get_without_a_goal_is_null_cell() {
    let agent = session_for_agent().await;
    let g = rpc("goals/get", json!({"agentId": agent})).await;
    assert_eq!(g["result"]["ok"], true, "{g}");
    assert_eq!(value(&g), &serde_json::Value::Null, "{g}");
}

/// A `ref` that names no goal is refused — and the two hosts refuse it with
/// different codes, which this cell records rather than papers over.
///
/// - **control (dsh)** answers `gateway/internal` with "no current goal".
/// - **candidate (vocoderd)** answers `goal/not-found`.
///
/// The candidate's code is the better-specified one (it names the domain), but
/// the control's is what a client written against the oracle sees, so the cell
/// asserts only the shared invariant: a typed failure that is not a success and
/// not a 5xx.
#[tokio::test]
async fn goals_complete_without_goal_errors_cell() {
    let agent = session_for_agent().await;
    let r = rpc(
        "goals/complete",
        json!({"agentId": agent, "ref": {"id": "goal-does-not-exist", "revision": 1}}),
    )
    .await;
    assert_eq!(r["result"]["ok"], false, "{r}");
    let code = error_code(&r);
    assert!(
        code == "goal/not-found" || code.starts_with("gateway/"),
        "a typed refusal, not a 5xx or a silent success: {code}: {r}"
    );
}

// ---------------------------------------------------------------- settings

// Every settings cell names a namespace the host actually *registers*: the
// control host refuses an unregistered one with `settings/rejected`, so an
// invented name would only ever exercise the refusal path. Each cell uses its
// own namespace so the cells do not contend for one revision counter.
//
// Revisions are per-namespace and *persist* in the home, so a cell must not
// assume it starts at 1: it reads the current revision from `describe` and
// asserts the increment. (These cells once hardcoded 1 and passed only because
// the home happened to be fresh; a second run in the same home failed.)

/// The current revision of one settings namespace, from `describe`.
async fn settings_revision(ns: &str) -> Value {
    let d = rpc("settings/describe", json!({})).await;
    assert_eq!(d["result"]["ok"], true, "{d}");
    value(&d)["namespaces"]
        .as_array()
        .expect("namespaces array")
        .iter()
        .find(|n| n["ns"] == ns)
        .unwrap_or_else(|| panic!("catalog must list {ns}: {d}"))["revision"]
        .clone()
}

#[tokio::test]
async fn settings_roundtrip_and_conflict_cell() {
    let ns = "ui-theme";
    let before = settings_revision(ns).await.as_u64().unwrap_or(0);
    // Each write carries a *distinct* value. A patch that changes nothing is a
    // no-op that keeps the revision — correct, but it silently turns the
    // assertions below into tautologies, so a counter keeps every write
    // material across repeated runs in one home.
    let tag = std::process::id();

    let u = rpc(
        "settings/update",
        json!({"ns": ns, "patch": {"cellTag": tag, "a": 1}}),
    )
    .await;
    assert_eq!(
        value(&u)["revision"].as_u64(),
        Some(before + 1),
        "a material write increments the revision: {u}"
    );
    let conflict = rpc(
        "settings/update",
        json!({"ns": ns, "patch": {"a": 2}, "expectedRevision": 99}),
    )
    .await;
    assert_eq!(error_code(&conflict), "settings/conflict", "{conflict}");
    assert_eq!(
        conflict["result"]["error"]["details"]["actual"].as_u64(),
        Some(before + 1),
        "{conflict}"
    );
    let ok = rpc(
        "settings/update",
        json!({"ns": ns, "patch": {"a": 2}, "expectedRevision": before + 1}),
    )
    .await;
    assert_eq!(
        value(&ok)["revision"].as_u64(),
        Some(before + 2),
        "the accepted write increments again: {ok}"
    );

    // A patch already reflected in the document is a no-op: the call succeeds
    // and the revision does not move. Pinned because it is the reason these
    // cells carry distinct values rather than a constant patch.
    let same = rpc(
        "settings/update",
        json!({"ns": ns, "patch": {"a": 2}, "expectedRevision": before + 2}),
    )
    .await;
    assert_eq!(same["result"]["ok"], true, "{same}");
    assert_eq!(
        value(&same)["revision"].as_u64(),
        Some(before + 2),
        "a no-op patch keeps the revision: {same}"
    );
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
    let seed = rpc(
        "settings/update",
        json!({"ns": ns, "patch": {"x": {"y": 0}}}),
    )
    .await;
    assert_eq!(seed["result"]["ok"], true, "{seed}");
    let m = rpc(
        "settings/mutate",
        json!({
            "ns": ns,
            "ops": [
                {"op": "set", "path": ["x", "y"], "value": 3},
                {"op": "unset", "path": ["x", "y"]},
            ],
        }),
    )
    .await;
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
    let blank = rpc(
        "workspace/rename",
        json!({"request": {"workspaceId": id, "title": "   "}}),
    )
    .await;
    assert_eq!(error_code(&blank), "gateway/bad-request");
}

#[tokio::test]
async fn workspace_invalid_path_cell() {
    let r = rpc(
        "workspace/create",
        json!({"request": {"path": "/no/such/absent-dir-hopefully"}}),
    )
    .await;
    assert_eq!(error_code(&r), "workspace/invalid-path");
}

#[tokio::test]
async fn workspace_delete_unknown_cell() {
    let r = rpc(
        "workspace/delete",
        json!({"request": {"workspaceId": "ghost"}}),
    )
    .await;
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

    // `session/list`'s body argument is spelled `_request`, not `request`: the
    // spec records the wire name (`parameters[0].wire == "_request"`) and the
    // control enforces it, answering `gateway/arguments-invalid` otherwise.
    // Every other session endpoint uses `request`, so this one is worth
    // spelling out rather than inferring from its neighbours.
    let l = rpc("session/list", json!({"_request": {}})).await;
    let items = value(&l)["items"].as_array().unwrap();
    assert!(items.iter().any(|i| i["sessionId"] == sid));

    let rn = rpc(
        "session/rename",
        json!({"request": {"sessionId": sid, "title": "wire cell"}}),
    )
    .await;
    assert_eq!(value(&rn)["title"], "wire cell");

    let p = rpc(
        "session/prompt",
        json!({"request": {
            "sessionId": sid, "requestId": format!("req-{}", std::process::id()),
            "mode": "queue", "content": [{"type": "text", "text": "hello cell"}],
        }}),
    )
    .await;
    assert_eq!(p["result"]["ok"], true, "{p}");

    // `page` answers `{records, hasMore}` — there is no `cursor` field in the
    // spec — and it addresses the log *by seq*: a `throughSeq` past the end is
    // refused (`gateway/bad-request`, "past cursor N") rather than clamped, so
    // a caller cannot mistake an empty page for a truncated one. This cell
    // therefore pages from 0, which is always a valid cursor.
    let pg = rpc(
        "session/page",
        json!({"request": {
            "address": {"kind": "session", "sessionId": sid}, "throughSeq": 0,
        }}),
    )
    .await;
    assert_eq!(pg["result"]["ok"], true, "{pg}");
    let records = value(&pg)["records"]
        .as_array()
        .unwrap_or_else(|| panic!("records array expected: {pg}"));
    assert!(!records.is_empty(), "the log carries its first event: {pg}");
    // Each record is a tagged event carrying its seq, per the spec's
    // `{type: "event", event: {...}}` shape.
    let first = &records[0];
    assert_eq!(first["type"], "event", "{pg}");
    assert!(first["event"]["seq"].is_number(), "{pg}");

    // Paging past the end is refused rather than clamped — the reason this cell
    // does not simply ask for a large `throughSeq`.
    let past = rpc(
        "session/page",
        json!({"request": {
            "address": {"kind": "session", "sessionId": sid}, "throughSeq": 100_000,
        }}),
    )
    .await;
    assert_eq!(past["result"]["ok"], false, "{past}");
    assert_eq!(error_code(&past), "gateway/bad-request", "{past}");
}

#[tokio::test]
async fn session_unknown_is_not_found_cell() {
    let r = rpc(
        "session/rename",
        json!({"request": {"sessionId": "ghost-session", "title": "t"}}),
    )
    .await;
    assert_eq!(error_code(&r), "session/not-found");
}

#[tokio::test]
async fn session_prompt_requires_content_cell() {
    let dir = mkdir_temp().await;
    let c = rpc("session/create", json!({"request": {"cwd": dir}})).await;
    let sid = value(&c)["sessionId"].clone();
    let p = rpc(
        "session/prompt",
        json!({"request": {
            "sessionId": sid, "requestId": "r", "mode": "queue", "content": [],
        }}),
    )
    .await;
    assert_eq!(error_code(&p), "gateway/bad-request");
}

#[tokio::test]
async fn session_model_catalog_shape_cell() {
    let r = rpc("session/modelCatalog", json!({})).await;
    let v = value(&r);
    assert!(
        v["default"].is_object()
            && v["groups"].is_array()
            && v["failures"].is_array()
            && v["routableProviders"].is_array()
    );
}

/// An unknown namespace is refused — in two *different shapes*, recorded here
/// rather than papered over.
///
/// - **control (dsh)** answers a bare HTTP **404** with a `text/plain` body:
///   its router has no `/api/<ns>/<method>` route, so the request never reaches
///   the gateway.
/// - **candidate (vocoderd)** answers HTTP 200 with a typed
///   `gateway/internal` envelope: it registers one catch-all route and judges
///   every path there.
///
/// The candidate's shape is strictly more informative — a client can branch on
/// a code instead of a status line — but it *is* a divergence. The cell asserts
/// the invariant both meet: never a 5xx, never a silent success. It sends its
/// request by hand rather than through [`rpc`], because that helper decodes
/// JSON and the control's refusal is not JSON.
#[tokio::test]
async fn unknown_namespace_is_refused_cleanly_cell() {
    let client = reqwest::Client::new();
    let method = "nosuch/method";
    let mut b = client
        .post(format!("{}/api/{method}", base_url()))
        .json(&json!({
            "type": "client-request",
            "rpcId": format!("cell-{}", std::process::id()),
            "method": method,
            "payload": { "args": {} },
        }));
    if let Some(c) = auth_cookie() {
        b = b.header("cookie", c);
    }
    let r = b.send().await.unwrap();
    let status = r.status().as_u16();
    let body = r.text().await.unwrap_or_default();
    assert!(
        status < 500,
        "must not be a server fault ({status}): {body}"
    );

    match serde_json::from_str::<Value>(&body) {
        Ok(v) => {
            assert_eq!(v["result"]["ok"], false, "{v}");
            let code = error_code(&v);
            assert!(code.starts_with("gateway/"), "unexpected code {code}: {v}");
        }
        Err(_) => {
            assert_eq!(status, 404, "control answers a bare 404: {body}");
        }
    }
}

// ------------------------------------------------- message feedback

/// `messageFeedback` answers its business failures as *values*: the wire type
/// is a result union, so a rejected operation is a successful call
/// (`ok: true`, `value.ok: false`). A cell asserting a RemoteError here would
/// pass against a host that got the distinction wrong.
///
/// The unknown session probes that shape without needing a log to exist, and
/// is the only feedback path reachable on both hosts before any message has
/// been recorded.
#[tokio::test]
async fn message_feedback_unknown_session_is_a_value_not_an_error_cell() {
    let r = rpc(
        "messageFeedback/list",
        json!({"request": {"sessionId": format!("ghost-{}", std::process::id())}}),
    )
    .await;
    assert_eq!(r["result"]["ok"], true, "the call itself succeeds: {r}");
    let v = value(&r);
    assert_eq!(v["ok"], false, "the operation is rejected: {r}");
    assert_eq!(v["error"]["code"], "session-not-found", "{r}");
}

/// A malformed payload is refused before any business logic runs — but the two
/// hosts refuse it with *different* codes, which this cell records rather than
/// papers over.
///
/// - **control (dsh)** answers `gateway/input-invalid`, naming the field that
///   failed and the endpoint.
/// - **candidate (vocoderd)** answers `gateway/bad-request`, because it
///   validates the decoded args in the machine rather than at a boundary.
///
/// Both are `gateway/*`, both are typed envelopes, and neither reaches the
/// session store — so the cell asserts that shared invariant.
#[tokio::test]
async fn message_feedback_missing_session_id_is_a_gateway_error_cell() {
    let r = rpc("messageFeedback/list", json!({"request": {}})).await;
    assert_eq!(r["result"]["ok"], false, "{r}");
    let code = error_code(&r);
    assert!(code.starts_with("gateway/"), "unexpected code {code}: {r}");
}

// ------------------------------------------------- session feedback

#[tokio::test]
async fn session_feedback_unknown_session_is_a_value_not_an_error_cell() {
    let r = rpc(
        "sessionFeedback/record",
        json!({"request": {"sessionId": format!("ghost-{}", std::process::id()), "text": "hi"}}),
    )
    .await;
    assert_eq!(r["result"]["ok"], true, "{r}");
    let v = value(&r);
    assert_eq!(v["ok"], false, "{r}");
    assert_eq!(v["error"]["code"], "session-not-found", "{r}");
}

// ------------------------------------------- session references

/// An unknown agent is refused — in two different shapes, which this cell
/// records rather than papers over.
///
/// - **control (dsh)** answers a `session/not-found` RemoteError: it resolves
///   the `agentId` lookup before running the remote, so no such agent is a
///   gateway-level failure.
/// - **candidate (vocoderd)** answers an empty candidate array: it treats the
///   lookup as a filter, and "no session by that id" simply excludes it.
///
/// The candidate's shape is the more forgiving one — a completion popup whose
/// target session vanished should show no candidates, not fail the keystroke —
/// but it *is* a difference. The cell asserts the invariant both satisfy: a
/// well-formed envelope with no 5xx and no HTML, never a silent success
/// carrying a non-array.
#[tokio::test]
async fn session_reference_candidates_answers_a_well_formed_envelope_cell() {
    let r = rpc(
        "sessionReferenceResolver/candidates",
        json!({"agentId": format!("ghost-{}", std::process::id()), "query": ""}),
    )
    .await;
    assert_eq!(r["type"], "server-response", "{r}");
    match r["result"]["ok"].as_bool() {
        Some(true) => {
            assert!(
                value(&r).is_array(),
                "a successful read is a bare array: {r}"
            );
        }
        Some(false) => {
            // The control: an unknown agent is a lookup failure.
            assert_eq!(error_code(&r), "session/not-found", "{r}");
        }
        None => panic!("result lacks ok: {r}"),
    }
}

// ------------------------------------------------- plugin inventory

/// The inventory reports one entry per loaded plugin, and every entry carries
/// the same four fields with non-empty identity strings.
///
/// The cell deliberately does **not** assert `enabled: true` or
/// `fiberPhase: "active"` for every entry: the control composes plugins it
/// ships disabled (`include:hmr` reports `enabled: false`, `fiberPhase: null`),
/// and that is the field's whole purpose. Asserting all-active would encode the
/// candidate's narrower reality as the contract and fail the oracle.
#[tokio::test]
async fn plugin_inventory_reports_well_formed_entries_cell() {
    let r = rpc("pluginInventory/list", json!({})).await;
    assert_eq!(r["result"]["ok"], true, "{r}");
    let entries = value(&r)["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("entries array expected: {r}"));
    assert!(!entries.is_empty(), "a booted host mounts something: {r}");
    for e in entries {
        assert!(
            e["entryId"].as_str().is_some_and(|s| !s.is_empty()),
            "entryId must be a non-empty string: {e}"
        );
        assert!(
            e["moduleName"].as_str().is_some_and(|s| !s.is_empty()),
            "moduleName must be a non-empty string: {e}"
        );
        // `enabled` is a boolean and `fiberPhase` is a phase-or-null; a
        // disabled entry is null, an enabled one is a phase string.
        let enabled = e["enabled"]
            .as_bool()
            .unwrap_or_else(|| panic!("enabled bool: {e}"));
        let phase = &e["fiberPhase"];
        if enabled {
            assert!(phase.is_string(), "an enabled entry has a phase: {e}");
        } else {
            assert_eq!(
                phase,
                &serde_json::Value::Null,
                "a disabled entry has none: {e}"
            );
        }
    }
}

/// A body that is not JSON at all is refused before the gateway decodes it —
/// and the two hosts refuse it in *different shapes*, which this cell records
/// rather than papers over.
///
/// - **control (dsh)** answers HTTP **400** with a `text/plain` body
///   (`body is not JSON`): its HTTP layer rejects the request before the
///   gateway sees it.
/// - **candidate (vocoderd)** answers HTTP 200 with a typed
///   `gateway/bad-request` envelope: it has one catch-all route and judges
///   everything there.
///
/// The invariant both satisfy is the thing worth asserting: never a 5xx, never
/// a silent success. `r.json()` here would panic on the control's plain-text
/// body, which is exactly the bug this cell had.
#[tokio::test]
async fn malformed_envelope_is_refused_cleanly_cell() {
    let client = reqwest::Client::new();
    let mut b = client
        .post(format!("{}/api/session/list", base_url()))
        .body("not json")
        .header("content-type", "application/json");
    if let Some(c) = auth_cookie() {
        b = b.header("cookie", c);
    }
    let r = b.send().await.unwrap();
    let status = r.status().as_u16();
    let body = r.text().await.unwrap_or_default();
    assert!(
        status < 500,
        "must not be a server fault ({status}): {body}"
    );

    match serde_json::from_str::<Value>(&body) {
        // The candidate: a typed failure envelope.
        Ok(v) => {
            assert_eq!(v["result"]["ok"], false, "{v}");
            assert!(error_code(&v).starts_with("gateway/"), "{v}");
        }
        // The control: a plain-text rejection, not the JSON envelope.
        Err(_) => {
            assert_eq!(status, 400, "control answers a bare 400: {body}");
            assert!(!body.is_empty(), "the refusal carries a reason");
        }
    }
}
