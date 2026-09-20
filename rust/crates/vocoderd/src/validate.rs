//! The dispatch boundary: check a call's arguments against the spec before a
//! machine sees them.
//!
//! Every machine's `handle` is sync and pure (`docs/architecture.md`), so it
//! cannot deserialize a DTO per call without either poking untyped JSON — the
//! `rpc::arg_str(&req, "path")` habit this module exists to retire — or
//! inverting the machine contract to `async_trait`. So the spec becomes *data*
//! (`vocoder_spec_api::validate`, generated from `spec/typert/remote.json`) and
//! this module applies it at one place: before a `vocoder/{ns}/call` event is
//! delivered.
//!
//! **Why here, and why this shape.** The control host validates at its own
//! boundary and answers four distinct codes, which were read off the running
//! control rather than inferred from the schema:
//!
//! 0. `gateway/internal` — the `payload` is not exactly one plain-object
//!    `args` field. This gate runs *before* the descriptor is resolved
//!    (`remoteRequest` in the gateway), so it lives in `main.rs` as
//!    [`payload_shape_ok`], not in [`check`].
//! 1. `gateway/arguments-invalid` — the `args` object's *names* do not match
//!    the descriptor (`missing "x"` / `unexpected "x"`).
//! 2. `gateway/input-invalid` — an arg is present but its *value* fails the
//!    codec's schema; `details.field` names the arg.
//! 3. `gateway/bad-request` with `details.issues` — a value violates a `min(1)`
//!    the remote's own zod schema declares.
//!
//! This module implements 1 and 2. **Layer 3 is deliberately absent**: the
//! extracted JSON Schema does not carry `minLength` (verified — the string does
//! not appear in `spec/typert/remote.json`), so it is not derivable here, and
//! the machines that need it implement it directly. Inventing it from the
//! control's observed behavior would put a rule in the validator that nothing
//! can trace back to the spec.
//!
//! **Nested values, not nested extras.** Layer 2 descends into an arg's object
//! members, array items, and `anyOf`/`allOf` branches, because the control
//! does: a *nested* missing required field (`session/page`'s `request` without
//! `address`) is `gateway/input-invalid` naming the **outer** arg, and a nested
//! value that fails its type or `const` is likewise refused. What it does
//! **not** do is reject a nested *extra* key: the control tolerates an
//! unexpected key anywhere below the top level of `args` and passes it to
//! domain logic. So the interpreter honors every keyword the generator kept and
//! has no `additionalProperties` rule at all — that keyword is dropped when the
//! spec is pruned, precisely so a deeper walk cannot become stricter than the
//! host it mirrors.
//!
//! The report is always the **outer** arg's wire name, matching the control:
//! a nested failure names `request`, never `address.sessionId`.

use serde_json::Value;
use vocoder_spec_api::validate::endpoint;

/// The verbatim message the control answers when an envelope's `payload`
/// fails its shape gate (`remoteRequest` in `api/gateway/src/index.ts`, which
/// throws a bare `Error` that `rpcFailure` reports as `gateway/internal`).
pub const PAYLOAD_SHAPE_MESSAGE: &str =
    "Remote payload must contain exactly one plain-object args field";

/// The payload-shape gate the control applies in `remoteRequest`, *before*
/// the descriptor is resolved: `payload` must be a plain object whose only
/// key is `args`, itself a plain object. The control does not distinguish the
/// six failing conditions (payload not an object, extra keys, `args` absent,
/// `args` null, `args` not an object) — one `||` chain, one message — so this
/// gate does not either.
///
/// Run before the registry lookup, as the control does: the shape of the
/// envelope is refused even for a namespace this host does not serve.
pub fn payload_shape_ok(payload: &serde_json::Value) -> bool {
    let Some(obj) = payload.as_object() else {
        return false;
    };
    obj.len() == 1 && obj.get("args").is_some_and(Value::is_object)
}

