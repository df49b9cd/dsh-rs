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
//! boundary and answers three distinct codes, which were read off the running
//! control rather than inferred from the schema:
//!
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
//! **What it does not do.** It does not validate nested object members. The
//! control rejects an unexpected key at the top level of `args` but passes one
//! *inside* a nested object straight through to domain logic, so validating
//! deeper would make this host stricter than the one it mirrors.

use vocoder_spec_api::validate::{WireShape, endpoint};

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
    let obj = args.as_object();

    // Layer 1a: every required arg is present.
    if let Some(obj) = obj {
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
    } else if spec.args.iter().any(|a| a.required) {
        // A non-object `args` cannot satisfy a descriptor that requires args.
        // An endpoint with no args accepts anything here, which matches the
        // control: it validates the shape of what it is given, and there is
        // nothing to check.
        let first = spec.args.iter().find(|a| a.required).unwrap();
        return Some(Rejection {
            code: "gateway/arguments-invalid",
            endpoint: format!("{namespace}/{method}"),
            field: first.wire.to_string(),
            kind: Some("missing"),
        });
    }

    // Layer 2: each present arg's value satisfies its codec's schema.
    if let Some(obj) = obj {
        for arg in spec.args {
            let Some(value) = obj.get(arg.wire) else {
                continue;
            };
            if !satisfies(value, arg.shape) {
                return Some(Rejection {
                    code: "gateway/input-invalid",
                    endpoint: format!("{namespace}/{method}"),
                    field: arg.wire.to_string(),
                    kind: None,
                });
            }
        }
    }
    None
}

/// Whether `value` satisfies `shape`.
///
/// Only the *top level* of the value is examined. A nested object is checked
/// for being an object, not for its members — the control tolerates an
/// unexpected nested key, so descending would diverge.
fn satisfies(value: &serde_json::Value, shape: WireShape) -> bool {
    match shape {
        WireShape::Any => true,
        WireShape::String => value.is_string(),
        WireShape::Number => value.is_number(),
        WireShape::Boolean => value.is_boolean(),
        WireShape::Array => value.is_array(),
        WireShape::Object => value.is_object(),
        WireShape::Const(expected) => value.as_str() == Some(expected),
        WireShape::Enum(allowed) => value.as_str().is_some_and(|s| allowed.contains(&s)),
        WireShape::Union(options) => options.iter().any(|s| satisfies(value, *s)),
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

    /// A nested object is checked for being an object, not for its members:
    /// the control tolerates an unexpected key inside a nested arg.
    #[test]
    fn a_nested_extra_key_is_not_rejected_here() {
        let r = check(
            "subagents",
            "prompt",
            &json!({ "request": { "anything": true } }),
        );
        assert!(r.is_none(), "{r:?}");
    }
}
