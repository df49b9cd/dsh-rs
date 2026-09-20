//! The `dynamicCordisRunner` namespace: dynamic (define/run) Cordis plugins.
//!
//! Upstream (`dsh/packages/extensions/cordis-host-runner/src/index.ts`) owns a
//! registry of *dynamically defined* plugins — plugin packages a session
//! authors and runs at runtime, the machinery behind the Cordis panel. The
//! namespace has twelve endpoints: the two the GUI calls at boot
//! (`inventory`, `syncInspectManifest`), and ten that address one defined
//! plugin by id.
//!
//! **vocoderd defines no dynamic plugins, and that is the faithful answer, not
//! a stub.** The registry is empty here by construction: there is no Cordis
//! panel, no runtime plugin authoring, and no machinery that could define one.
//! So `inventory` is `[]` and `syncInspectManifest` is `null` — both *exactly*
//! what the control returns when its own registry is empty, which is the state
//! a fresh control host boots in too (verified: the live control answers
//! `{"apps":[]}`-equivalent `[]` and `null` for both before any plugin is
//! defined).
//!
//! **The ten plugin-addressed endpoints refuse with the control's own shape.**
//! A call naming a plugin this host does not hold answers the message the
//! control answers for an unknown id — `no dynamic plugin "<id>" in this
//! process — it may have been removed or lost on DSH restart` — in whichever
//! envelope that endpoint declares: some return it as a top-level error
//! (`gateway/internal`, as `getClientCode` does), most as a success value
//! carrying `{ok: false, ...}` (`invoke`'s `plugin-not-running`, the panel
//! verbs' `plugin-missing`). The distinction is per-endpoint and is taken from
//! the control, not chosen. The two *report* endpoints are the exception: a
//! render or guard failure from the client is accepted and dropped
//! (`null`), because it is a client-side diagnostic the host only records — and
//! this host records nothing, so accepting it is the honest behavior.
//!
//! Arg-shape validation is not repeated here: `vocoder-spec-api`'s generated
//! table already refuses a malformed payload at the dispatch boundary, which is
//! where the control does it (the control's `gateway/arguments-invalid` for a
//! wrong field set is that layer, not this machine).

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::rpc;

/// The message the control answers for a plugin id no live registry holds.
/// Copied verbatim: it is what the client's panel renders, and a paraphrase
/// would change the text a user reads without changing anything testable here.
const NO_SUCH_PLUGIN: &str =
    "no dynamic plugin \"{id}\" in this process — it may have been removed or lost on DSH restart";

pub struct DynamicCordisRunnerMachine;

impl DynamicCordisRunnerMachine {
    pub fn new() -> Self {
        Self
    }

    /// The "no such plugin" message for one id.
    fn missing(id: &str) -> String {
        NO_SUCH_PLUGIN.replace("{id}", id)
    }
}

