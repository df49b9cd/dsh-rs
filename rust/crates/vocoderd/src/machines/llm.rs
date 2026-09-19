//! The `llm` namespace: the model registry a client reads to populate its model
//! picker, plus the catalog the session machine serves.
//!
//! Upstream (`dsh/packages/llm/llm/src/index.ts`) is an `LlmRuntime` service
//! holding an adapter registry. Provider plugins call `registerAdapter([id], …)`
//! to claim a route and `registerConfigurableProviders(...)` to declare routes
//! they *could* activate from configuration. Three remotes read that registry:
//!
//! - `listProviders` — routes with a live adapter.
//! - `listConfigurableProviders` — every declarable route, with the settings
//!   namespace and path a client would write to activate it.
//! - `discoverModels` — interrogates a draft provider for its model list.
//!
//! **What vocoderd composes.** This host ships one provider route,
//! `deepseek-official`, backed by the same model catalog upstream's
//! `llm-deepseek` plugin declares (four models, `deepseek-flash` as the
//! default). That is the whole registry: there is no `llm-pi-ai` plugin here,
//! so the 39 routes that plugin contributes are absent rather than listed as
//! unavailable. Listing them would tell a settings panel that this host can
//! activate providers it has no adapter for.
//!
//! **The catalog is data, not a network call.** `listModels` upstream resolves
//! against the adapter's static declaration (a config override or
//! `DEFAULT_MODELS`), so reading it costs no I/O. That is why this machine has
//! no effects and no read cache: it answers from constants, which is also what
//! makes it safe on the startup path — the client calls `session/modelCatalog`
//! during cold boot.
//!
//! **`discoverModels` is a real network call upstream** and therefore cannot be
//! answered here: vocoderd has no model-discovery registration. It fails with
//! the same typed error the control produces for an unregistered namespace
//! rather than returning an empty list, because "this host cannot do that" and
//! "that provider has no models" are different answers.

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::rpc;

/// The one provider route this host composes.
pub const PROVIDER_ID: &str = "deepseek-official";
/// Its display name.
pub const PROVIDER_NAME: &str = "DeepSeek";
/// The settings namespace that configures it.
pub const PROVIDER_SETTINGS_NS: &str = "llm-deepseek";

/// One reasoning effort a model accepts.
struct Effort {
    id: &'static str,
    name: &'static str,
    description: &'static str,
}

/// The four efforts every DeepSeek model here declares, in presentation order.
const EFFORTS: &[Effort] = &[
    Effort {
        id: "off",
        name: "Off",
        description: "Use for simple tasks that do not need reasoning.",
    },
    Effort {
        id: "low",
        name: "Low",
        description: "Prefer for routine or latency-sensitive tasks.",
    },
    Effort {
        id: "high",
        name: "High",
        description: "The default balance for most tasks.",
    },
    Effort {
        id: "max",
        name: "Max",
        description: "Reserve for the hardest quality-first tasks.",
    },
];

/// One catalog model, mirroring `llm-deepseek`'s `DEFAULT_MODELS`.
struct Model {
    id: &'static str,
    name: &'static str,
    description: Option<&'static str>,
}

/// The provider's model list, in declaration order.
///
/// Order is load-bearing: the picker renders it as given, and the client's
/// default (`deepseek-flash`) is the first entry.
const MODELS: &[Model] = &[
    Model {
        id: "deepseek-flash",
        name: "DeepSeek-V41-Flash",
        description: None,
    },
    Model {
        id: "deepseek-v4-flash",
        name: "DeepSeek-V4-Flash",
        description: Some(
            "Fast, efficient, and economical; suited to focused, routine, or parallel tasks.",
        ),
    },
    Model {
        id: "deepseek-v4-pro",
        name: "DeepSeek-V4-Pro",
        description: Some(
            "Stronger agentic coding, knowledge, and difficult reasoning; suited to complex or quality-critical tasks at higher cost.",
        ),
    },
    Model {
        id: "deepseek-v4-flash-vision-exp",
        name: "DeepSeek-V4-Flash-Vision-Exp",
        description: None,
    },
];

/// The model a session uses before it selects one.
pub const DEFAULT_MODEL: &str = "deepseek-flash";

/// The `ModelCatalog` a client renders, and the shape `session/modelCatalog`
/// serves.
///
/// Shared rather than duplicated so the two endpoints cannot drift: the client
/// calls both during boot and compares.
pub fn model_catalog() -> serde_json::Value {
    let models: Vec<serde_json::Value> = MODELS
        .iter()
        .map(|m| {
            let mut v = serde_json::json!({
                "id": m.id,
                "name": m.name,
                "reasoning": {
                    "defaultEffort": "high",
                    "efforts": EFFORTS
                        .iter()
                        .map(|e| serde_json::json!({
                            "id": e.id,
                            "name": e.name,
                            "description": e.description,
                        }))
                        .collect::<Vec<_>>(),
                },
            });
            if let Some(description) = m.description {
                v["description"] = description.into();
            }
            v
        })
        .collect();
    serde_json::json!({
        "default": { "provider": PROVIDER_ID, "model": DEFAULT_MODEL },
        "routableProviders": [PROVIDER_ID],
        "groups": [{
            "id": PROVIDER_ID,
            "name": PROVIDER_NAME,
            "models": models,
        }],
        // No route failed to enumerate: the one provider answers from a static
        // declaration, so there is nothing that can fail per-provider.
        "failures": [],
    })
}