/// Why an argument list was refused, as the code plus the field to name.
///
/// The two codes are the control's; keeping them distinct matters because a
/// client can act differently on "you sent the wrong field names" than on
/// "that field's value is malformed".
#[derive(Debug, PartialEq, Eq)]
pub struct Rejection {
    /// `gateway/arguments-invalid` or `gateway/input-invalid`.
    pub code: &'static str,
    /// The `namespace/method` the call named, for the message and `details`.
    pub endpoint: String,
    /// The wire name to report. For a missing arg this is the absent name; for
    /// a bad value it is the arg that failed.
    pub field: String,
    /// `"missing"` or `"unexpected"` for the arguments layer; unused otherwise.
    pub kind: Option<&'static str>,
}

/// Check `args` against the spec'd descriptor for `namespace/method`.
///
/// Returns `None` when the call may proceed. An endpoint the spec does not
/// declare returns `None` too: this module's job is to enforce the spec, not to
/// decide which endpoints exist — an unknown method is the machine's own
/// `gateway/bad-request`, and an unknown namespace is the registry's.
pub fn check(namespace: &str, method: &str, args: &serde_json::Value) -> Option<Rejection> {
    let spec = endpoint(namespace, method)?;
    let Some(obj) = args.as_object() else {
        // The payload-shape gate in `main.rs` (`payload_shape_ok`) refuses a
        // non-object `args` before this module runs, so this is unreachable
        // from the wire. A direct caller gets no opinion rather than a wrong
        // one: shape is the gate's layer, not this one's.
        return None;
    };

    // Layer 1a: every required arg is present.
    for arg in spec.args.iter().filter(|a| a.required) {
        if !obj.contains_key(arg.wire) {
            return Some(Rejection {
                code: "gateway/arguments-invalid",
                endpoint: format!("{namespace}/{method}"),
                field: arg.wire.to_string(),
                kind: Some("missing"),
            });
        }
    }
    // Layer 1b: no arg the descriptor does not declare.
    if let Some(extra) = obj
        .keys()
        .find(|k| !spec.args.iter().any(|a| a.wire == k.as_str()))
    {
        return Some(Rejection {
            code: "gateway/arguments-invalid",
            endpoint: format!("{namespace}/{method}"),
            field: extra.clone(),
            kind: Some("unexpected"),
        });
    }

    // Layer 2: each present arg's value satisfies its codec's schema.
    for arg in spec.args {
        let Some(value) = obj.get(arg.wire) else {
            continue;
        };
        if !satisfies_str(value, arg.schema) {
            return Some(Rejection {
                code: "gateway/input-invalid",
                endpoint: format!("{namespace}/{method}"),
                field: arg.wire.to_string(),
                kind: None,
            });
        }
    }
    None
}

/// Whether `value` satisfies an arg's pruned JSON Schema, given as text.
///
/// An empty string means the spec declared no constraint for this arg (the
/// generator emits `""` for a schema with nothing left after pruning), which
/// admits any value. The schema text is generated, so a parse failure is a
/// build-time bug, not a runtime condition: it is treated as "no constraint"
/// rather than panicking in the request path.
fn satisfies_str(value: &Value, schema: &str) -> bool {
    if schema.is_empty() {
        return true;
    }
    match serde_json::from_str::<Value>(schema) {
        Ok(parsed) => {
            let defs = parsed.get("$defs");
            satisfies(value, &parsed, defs)
        }
        Err(_) => true,
    }
}