impl Default for DynamicCordisRunnerMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginMachine for DynamicCordisRunnerMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("dynamicCordisRunner") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let args = payload
            .get("args")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let arg = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or_default();

        match method {
            // The registry is empty, so the frame-wide inventory is empty and
            // the client inspect manifest has nothing to sync. Both match the
            // control's own empty-registry answer.
            "inventory" => rpc::ok(serde_json::json!([])),
            "syncInspectManifest" => rpc::ok(serde_json::Value::Null),

            // A client-side render/guard failure is recorded, not refused; this
            // host keeps no such record, so it accepts and drops — the shape
            // the result schema declares.
            "reportRenderFailure" | "reportClientGuardFailure" => rpc::ok(serde_json::Value::Null),

            // The two verbs that answer a *top-level* error for an unknown
            // plugin. `getClientCode` resolves the session first (the control
            // answers `session/not-found` for an unknown agent, which the session
            // machine owns); with a live agent and an unknown plugin it answers
            // `gateway/internal` carrying the message.
            "getClientCode" => rpc::err("gateway/internal", Self::missing(arg("pluginId"))),

            // `invoke` answers a business result: `{ok: false, code:
            // 'plugin-not-running', message}`.
            "invoke" => rpc::ok(serde_json::json!({
                "ok": false,
                "code": "plugin-not-running",
                "message": Self::missing(arg("pluginId")),
            })),

            // The panel verbs (`stopFromPanel`, `undefineFromPanel`) answer
            // `{ok: false, reason: 'plugin-missing', message}`.
            "stopFromPanel" | "undefineFromPanel" => rpc::ok(serde_json::json!({
                "ok": false,
                "reason": "plugin-missing",
                "message": Self::missing(arg("pluginId")),
            })),

            // `runHostHalf` and `settleUserRun` answer a business result without
            // the reason discriminant: `{ok: false, message}`.
            "runHostHalf" | "settleUserRun" => rpc::ok(serde_json::json!({
                "ok": false,
                "message": Self::missing(arg("pluginId")),
            })),

            // The two query-resolution endpoints answer an ack
            // (`{accepted}`), not an error, when nothing is waiting. With no
            // dynamic plugin there is never a pending query, so nothing is
            // accepted.
            "resolveInspectQuery" | "resolveRequestRun" => {
                rpc::ok(serde_json::json!({ "accepted": false }))
            }

            other => rpc::err(
                "gateway/bad-request",
                format!("unsupported dynamicCordisRunner method: {other}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(
        m: &mut DynamicCordisRunnerMachine,
        method: &str,
        args: serde_json::Value,
    ) -> serde_json::Value {
        m.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("dynamicCordisRunner")),
            payload: serde_json::json!({ "method": method, "args": args }),
        })
        .iter()
        .find_map(|o| match o {
            MachineOut::Reply(r) => Some(r.to_wire_json()),
            _ => None,
        })
        .unwrap()
    }

    #[test]
    fn an_empty_registry_answers_the_two_boot_endpoints() {
        let mut m = DynamicCordisRunnerMachine::new();
        // `inventory` is the empty array; the control's empty-registry answer.
        assert_eq!(
            call(&mut m, "inventory", serde_json::json!({})),
            serde_json::json!({ "ok": true, "value": [] })
        );
        // `syncInspectManifest` accepts a manifest and returns null.
        assert_eq!(
            call(
                &mut m,
                "syncInspectManifest",
                serde_json::json!({ "providers": [] })
            ),
            serde_json::json!({ "ok": true, "value": null })
        );
    }

    #[test]
    fn a_named_plugin_that_does_not_exist_refuses_in_the_endpoints_own_envelope() {
        let mut m = DynamicCordisRunnerMachine::new();
        // A top-level error, as the control answers `getClientCode`.
        let v = call(
            &mut m,
            "getClientCode",
            serde_json::json!({ "pluginId": "nope" }),
        );
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "gateway/internal");
        assert!(v["error"]["message"].as_str().unwrap().contains("\"nope\""));
        // A business result, as the control answers `invoke`.
        let v = call(&mut m, "invoke", serde_json::json!({ "pluginId": "nope" }));
        assert_eq!(v["value"]["ok"], false);
        assert_eq!(v["value"]["code"], "plugin-not-running");
        // The panel verbs carry the `reason` discriminant.
        let v = call(
            &mut m,
            "stopFromPanel",
            serde_json::json!({ "pluginId": "nope" }),
        );
        assert_eq!(v["value"]["reason"], "plugin-missing");
        let v = call(
            &mut m,
            "undefineFromPanel",
            serde_json::json!({ "pluginId": "nope" }),
        );
        assert_eq!(v["value"]["reason"], "plugin-missing");
    }

    #[test]
    fn a_report_is_accepted_and_dropped() {
        // The client's render/guard failure reports are diagnostics; with no
        // record to keep, accepting them is the honest answer.
        let mut m = DynamicCordisRunnerMachine::new();
        for method in ["reportRenderFailure", "reportClientGuardFailure"] {
            let v = call(&mut m, method, serde_json::json!({ "pluginId": "nope" }));
            assert_eq!(
                v,
                serde_json::json!({ "ok": true, "value": null }),
                "{method}"
            );
        }
    }

    #[test]
    fn a_query_with_nothing_waiting_is_not_accepted() {
        let mut m = DynamicCordisRunnerMachine::new();
        for method in ["resolveInspectQuery", "resolveRequestRun"] {
            let v = call(&mut m, method, serde_json::json!({ "requestId": "nope" }));
            assert_eq!(v["value"]["accepted"], false, "{method}");
        }
    }

    #[test]
    fn an_unknown_method_is_a_bad_request() {
        let mut m = DynamicCordisRunnerMachine::new();
        let v = call(&mut m, "notAMethod", serde_json::json!({}));
        assert_eq!(v["error"]["code"], "gateway/bad-request");
    }
}
