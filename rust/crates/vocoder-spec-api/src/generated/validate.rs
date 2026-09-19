// GENERATED from spec/typert/remote.json — do not edit.
//! Per-endpoint argument descriptors, used by the dispatch boundary.
//!
//! See `vocoderd/src/validate.rs` for how these are applied.
#![allow(clippy::all)]

/// The shape a wire value must have to satisfy its codec's schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireShape {
    /// Any JSON value (`unknown` / no `type`).
    Any,
    String,
    Number,
    Boolean,
    Array,
    Object,
    /// `const` — the one value it may take.
    Const(&'static str),
    /// An `enum` of string values.
    Enum(&'static [&'static str]),
    /// An `anyOf`/`oneOf` union; the value must satisfy at least one.
    Union(&'static [WireShape]),
}

/// One wire argument of one endpoint.
#[derive(Debug, Clone, Copy)]
pub struct ArgSpec {
    /// The argument's **wire** name (not always its source name).
    pub wire: &'static str,
    pub required: bool,
    pub shape: WireShape,
}

/// One endpoint's argument list.
#[derive(Debug, Clone, Copy)]
pub struct EndpointSpec {
    pub namespace: &'static str,
    pub method: &'static str,
    pub args: &'static [ArgSpec],
}

// Shape literals (referenced by the table below).
const C0: WireShape = WireShape::Union(&[
    WireShape::Any,
    WireShape::String,
    WireShape::Number,
    WireShape::Boolean,
    WireShape::Boolean,
    WireShape::Array,
    WireShape::Object,
]);
const C1: WireShape = WireShape::Union(&[WireShape::Object, WireShape::Object]);
const C2: WireShape = WireShape::Union(&[WireShape::Object, WireShape::Object]);
const C3: WireShape = WireShape::Const("run");
const C4: WireShape = WireShape::Const("update");
const C5: WireShape = WireShape::Union(&[/*shape*/ C3, /*shape*/ C4]);
const C6: WireShape = WireShape::Union(&[WireShape::Any, WireShape::String]);
const C7: WireShape = WireShape::Union(&[WireShape::Object, WireShape::Object]);
const C8: WireShape = WireShape::Const("continuable");