/// Whether `value` satisfies `schema`, resolving `$ref` against `defs`.
///
/// The walk is recursive and follows `anyOf`/`oneOf`/`allOf`, `properties`,
/// `items`, and `$ref` (including cycles — the recursion is bounded by the
/// *value's* depth, not the schema's, so a self-referential `$ref` terminates).
/// It has no `additionalProperties` rule: nested extras are the control's to
/// tolerate, and the generator drops the keyword so one cannot creep in here.
fn satisfies(value: &Value, schema: &Value, defs: Option<&Value>) -> bool {
    let Some(obj) = schema.as_object() else {
        // A non-object schema (or `true`/`false`) constrains nothing we model.
        return true;
    };

    // `$ref` replaces this schema with the referenced one; `$defs` travel with
    // the top-level schema and are threaded through so a ref can resolve.
    if let Some(r) = obj.get("$ref").and_then(|v| v.as_str()) {
        if let Some(target) = resolve_ref(r, defs) {
            return satisfies(value, target, defs);
        }
        return true;
    }

    // `allOf` is a conjunction: every member must hold.
    if let Some(members) = obj.get("allOf").and_then(|v| v.as_array())
        && !members.iter().all(|m| satisfies(value, m, defs))
    {
        return false;
    }

    // `anyOf` / `oneOf`: at least one member must hold. (The spec uses neither
    // as an exclusive `oneOf`, and the control's observed behavior is
    // at-least-one — an `address` satisfying both branches is accepted.)
    for key in ["anyOf", "oneOf"] {
        if let Some(members) = obj.get(key).and_then(|v| v.as_array())
            && !members.iter().any(|m| satisfies(value, m, defs))
        {
            return false;
        }
    }

    if let Some(t) = obj.get("type").and_then(|v| v.as_str())
        && !matches_type(value, t)
    {
        return false;
    }

    if let Some(c) = obj.get("const")
        && value != c
    {
        return false;
    }

    if let Some(allowed) = obj.get("enum").and_then(|v| v.as_array())
        && !allowed.iter().any(|a| a == value)
    {
        return false;
    }

    // Object members: every required name present, every named present member
    // satisfying its own sub-schema.
    if let Some(props) = obj.get("properties").and_then(|v| v.as_object())
        && let Some(vobj) = value.as_object()
    {
        if let Some(required) = obj.get("required").and_then(|v| v.as_array()) {
            for name in required.iter().filter_map(|v| v.as_str()) {
                if !vobj.contains_key(name) {
                    return false;
                }
            }
        }
        for (name, sub) in props {
            if let Some(member) = vobj.get(name)
                && !satisfies(member, sub, defs)
            {
                return false;
            }
        }
    }

    // Array items: every element satisfies the item schema.
    if let Some(items) = obj.get("items")
        && let Some(arr) = value.as_array()
        && !arr.iter().all(|el| satisfies(el, items, defs))
    {
        return false;
    }

    true
}

/// Resolve a local `#/$defs/<name>` reference against the schema's `$defs`.
fn resolve_ref<'a>(r: &str, defs: Option<&'a Value>) -> Option<&'a Value> {
    let name = r.strip_prefix("#/$defs/")?;
    defs?.as_object()?.get(name)
}