pub struct LlmMachine;

impl PluginMachine for LlmMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("llm") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        match method {
            "listProviders" => rpc::ok(serde_json::json!([{
                "id": PROVIDER_ID,
                "name": PROVIDER_NAME,
            }])),
            "listConfigurableProviders" => rpc::ok(serde_json::json!([{
                "provider": PROVIDER_ID,
                "displayName": PROVIDER_NAME,
                "settingsNs": PROVIDER_SETTINGS_NS,
                // Empty: this provider's config *is* the namespace root, so a
                // client writes its fields directly rather than under a
                // sub-path. The `llm-pi-ai` routes upstream use a path because
                // one namespace holds many providers.
                "settingsPath": [],
            }])),
            "discoverModels" => {
                // Upstream interrogates a *draft* provider over the network.
                // This host registers no discovery, and the control answers the
                // same way for an unregistered namespace.
                let args = payload.get("args").cloned().unwrap_or_default();
                let settings_ns = rpc::arg_str(&args, "settingsNs").unwrap_or_default();
                rpc::err_details(
                    "llm/model-discovery-rejected",
                    format!("no model discovery is registered for \"{settings_ns}\""),
                    serde_json::json!({ "settingsNs": settings_ns }),
                )
            }
            other => rpc::err(
                "gateway/bad-request",
                format!("unsupported llm method: {other}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(m: &mut LlmMachine, method: &str, args: serde_json::Value) -> serde_json::Value {
        m.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("llm")),
            payload: serde_json::json!({ "method": method, "args": args }),
        })
        .iter()
        .find_map(|o| match o {
            MachineOut::Reply(r) => Some(r.to_wire_json()),
            _ => None,
        })
        .expect("a reply")
    }

    #[test]
    fn the_catalog_names_its_one_provider_and_default_model() {
        let catalog = model_catalog();
        assert_eq!(catalog["default"]["provider"], PROVIDER_ID);
        assert_eq!(catalog["default"]["model"], DEFAULT_MODEL);
        assert_eq!(
            catalog["routableProviders"],
            serde_json::json!([PROVIDER_ID])
        );
        assert_eq!(catalog["failures"], serde_json::json!([]));

        let groups = catalog["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        let models = groups[0]["models"].as_array().unwrap();
        assert_eq!(models.len(), MODELS.len());
        // The default model is present and first, which is what the picker's
        // initial selection depends on.
        assert_eq!(models[0]["id"], DEFAULT_MODEL);
        // Every model carries the effort vocabulary the picker renders.
        for m in models {
            assert_eq!(m["reasoning"]["efforts"].as_array().unwrap().len(), 4);
            assert_eq!(m["reasoning"]["defaultEffort"], "high");
        }
    }

    /// A description is omitted rather than sent empty, matching the schema's
    /// optional field and the control's own output.
    #[test]
    fn models_without_a_description_omit_the_field() {
        let catalog = model_catalog();
        let models = catalog["groups"][0]["models"].as_array().unwrap();
        let flash = models.iter().find(|m| m["id"] == "deepseek-flash").unwrap();
        assert!(flash.get("description").is_none(), "{flash}");
        let pro = models
            .iter()
            .find(|m| m["id"] == "deepseek-v4-pro")
            .unwrap();
        assert!(pro["description"].is_string(), "{pro}");
    }

    #[test]
    fn providers_and_configurable_providers_agree_on_the_route() {
        let mut m = LlmMachine;
        let providers = call(&mut m, "listProviders", serde_json::json!({}));
        let providers = providers["value"].as_array().unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["id"], PROVIDER_ID);
        assert_eq!(providers[0]["name"], PROVIDER_NAME);

        let configurable = call(&mut m, "listConfigurableProviders", serde_json::json!({}));
        let configurable = configurable["value"].as_array().unwrap();
        assert_eq!(configurable.len(), 1);
        assert_eq!(configurable[0]["provider"], PROVIDER_ID);
        assert_eq!(configurable[0]["settingsNs"], PROVIDER_SETTINGS_NS);
        assert_eq!(configurable[0]["settingsPath"], serde_json::json!([]));
    }

    /// Discovery is refused with the typed error the control produces, not an
    /// empty list: "cannot discover here" and "no models" differ.
    #[test]
    fn discover_models_is_refused_typed() {
        let mut m = LlmMachine;
        let v = call(
            &mut m,
            "discoverModels",
            serde_json::json!({"settingsNs": "llm-deepseek", "request": {}}),
        );
        assert_eq!(v["error"]["code"], "llm/model-discovery-rejected", "{v}");
        assert_eq!(v["error"]["details"]["settingsNs"], "llm-deepseek", "{v}");
    }

    #[test]
    fn unknown_method_is_a_typed_bad_request() {
        let mut m = LlmMachine;
        let v = call(&mut m, "nope", serde_json::json!({}));
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
    }
}