/// Every endpoint the spec declares, sorted by namespace then method.
pub static ENDPOINTS: &[EndpointSpec] = &[
    EndpointSpec {
        namespace: "agentPresets",
        method: "copy",
        args: &[
            ArgSpec {
                wire: "from",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "id",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "name",
                required: false,
                shape: WireShape::Any,
            },
        ],
    },
    EndpointSpec {
        namespace: "agentPresets",
        method: "deletePreset",
        args: &[ArgSpec {
            wire: "id",
            required: true,
            shape: WireShape::String,
        }],
    },
    EndpointSpec {
        namespace: "agentPresets",
        method: "list",
        args: &[],
    },
    EndpointSpec {
        namespace: "agentPresets",
        method: "read",
        args: &[ArgSpec {
            wire: "agentPreset",
            required: true,
            shape: WireShape::String,
        }],
    },
    EndpointSpec {
        namespace: "agentPresets",
        method: "select",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "agentPreset",
                required: true,
                shape: WireShape::String,
            },
        ],
    },
    EndpointSpec {
        namespace: "agentTeams",
        method: "createTask",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "request",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "agentTeams",
        method: "updateTask",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "request",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "agentTeams",
        method: "view",
        args: &[ArgSpec {
            wire: "agentId",
            required: true,
            shape: WireShape::String,
        }],
    },
    EndpointSpec {
        namespace: "commands",
        method: "execute",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "line",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "submittedAttachments",
                required: true,
                shape: WireShape::Array,
            },
        ],
    },
    EndpointSpec {
        namespace: "commands",
        method: "list",
        args: &[ArgSpec {
            wire: "agentId",
            required: true,
            shape: WireShape::String,
        }],
    },
    EndpointSpec {
        namespace: "credentials",
        method: "describe",
        args: &[ArgSpec {
            wire: "refs",
            required: true,
            shape: WireShape::Array,
        }],
    },
    EndpointSpec {
        namespace: "credentials",
        method: "set",
        args: &[
            ArgSpec {
                wire: "ref",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "value",
                required: true,
                shape: WireShape::String,
            },
        ],
    },
    EndpointSpec {
        namespace: "credentials",
        method: "unset",
        args: &[ArgSpec {
            wire: "ref",
            required: true,
            shape: WireShape::String,
        }],
    },
    EndpointSpec {
        namespace: "directoryPicker",
        method: "createDirectory",
        args: &[
            ArgSpec {
                wire: "path",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "name",
                required: true,
                shape: WireShape::String,
            },
        ],
    },
    EndpointSpec {
        namespace: "directoryPicker",
        method: "list",
        args: &[ArgSpec {
            wire: "path",
            required: false,
            shape: WireShape::Any,
        }],
    },
    EndpointSpec {
        namespace: "directoryPicker",
        method: "pick",
        args: &[],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "getClientCode",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "pluginRunId",
                required: true,
                shape: WireShape::String,
            },
        ],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "inventory",
        args: &[],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "invoke",
        args: &[
            ArgSpec {
                wire: "pluginId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "pluginRunId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "method",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "args",
                required: true,
                shape: C0,
            },
        ],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "reportClientGuardFailure",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "pluginRunId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "failure",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "reportRenderFailure",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "pluginRunId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "failure",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "resolveInspectQuery",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "requestId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "resolution",
                required: true,
                shape: C1,
            },
        ],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "resolveRequestRun",
        args: &[
            ArgSpec {
                wire: "requestId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "resolution",
                required: true,
                shape: C2,
            },
        ],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "runHostHalf",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "packageId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "mode",
                required: true,
                shape: C5,
            },
            ArgSpec {
                wire: "requestId",
                required: true,
                shape: C6,
            },
            ArgSpec {
                wire: "approveFutureVersions",
                required: true,
                shape: WireShape::Boolean,
            },
        ],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "settleUserRun",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "resolution",
                required: true,
                shape: C7,
            },
        ],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "stopFromPanel",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                shape: WireShape::String,
            },
        ],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "syncInspectManifest",
        args: &[ArgSpec {
            wire: "providers",
            required: true,
            shape: WireShape::Array,
        }],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "undefineFromPanel",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                shape: WireShape::String,
            },
        ],
    },
    EndpointSpec {
        namespace: "fileReferences",
        method: "list",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "query",
                required: true,
                shape: WireShape::String,
            },
        ],
    },
    EndpointSpec {
        namespace: "fileUploads",
        method: "upload",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "request",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "goals",
        method: "clear",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "ref",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "goals",
        method: "complete",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "ref",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "goals",
        method: "create",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "request",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "goals",
        method: "edit",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "ref",
                required: true,
                shape: WireShape::Object,
            },
            ArgSpec {
                wire: "request",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "goals",
        method: "get",
        args: &[ArgSpec {
            wire: "agentId",
            required: true,
            shape: WireShape::String,
        }],
    },
    EndpointSpec {
        namespace: "goals",
        method: "pause",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "ref",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "goals",
        method: "resume",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "ref",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "llm",
        method: "discoverModels",
        args: &[
            ArgSpec {
                wire: "settingsNs",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "request",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "llm",
        method: "listConfigurableProviders",
        args: &[],
    },
    EndpointSpec {
        namespace: "llm",
        method: "listProviders",
        args: &[],
    },
    EndpointSpec {
        namespace: "messageFeedback",
        method: "delete",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "messageFeedback",
        method: "list",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "messageFeedback",
        method: "put",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "pluginInventory",
        method: "list",
        args: &[],
    },
    EndpointSpec {
        namespace: "session",
        method: "attachment",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "canOpenWorkspacePath",
        args: &[],
    },
    EndpointSpec {
        namespace: "session",
        method: "cancel",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "control",
        args: &[],
    },
    EndpointSpec {
        namespace: "session",
        method: "create",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "follow",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "fork",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "list",
        args: &[ArgSpec {
            wire: "_request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "modelCatalog",
        args: &[],
    },
    EndpointSpec {
        namespace: "session",
        method: "openWorkspacePath",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "page",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "prompt",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "rename",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "search",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "selectModel",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "updateQueue",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "sessionFeedback",
        method: "record",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "sessionReferenceResolver",
        method: "candidates",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "query",
                required: true,
                shape: WireShape::String,
            },
        ],
    },
    EndpointSpec {
        namespace: "settings",
        method: "canOpenAgentPresetDirectory",
        args: &[],
    },
    EndpointSpec {
        namespace: "settings",
        method: "describe",
        args: &[],
    },
    EndpointSpec {
        namespace: "settings",
        method: "mutate",
        args: &[
            ArgSpec {
                wire: "ns",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "ops",
                required: true,
                shape: WireShape::Array,
            },
            ArgSpec {
                wire: "expectedRevision",
                required: false,
                shape: WireShape::Any,
            },
        ],
    },
    EndpointSpec {
        namespace: "settings",
        method: "openAgentPresetDirectory",
        args: &[ArgSpec {
            wire: "agentPreset",
            required: true,
            shape: WireShape::String,
        }],
    },
    EndpointSpec {
        namespace: "settings",
        method: "openSettingsDocument",
        args: &[],
    },
    EndpointSpec {
        namespace: "settings",
        method: "replace",
        args: &[
            ArgSpec {
                wire: "ns",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "section",
                required: true,
                shape: WireShape::Object,
            },
            ArgSpec {
                wire: "expectedRevision",
                required: false,
                shape: WireShape::Any,
            },
        ],
    },
    EndpointSpec {
        namespace: "settings",
        method: "update",
        args: &[
            ArgSpec {
                wire: "ns",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "patch",
                required: true,
                shape: WireShape::Object,
            },
            ArgSpec {
                wire: "expectedRevision",
                required: false,
                shape: WireShape::Any,
            },
        ],
    },
    EndpointSpec {
        namespace: "skills",
        method: "list",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "subagents",
        method: "interruptByParent",
        args: &[
            ArgSpec {
                wire: "childSessionId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "parentSessionId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "mode",
                required: true,
                shape: C8,
            },
        ],
    },
    EndpointSpec {
        namespace: "subagents",
        method: "list",
        args: &[ArgSpec {
            wire: "parentSessionId",
            required: true,
            shape: WireShape::String,
        }],
    },
    EndpointSpec {
        namespace: "subagents",
        method: "prompt",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "workspace",
        method: "archiveSession",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "workspace",
        method: "create",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "workspace",
        method: "delete",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "workspace",
        method: "follow",
        args: &[],
    },
    EndpointSpec {
        namespace: "workspace",
        method: "insertBefore",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "workspace",
        method: "insertSessionBefore",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "workspace",
        method: "rename",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            shape: WireShape::Object,
        }],
    },
    EndpointSpec {
        namespace: "workspaceFiles",
        method: "changes",
        args: &[ArgSpec {
            wire: "workspaceFileScopeId",
            required: true,
            shape: WireShape::String,
        }],
    },
    EndpointSpec {
        namespace: "workspaceFiles",
        method: "list",
        args: &[
            ArgSpec {
                wire: "workspaceFileScopeId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "path",
                required: true,
                shape: WireShape::String,
            },
        ],
    },
    EndpointSpec {
        namespace: "workspaceFiles",
        method: "read",
        args: &[
            ArgSpec {
                wire: "workspaceFileScopeId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "path",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "range",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "workspaceFiles",
        method: "readAll",
        args: &[
            ArgSpec {
                wire: "workspaceFileScopeId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "path",
                required: true,
                shape: WireShape::String,
            },
        ],
    },
    EndpointSpec {
        namespace: "workspaceFiles",
        method: "readBytes",
        args: &[
            ArgSpec {
                wire: "workspaceFileScopeId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "path",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "range",
                required: true,
                shape: WireShape::Object,
            },
        ],
    },
    EndpointSpec {
        namespace: "workspaceFiles",
        method: "readRelated",
        args: &[
            ArgSpec {
                wire: "workspaceFileScopeId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "path",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "relativePath",
                required: true,
                shape: WireShape::String,
            },
        ],
    },
    EndpointSpec {
        namespace: "workspaceFiles",
        method: "stat",
        args: &[
            ArgSpec {
                wire: "workspaceFileScopeId",
                required: true,
                shape: WireShape::String,
            },
            ArgSpec {
                wire: "path",
                required: true,
                shape: WireShape::String,
            },
        ],
    },
];

/// Look up one endpoint's argument descriptor.
pub fn endpoint(namespace: &str, method: &str) -> Option<&'static EndpointSpec> {
    ENDPOINTS
        .iter()
        .find(|e| e.namespace == namespace && e.method == method)
}