/// Whether a JSON value has the named JSON Schema type.
///
/// `"integer"` accepts any number (the spec emits no separate integer type and
/// JSON has no integer/number distinction at the wire); `"null"` is a real
/// type here because the spec uses `{"type":"null","const":null}` in unions.
/// An unrecognized type (e.g. the extractor's `"unknown"`) constrains nothing.
fn matches_type(value: &Value, t: &str) -> bool {
    match t {
        "string" => value.is_string(),
        "number" | "integer" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

/// Render a [`Rejection`] as the HTTP response the gateway returns.
///
/// The message mirrors the control's wording closely enough to be read the
/// same way, and `details` carries `endpoint` plus — for the value layer — the
/// `field`, which is where the control puts it.
pub fn respond(rpc_id: String, r: &Rejection) -> (axum::http::StatusCode, String) {
    let bytes = vocoder_typert::encode_rpc_server_response(&vocoder_typert::ServerResponse {
        rpc_id,
        result: vocoder_typert::RpcResult::Err {
            ok: vocoder_typert::OkTag(false),
            error: vocoder_typert::RpcError {
                code: r.code.to_string(),
                message: message_for(r),
                details: Some(details_for(r)),
            },
        },
    });
    (
        axum::http::StatusCode::OK,
        String::from_utf8(bytes).expect("encoded response is UTF-8"),
    )
}

/// The human-readable half of a boundary rejection.
pub fn message_for(r: &Rejection) -> String {
    match r.kind {
        Some(kind) => format!(
            "typert gateway: {}: args fields do not match the descriptor: {kind} \"{}\"",
            r.endpoint, r.field
        ),
        None => format!(
            "typert gateway: {}: wire field \"{}\" failed boundary validation",
            r.endpoint, r.field
        ),
    }
}

/// The `details` half, matching the control's shape.
pub fn details_for(r: &Rejection) -> serde_json::Value {
    match r.kind {
        Some(_) => serde_json::json!({ "endpoint": r.endpoint }),
        None => serde_json::json!({ "endpoint": r.endpoint, "field": r.field }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_missing_required_arg_is_arguments_invalid() {
        let r = check("subagents", "list", &json!({})).unwrap();
        assert_eq!(r.code, "gateway/arguments-invalid");
        assert_eq!(r.field, "parentSessionId");
        assert_eq!(r.kind, Some("missing"));
    }

    /// The payload-shape gate: exactly one plain-object `args`, nothing else —
    /// the control's `remoteRequest` conditions, and its single answer for all
    /// of them. `check` never sees these; `main.rs` refuses them first.
    #[test]
    fn the_payload_shape_gate_refuses_what_the_control_refuses() {
        for bad in [
            json!(null),
            json!([]),
            json!({ "args": {}, "extra": true }),
            json!({ "only": true }),
            json!({ "args": null }),
            json!({ "args": [] }),
            json!({ "args": "not-an-object" }),
        ] {
            assert!(!payload_shape_ok(&bad), "{bad}");
        }
        // The valid shapes: present plain-object `args`, empty or not.
        assert!(payload_shape_ok(&json!({ "args": {} })));
        assert!(payload_shape_ok(&json!({ "args": { "x": 1 } })));
    }

    #[test]
    fn an_unexpected_arg_is_arguments_invalid() {
        let r = check(
            "subagents",
            "list",
            &json!({ "parentSessionId": "p", "zzz": 1 }),
        )
        .unwrap();
        assert_eq!(r.code, "gateway/arguments-invalid");
        assert_eq!(r.field, "zzz");
        assert_eq!(r.kind, Some("unexpected"));
    }

    #[test]
    fn a_wrong_typed_arg_is_input_invalid() {
        let r = check("subagents", "list", &json!({ "parentSessionId": 7 })).unwrap();
        assert_eq!(r.code, "gateway/input-invalid");
        assert_eq!(r.field, "parentSessionId");
        assert_eq!(r.kind, None);
    }

    /// The wire name is what is checked, not the source name. `session/list`'s
    /// body arg is spelled `_request`, and passing `request` is exactly the
    /// mistake the control caught once already — so the *reported* field is
    /// the wire name the caller should have used.
    ///
    /// Passing `request` produces a **missing** `_request`, not an unexpected
    /// `request`: the control reports the absent required arg first, and this
    /// module follows that order.
    #[test]
    fn the_wire_name_is_what_counts() {
        let ok = check("session", "list", &json!({ "_request": {} }));
        assert!(ok.is_none(), "{ok:?}");
        let bad = check("session", "list", &json!({ "request": {} })).unwrap();
        assert_eq!(bad.code, "gateway/arguments-invalid");
        assert_eq!(bad.field, "_request");
        assert_eq!(bad.kind, Some("missing"));
    }

    /// An endpoint that takes no args accepts an empty object — and rejects an
    /// invented arg, which is what the control does.
    #[test]
    fn a_no_arg_endpoint_rejects_an_invented_arg() {
        assert!(check("session", "modelCatalog", &json!({})).is_none());
        let r = check("session", "modelCatalog", &json!({ "sessionId": "x" })).unwrap();
        assert_eq!(r.code, "gateway/arguments-invalid");
        assert_eq!(r.field, "sessionId");
    }

    /// A `const` argument is enforced at the value layer.
    #[test]
    fn a_const_arg_must_equal_its_const() {
        let ok = check(
            "subagents",
            "interruptByParent",
            &json!({
                "parentSessionId": "p",
                "childSessionId": "c",
                "mode": "continuable",
            }),
        );
        // `parentSessionId`/`childSessionId` are non-empty elsewhere; the shape
        // layer only sees strings, so this passes the boundary.
        assert!(ok.is_none(), "{ok:?}");

        let r = check(
            "subagents",
            "interruptByParent",
            &json!({
                "parentSessionId": "p",
                "childSessionId": "c",
                "mode": "one-shot",
            }),
        )
        .unwrap();
        assert_eq!(r.code, "gateway/input-invalid");
        assert_eq!(r.field, "mode");
    }

    /// An endpoint the spec does not declare is not this module's business:
    /// the machine owns "unknown method", and the registry owns "unknown
    /// namespace".
    #[test]
    fn an_undeclared_endpoint_is_left_to_the_machine() {
        assert!(check("subagents", "noSuchMethod", &json!({})).is_none());
        assert!(check("noSuchNamespace", "list", &json!({})).is_none());
    }

    /// A **nested** extra key is tolerated, given the nested requireds are
    /// present: the control passes an unexpected key below the top level of
    /// `args` straight to domain logic, so the boundary must not reject one.
    #[test]
    fn a_nested_extra_key_is_not_rejected_here() {
        let r = check(
            "subagents",
            "prompt",
            &json!({ "request": {
                "requestId": "r",
                "parentSessionId": "p",
                "childSessionId": "c",
                "mode": "continuable",
                "delivery": "queue",
                "content": [],
                "anything": true,
            } }),
        );
        assert!(r.is_none(), "{r:?}");
    }

    /// A **nested** missing required field is `gateway/input-invalid` naming
    /// the **outer** arg — measured on the control, which answers exactly this
    /// for `session/page`'s `request` without `address`.
    #[test]
    fn a_nested_missing_required_field_is_input_invalid() {
        // `content` is required and absent.
        let r = check(
            "subagents",
            "prompt",
            &json!({ "request": {
                "requestId": "r",
                "parentSessionId": "p",
                "childSessionId": "c",
                "mode": "continuable",
                "delivery": "queue",
            } }),
        )
        .unwrap();
        assert_eq!(r.code, "gateway/input-invalid");
        assert_eq!(r.field, "request");
    }

    /// The walk descends through `anyOf` branches and array `items`: a
    /// `session/prompt` content element missing the discriminator's companion
    /// field is refused, and the outer `request` is what is named.
    #[test]
    fn a_nested_array_item_required_is_enforced() {
        let base = |content: serde_json::Value| {
            json!({ "request": {
                "requestId": "r",
                "sessionId": "s",
                "mode": "queue",
                "content": content,
            } })
        };
        assert!(check("session", "prompt", &base(json!([]))).is_none());
        let r = check("session", "prompt", &base(json!([{ "type": "text" }]))).unwrap();
        assert_eq!(r.code, "gateway/input-invalid");
        assert_eq!(r.field, "request");
    }

    /// A nested `const` is enforced: `session/prompt`'s `mode` must be `queue`
    /// or `steer`, and an unknown value is refused at the boundary.
    #[test]
    fn a_nested_const_is_enforced() {
        let r = check(
            "session",
            "prompt",
            &json!({ "request": {
                "requestId": "r",
                "sessionId": "s",
                "mode": "run",
                "content": [],
            } }),
        )
        .unwrap();
        assert_eq!(r.code, "gateway/input-invalid");
        assert_eq!(r.field, "request");
    }

    /// `allOf` is a conjunction, as `commands/execute`'s image branch depends
    /// on: the branch is two `allOf` members whose requireds must all hold.
    #[test]
    fn an_all_of_branch_requires_every_member() {
        let with = |att: serde_json::Value| json!({ "agentId": "a", "line": "/x", "submittedAttachments": [att] });
        // Missing the second member's requireds (`mediaType`, `data`).
        let r = check("commands", "execute", &with(json!({ "type": "image" }))).unwrap();
        assert_eq!(r.code, "gateway/input-invalid");
        assert_eq!(r.field, "submittedAttachments");

        // Both `allOf` members satisfied.
        let ok = check(
            "commands",
            "execute",
            &with(json!({ "type": "image", "mediaType": "image/png", "data": "aGk=" })),
        );
        assert!(ok.is_none(), "{ok:?}");
    }

    /// A `$ref` is followed, including a self-referential one: the recursion is
    /// bounded by the value's depth, so a cyclic `#/$defs/__schema0` terminates.
    #[test]
    fn a_self_referential_ref_terminates_and_validates() {
        // `dynamicCordisRunner/invoke`'s `args` is an object whose values
        // recurse through `$defs/__schema0` (an anyOf that can nest).
        let ok = check(
            "dynamicCordisRunner",
            "invoke",
            &json!({ "pluginId": "p", "pluginRunId": "r", "method": "m", "args": { "a": { "b": 1 } } }),
        );
        assert!(ok.is_none(), "{ok:?}");
    }
}
