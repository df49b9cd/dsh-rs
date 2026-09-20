// GENERATED from spec/typert/remote.json — do not edit.
//! Per-endpoint argument descriptors, used by the dispatch boundary.
//!
//! See `vocoderd/src/validate.rs` for how these are applied.
#![allow(clippy::all)]

/// One wire argument of one endpoint.
#[derive(Debug, Clone, Copy)]
pub struct ArgSpec {
    /// The argument's **wire** name (not always its source name).
    pub wire: &'static str,
    pub required: bool,
    /// The arg value's **pruned** JSON Schema, as JSON text; `""`
    /// when the spec declares no constraint.
    pub schema: &'static str,
}

/// One endpoint's argument list.
#[derive(Debug, Clone, Copy)]
pub struct EndpointSpec {
    pub namespace: &'static str,
    pub method: &'static str,
    pub args: &'static [ArgSpec],
}

/// Every endpoint the spec declares, sorted by namespace then method.
pub static ENDPOINTS: &[EndpointSpec] = &[
    EndpointSpec {
        namespace: "agentPresets",
        method: "copy",
        args: &[
            ArgSpec {
                wire: "from",
                required: true,
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "id",
                required: true,
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "name",
                required: false,
                schema: "{\"type\":\"unknown\"}",
            },
        ],
    },
    EndpointSpec {
        namespace: "agentPresets",
        method: "deletePreset",
        args: &[ArgSpec {
            wire: "id",
            required: true,
            schema: "{\"type\":\"string\"}",
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
            schema: "{\"type\":\"string\"}",
        }],
    },
    EndpointSpec {
        namespace: "agentPresets",
        method: "select",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "agentPreset",
                required: true,
                schema: "{\"type\":\"string\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "request",
                required: true,
                schema: "{\"properties\":{\"blockedBy\":{\"items\":{\"allOf\":[{\"type\":\"string\"},{}]},\"type\":\"array\"},\"description\":{\"type\":\"string\"},\"subject\":{\"type\":\"string\"},\"writeScopes\":{\"items\":{\"type\":\"string\"},\"type\":\"array\"}},\"required\":[\"subject\",\"description\"],\"type\":\"object\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "request",
                required: true,
                schema: "{\"properties\":{\"action\":{\"anyOf\":[{\"const\":\"complete\",\"type\":\"string\"},{\"const\":\"edit\",\"type\":\"string\"},{\"const\":\"claim\",\"type\":\"string\"},{\"const\":\"release\",\"type\":\"string\"},{\"const\":\"set_dependencies\",\"type\":\"string\"},{\"const\":\"reopen\",\"type\":\"string\"},{\"const\":\"reassign\",\"type\":\"string\"},{\"const\":\"delete\",\"type\":\"string\"}]},\"blockedBy\":{\"items\":{\"allOf\":[{\"type\":\"string\"},{}]},\"type\":\"array\"},\"description\":{\"type\":\"string\"},\"expectedRevision\":{\"type\":\"number\"},\"owner\":{\"type\":\"string\"},\"subject\":{\"type\":\"string\"},\"taskId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"writeScopes\":{\"items\":{\"type\":\"string\"},\"type\":\"array\"}},\"required\":[\"taskId\",\"expectedRevision\",\"action\"],\"type\":\"object\"}",
            },
        ],
    },
    EndpointSpec {
        namespace: "agentTeams",
        method: "view",
        args: &[ArgSpec {
            wire: "agentId",
            required: true,
            schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
        }],
    },
    EndpointSpec {
        namespace: "commands",
        method: "execute",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "line",
                required: true,
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "submittedAttachments",
                required: true,
                schema: "{\"items\":{\"anyOf\":[{\"allOf\":[{\"properties\":{\"type\":{\"const\":\"image\",\"type\":\"string\"}},\"required\":[\"type\"],\"type\":\"object\"},{\"properties\":{\"data\":{\"type\":\"string\"},\"mediaType\":{\"anyOf\":[{\"const\":\"image/png\",\"type\":\"string\"},{\"const\":\"image/jpeg\",\"type\":\"string\"},{\"const\":\"image/webp\",\"type\":\"string\"},{\"const\":\"image/gif\",\"type\":\"string\"}]},\"name\":{\"type\":\"string\"}},\"required\":[\"mediaType\",\"data\"],\"type\":\"object\"}]},{\"properties\":{\"receiptId\":{\"type\":\"string\"},\"type\":{\"const\":\"file\",\"type\":\"string\"}},\"required\":[\"type\",\"receiptId\"],\"type\":\"object\"}]},\"type\":\"array\"}",
            },
        ],
    },
    EndpointSpec {
        namespace: "commands",
        method: "list",
        args: &[ArgSpec {
            wire: "agentId",
            required: true,
            schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
        }],
    },
    EndpointSpec {
        namespace: "credentials",
        method: "describe",
        args: &[ArgSpec {
            wire: "refs",
            required: true,
            schema: "{\"items\":{\"type\":\"string\"},\"type\":\"array\"}",
        }],
    },
    EndpointSpec {
        namespace: "credentials",
        method: "set",
        args: &[
            ArgSpec {
                wire: "ref",
                required: true,
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "value",
                required: true,
                schema: "{\"type\":\"string\"}",
            },
        ],
    },
    EndpointSpec {
        namespace: "credentials",
        method: "unset",
        args: &[ArgSpec {
            wire: "ref",
            required: true,
            schema: "{\"type\":\"string\"}",
        }],
    },
    EndpointSpec {
        namespace: "directoryPicker",
        method: "createDirectory",
        args: &[
            ArgSpec {
                wire: "path",
                required: true,
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "name",
                required: true,
                schema: "{\"type\":\"string\"}",
            },
        ],
    },
    EndpointSpec {
        namespace: "directoryPicker",
        method: "list",
        args: &[ArgSpec {
            wire: "path",
            required: false,
            schema: "{\"type\":\"unknown\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "pluginRunId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "pluginRunId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "method",
                required: true,
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "args",
                required: true,
                schema: "{\"$defs\":{\"__schema0\":{\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"type\":\"string\"},{\"type\":\"number\"},{\"const\":false,\"type\":\"boolean\"},{\"const\":true,\"type\":\"boolean\"},{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},{\"type\":\"object\"}]}},\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"type\":\"string\"},{\"type\":\"number\"},{\"const\":false,\"type\":\"boolean\"},{\"const\":true,\"type\":\"boolean\"},{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},{\"type\":\"object\"}]}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "pluginRunId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "failure",
                required: true,
                schema: "{\"properties\":{\"message\":{\"type\":\"string\"},\"stack\":{\"type\":\"string\"}},\"required\":[\"message\"],\"type\":\"object\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "pluginRunId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "failure",
                required: true,
                schema: "{\"properties\":{\"abdicated\":{\"type\":\"boolean\"},\"message\":{\"type\":\"string\"},\"slot\":{\"type\":\"string\"},\"stack\":{\"type\":\"string\"}},\"required\":[\"slot\",\"message\",\"abdicated\"],\"type\":\"object\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "requestId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "resolution",
                required: true,
                schema: "{\"$defs\":{\"__schema0\":{\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"type\":\"string\"},{\"type\":\"number\"},{\"const\":false,\"type\":\"boolean\"},{\"const\":true,\"type\":\"boolean\"},{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},{\"type\":\"object\"}]}},\"anyOf\":[{\"properties\":{\"data\":{\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"type\":\"string\"},{\"type\":\"number\"},{\"const\":false,\"type\":\"boolean\"},{\"const\":true,\"type\":\"boolean\"},{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},{\"type\":\"object\"}]},\"ok\":{\"const\":true,\"type\":\"boolean\"}},\"required\":[\"ok\",\"data\"],\"type\":\"object\"},{\"properties\":{\"message\":{\"type\":\"string\"},\"ok\":{\"const\":false,\"type\":\"boolean\"},\"reason\":{\"anyOf\":[{\"const\":\"cancelled\",\"type\":\"string\"},{\"const\":\"provider-missing\",\"type\":\"string\"},{\"const\":\"method-missing\",\"type\":\"string\"},{\"const\":\"invalid-input\",\"type\":\"string\"},{\"const\":\"provider-error\",\"type\":\"string\"}]}},\"required\":[\"ok\",\"reason\",\"message\"],\"type\":\"object\"}]}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "resolution",
                required: true,
                schema: "{\"anyOf\":[{\"properties\":{\"ok\":{\"const\":true,\"type\":\"boolean\"},\"pluginRunId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"waitingFor\":{\"items\":{\"type\":\"string\"},\"type\":\"array\"}},\"required\":[\"ok\",\"pluginRunId\"],\"type\":\"object\"},{\"properties\":{\"message\":{\"type\":\"string\"},\"ok\":{\"const\":false,\"type\":\"boolean\"},\"pluginRunId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"reason\":{\"anyOf\":[{\"const\":\"rejected\",\"type\":\"string\"},{\"const\":\"host-half-failed\",\"type\":\"string\"},{\"const\":\"client-half-failed\",\"type\":\"string\"}]},\"stack\":{\"type\":\"string\"},\"startedHere\":{\"type\":\"boolean\"}},\"required\":[\"ok\",\"reason\"],\"type\":\"object\"}]}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "packageId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "mode",
                required: true,
                schema: "{\"anyOf\":[{\"const\":\"run\",\"type\":\"string\"},{\"const\":\"update\",\"type\":\"string\"}]}",
            },
            ArgSpec {
                wire: "requestId",
                required: true,
                schema: "{\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"allOf\":[{\"type\":\"string\"},{}]}]}",
            },
            ArgSpec {
                wire: "approveFutureVersions",
                required: true,
                schema: "{\"type\":\"boolean\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "resolution",
                required: true,
                schema: "{\"anyOf\":[{\"properties\":{\"ok\":{\"const\":true,\"type\":\"boolean\"},\"pluginRunId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"waitingFor\":{\"items\":{\"type\":\"string\"},\"type\":\"array\"}},\"required\":[\"ok\",\"pluginRunId\"],\"type\":\"object\"},{\"properties\":{\"message\":{\"type\":\"string\"},\"ok\":{\"const\":false,\"type\":\"boolean\"},\"pluginRunId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"reason\":{\"anyOf\":[{\"const\":\"rejected\",\"type\":\"string\"},{\"const\":\"host-half-failed\",\"type\":\"string\"},{\"const\":\"client-half-failed\",\"type\":\"string\"}]},\"stack\":{\"type\":\"string\"},\"startedHere\":{\"type\":\"boolean\"}},\"required\":[\"ok\",\"reason\"],\"type\":\"object\"}]}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
        ],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "syncInspectManifest",
        args: &[ArgSpec {
            wire: "providers",
            required: true,
            schema: "{\"$defs\":{\"__schema0\":{\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"type\":\"string\"},{\"type\":\"number\"},{\"const\":false,\"type\":\"boolean\"},{\"const\":true,\"type\":\"boolean\"},{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},{\"type\":\"object\"}]}},\"items\":{\"properties\":{\"description\":{\"type\":\"string\"},\"id\":{\"type\":\"string\"},\"methods\":{\"items\":{\"properties\":{\"description\":{\"type\":\"string\"},\"inputSchema\":{\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"type\":\"string\"},{\"type\":\"number\"},{\"const\":false,\"type\":\"boolean\"},{\"const\":true,\"type\":\"boolean\"},{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},{\"type\":\"object\"}]},\"name\":{\"type\":\"string\"},\"outputSchema\":{\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"type\":\"string\"},{\"type\":\"number\"},{\"const\":false,\"type\":\"boolean\"},{\"const\":true,\"type\":\"boolean\"},{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},{\"type\":\"object\"}]}},\"required\":[\"name\",\"description\",\"inputSchema\",\"outputSchema\"],\"type\":\"object\"},\"type\":\"array\"}},\"required\":[\"id\",\"description\",\"methods\"],\"type\":\"object\"},\"type\":\"array\"}",
        }],
    },
    EndpointSpec {
        namespace: "dynamicCordisRunner",
        method: "undefineFromPanel",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "pluginId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "query",
                required: true,
                schema: "{\"type\":\"string\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "request",
                required: true,
                schema: "{\"properties\":{\"data\":{\"type\":\"string\"},\"name\":{\"type\":\"string\"}},\"required\":[\"data\"],\"type\":\"object\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "ref",
                required: true,
                schema: "{\"properties\":{\"id\":{\"allOf\":[{\"type\":\"string\"},{}]},\"revision\":{\"type\":\"number\"}},\"required\":[\"id\",\"revision\"],\"type\":\"object\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "ref",
                required: true,
                schema: "{\"properties\":{\"id\":{\"allOf\":[{\"type\":\"string\"},{}]},\"revision\":{\"type\":\"number\"}},\"required\":[\"id\",\"revision\"],\"type\":\"object\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "request",
                required: true,
                schema: "{\"properties\":{\"maxGoalRounds\":{\"type\":\"number\"},\"objective\":{\"type\":\"string\"}},\"required\":[\"objective\"],\"type\":\"object\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "ref",
                required: true,
                schema: "{\"properties\":{\"id\":{\"allOf\":[{\"type\":\"string\"},{}]},\"revision\":{\"type\":\"number\"}},\"required\":[\"id\",\"revision\"],\"type\":\"object\"}",
            },
            ArgSpec {
                wire: "request",
                required: true,
                schema: "{\"properties\":{\"maxGoalRounds\":{\"type\":\"number\"},\"objective\":{\"type\":\"string\"}},\"type\":\"object\"}",
            },
        ],
    },
    EndpointSpec {
        namespace: "goals",
        method: "get",
        args: &[ArgSpec {
            wire: "agentId",
            required: true,
            schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
        }],
    },
    EndpointSpec {
        namespace: "goals",
        method: "pause",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "ref",
                required: true,
                schema: "{\"properties\":{\"id\":{\"allOf\":[{\"type\":\"string\"},{}]},\"revision\":{\"type\":\"number\"}},\"required\":[\"id\",\"revision\"],\"type\":\"object\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "ref",
                required: true,
                schema: "{\"properties\":{\"id\":{\"allOf\":[{\"type\":\"string\"},{}]},\"revision\":{\"type\":\"number\"}},\"required\":[\"id\",\"revision\"],\"type\":\"object\"}",
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
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "request",
                required: true,
                schema: "{\"properties\":{\"api\":{\"type\":\"string\"},\"apiKey\":{\"type\":\"string\"},\"baseURL\":{\"type\":\"string\"},\"provider\":{\"type\":\"string\"}},\"type\":\"object\"}",
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
            schema: "{\"properties\":{\"ifVersion\":{\"allOf\":[{\"type\":\"string\"},{}]},\"messageId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"sessionId\",\"messageId\",\"ifVersion\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "messageFeedback",
        method: "list",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"sessionId\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "messageFeedback",
        method: "put",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"category\":{\"anyOf\":[{\"const\":\"other\",\"type\":\"string\"},{\"const\":\"task-result\",\"type\":\"string\"},{\"const\":\"instruction-following\",\"type\":\"string\"},{\"const\":\"product-interaction\",\"type\":\"string\"},{\"const\":\"service-stability\",\"type\":\"string\"},{\"const\":\"resource-cost\",\"type\":\"string\"},{\"const\":\"security-privacy-permission\",\"type\":\"string\"}]},\"ifVersion\":{\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"allOf\":[{\"type\":\"string\"},{}]}]},\"messageId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"note\":{\"type\":\"string\"},\"rating\":{\"anyOf\":[{\"const\":\"positive\",\"type\":\"string\"},{\"const\":\"negative\",\"type\":\"string\"}]},\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"sessionId\",\"messageId\",\"rating\",\"ifVersion\"],\"type\":\"object\"}",
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
            schema: "{\"properties\":{\"attachmentId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"sessionId\",\"attachmentId\"],\"type\":\"object\"}",
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
            schema: "{\"properties\":{\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"sessionId\"],\"type\":\"object\"}",
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
            schema: "{\"properties\":{\"agentPreset\":{\"type\":\"string\"},\"cwd\":{\"type\":\"string\"},\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"workspaceId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "follow",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"address\":{\"anyOf\":[{\"properties\":{\"kind\":{\"const\":\"session\",\"type\":\"string\"},\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"kind\",\"sessionId\"],\"type\":\"object\"},{\"properties\":{\"childSessionId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"kind\":{\"const\":\"subagent\",\"type\":\"string\"},\"mode\":{\"anyOf\":[{\"const\":\"one-shot\",\"type\":\"string\"},{\"const\":\"continuable\",\"type\":\"string\"}]},\"parentSessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"kind\",\"parentSessionId\",\"childSessionId\",\"mode\"],\"type\":\"object\"}]},\"assistantStream\":{\"const\":true,\"type\":\"boolean\"},\"maxMessages\":{\"type\":\"number\"}},\"required\":[\"address\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "fork",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"atSeq\":{\"type\":\"number\"},\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"sessionId\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "list",
        args: &[ArgSpec {
            wire: "_request",
            required: true,
            schema: "{\"properties\":{\"cursor\":{\"type\":\"string\"}},\"type\":\"object\"}",
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
            schema: "{\"properties\":{\"action\":{\"const\":\"reveal\",\"type\":\"string\"},\"path\":{\"type\":\"string\"}},\"required\":[\"path\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "page",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"address\":{\"anyOf\":[{\"properties\":{\"kind\":{\"const\":\"session\",\"type\":\"string\"},\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"kind\",\"sessionId\"],\"type\":\"object\"},{\"properties\":{\"childSessionId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"kind\":{\"const\":\"subagent\",\"type\":\"string\"},\"mode\":{\"anyOf\":[{\"const\":\"one-shot\",\"type\":\"string\"},{\"const\":\"continuable\",\"type\":\"string\"}]},\"parentSessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"kind\",\"parentSessionId\",\"childSessionId\",\"mode\"],\"type\":\"object\"}]},\"beforeSeq\":{\"type\":\"number\"},\"maxMessages\":{\"type\":\"number\"},\"throughSeq\":{\"type\":\"number\"}},\"required\":[\"address\",\"throughSeq\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "prompt",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"clientTimeZone\":{\"type\":\"string\"},\"content\":{\"items\":{\"anyOf\":[{\"properties\":{\"text\":{\"type\":\"string\"},\"type\":{\"const\":\"text\",\"type\":\"string\"}},\"required\":[\"type\",\"text\"],\"type\":\"object\"},{\"properties\":{\"data\":{\"type\":\"string\"},\"mediaType\":{\"anyOf\":[{\"const\":\"image/png\",\"type\":\"string\"},{\"const\":\"image/jpeg\",\"type\":\"string\"},{\"const\":\"image/webp\",\"type\":\"string\"},{\"const\":\"image/gif\",\"type\":\"string\"}]},\"name\":{\"type\":\"string\"},\"type\":{\"const\":\"image\",\"type\":\"string\"}},\"required\":[\"type\",\"mediaType\",\"data\"],\"type\":\"object\"},{\"properties\":{\"receiptId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"type\":{\"const\":\"file\",\"type\":\"string\"}},\"required\":[\"type\",\"receiptId\"],\"type\":\"object\"}]},\"type\":\"array\"},\"mode\":{\"anyOf\":[{\"const\":\"queue\",\"type\":\"string\"},{\"const\":\"steer\",\"type\":\"string\"}]},\"requestId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"requestId\",\"sessionId\",\"mode\",\"content\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "rename",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"title\":{\"type\":\"string\"}},\"required\":[\"sessionId\",\"title\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "search",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"query\":{\"type\":\"string\"}},\"required\":[\"query\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "selectModel",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"model\":{\"type\":\"string\"},\"provider\":{\"type\":\"string\"},\"reasoningEffort\":{\"type\":\"string\"},\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"sessionId\",\"provider\",\"model\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "session",
        method: "updateQueue",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"$defs\":{\"__schema0\":{\"anyOf\":[{\"properties\":{\"attachment\":{\"properties\":{\"attachmentId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"bytes\":{\"type\":\"number\"},\"name\":{\"type\":\"string\"}},\"required\":[\"attachmentId\",\"name\",\"bytes\"],\"type\":\"object\"},\"type\":{\"const\":\"file\",\"type\":\"string\"}},\"required\":[\"type\",\"attachment\"],\"type\":\"object\"},{\"properties\":{\"text\":{\"type\":\"string\"},\"type\":{\"const\":\"text\",\"type\":\"string\"}},\"required\":[\"type\",\"text\"],\"type\":\"object\"},{\"properties\":{\"attachment\":{\"properties\":{\"attachmentId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"bytes\":{\"type\":\"number\"},\"height\":{\"type\":\"number\"},\"mediaType\":{\"anyOf\":[{\"const\":\"image/png\",\"type\":\"string\"},{\"const\":\"image/jpeg\",\"type\":\"string\"},{\"const\":\"image/webp\",\"type\":\"string\"},{\"const\":\"image/gif\",\"type\":\"string\"}]},\"name\":{\"type\":\"string\"},\"originalDimensions\":{\"properties\":{\"height\":{\"type\":\"number\"},\"width\":{\"type\":\"number\"}},\"required\":[\"width\",\"height\"],\"type\":\"object\"},\"width\":{\"type\":\"number\"}},\"required\":[\"attachmentId\",\"mediaType\",\"bytes\",\"width\",\"height\"],\"type\":\"object\"},\"type\":{\"const\":\"image\",\"type\":\"string\"}},\"required\":[\"type\",\"attachment\"],\"type\":\"object\"},{\"properties\":{\"text\":{\"type\":\"string\"},\"type\":{\"const\":\"reasoning\",\"type\":\"string\"}},\"required\":[\"type\",\"text\"],\"type\":\"object\"},{\"properties\":{\"arguments\":{\"type\":\"string\"},\"id\":{\"allOf\":[{\"type\":\"string\"},{}]},\"name\":{\"type\":\"string\"},\"type\":{\"const\":\"tool-call\",\"type\":\"string\"}},\"required\":[\"type\",\"id\",\"name\",\"arguments\"],\"type\":\"object\"},{\"properties\":{\"content\":{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},\"isError\":{\"type\":\"boolean\"},\"toolCallId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"type\":{\"const\":\"tool-result\",\"type\":\"string\"}},\"required\":[\"type\",\"toolCallId\",\"content\"],\"type\":\"object\"}]}},\"properties\":{\"action\":{\"anyOf\":[{\"properties\":{\"content\":{\"items\":{\"anyOf\":[{\"properties\":{\"attachment\":{\"properties\":{\"attachmentId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"bytes\":{\"type\":\"number\"},\"name\":{\"type\":\"string\"}},\"required\":[\"attachmentId\",\"name\",\"bytes\"],\"type\":\"object\"},\"type\":{\"const\":\"file\",\"type\":\"string\"}},\"required\":[\"type\",\"attachment\"],\"type\":\"object\"},{\"properties\":{\"text\":{\"type\":\"string\"},\"type\":{\"const\":\"text\",\"type\":\"string\"}},\"required\":[\"type\",\"text\"],\"type\":\"object\"},{\"properties\":{\"attachment\":{\"properties\":{\"attachmentId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"bytes\":{\"type\":\"number\"},\"height\":{\"type\":\"number\"},\"mediaType\":{\"anyOf\":[{\"const\":\"image/png\",\"type\":\"string\"},{\"const\":\"image/jpeg\",\"type\":\"string\"},{\"const\":\"image/webp\",\"type\":\"string\"},{\"const\":\"image/gif\",\"type\":\"string\"}]},\"name\":{\"type\":\"string\"},\"originalDimensions\":{\"properties\":{\"height\":{\"type\":\"number\"},\"width\":{\"type\":\"number\"}},\"required\":[\"width\",\"height\"],\"type\":\"object\"},\"width\":{\"type\":\"number\"}},\"required\":[\"attachmentId\",\"mediaType\",\"bytes\",\"width\",\"height\"],\"type\":\"object\"},\"type\":{\"const\":\"image\",\"type\":\"string\"}},\"required\":[\"type\",\"attachment\"],\"type\":\"object\"},{\"properties\":{\"text\":{\"type\":\"string\"},\"type\":{\"const\":\"reasoning\",\"type\":\"string\"}},\"required\":[\"type\",\"text\"],\"type\":\"object\"},{\"properties\":{\"arguments\":{\"type\":\"string\"},\"id\":{\"allOf\":[{\"type\":\"string\"},{}]},\"name\":{\"type\":\"string\"},\"type\":{\"const\":\"tool-call\",\"type\":\"string\"}},\"required\":[\"type\",\"id\",\"name\",\"arguments\"],\"type\":\"object\"},{\"properties\":{\"content\":{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},\"isError\":{\"type\":\"boolean\"},\"toolCallId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"type\":{\"const\":\"tool-result\",\"type\":\"string\"}},\"required\":[\"type\",\"toolCallId\",\"content\"],\"type\":\"object\"}]},\"type\":\"array\"},\"kind\":{\"const\":\"edit\",\"type\":\"string\"}},\"required\":[\"kind\",\"content\"],\"type\":\"object\"},{\"properties\":{\"kind\":{\"const\":\"remove\",\"type\":\"string\"}},\"required\":[\"kind\"],\"type\":\"object\"},{\"properties\":{\"kind\":{\"const\":\"steer\",\"type\":\"string\"}},\"required\":[\"kind\"],\"type\":\"object\"}]},\"itemId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"sessionId\",\"itemId\",\"action\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "sessionFeedback",
        method: "record",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"category\":{\"anyOf\":[{\"const\":\"other\",\"type\":\"string\"},{\"const\":\"task-result\",\"type\":\"string\"},{\"const\":\"instruction-following\",\"type\":\"string\"},{\"const\":\"product-interaction\",\"type\":\"string\"},{\"const\":\"service-stability\",\"type\":\"string\"},{\"const\":\"resource-cost\",\"type\":\"string\"},{\"const\":\"security-privacy-permission\",\"type\":\"string\"}]},\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"text\":{\"type\":\"string\"}},\"required\":[\"sessionId\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "sessionReferenceResolver",
        method: "candidates",
        args: &[
            ArgSpec {
                wire: "agentId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "query",
                required: true,
                schema: "{\"type\":\"string\"}",
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
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "ops",
                required: true,
                schema: "{\"$defs\":{\"__schema0\":{\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"type\":\"string\"},{\"type\":\"number\"},{\"const\":false,\"type\":\"boolean\"},{\"const\":true,\"type\":\"boolean\"},{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},{\"type\":\"object\"}]}},\"items\":{\"anyOf\":[{\"properties\":{\"op\":{\"const\":\"set\",\"type\":\"string\"},\"path\":{\"items\":{\"type\":\"string\"},\"type\":\"array\"},\"value\":{\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"type\":\"string\"},{\"type\":\"number\"},{\"const\":false,\"type\":\"boolean\"},{\"const\":true,\"type\":\"boolean\"},{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},{\"type\":\"object\"}]}},\"required\":[\"op\",\"path\",\"value\"],\"type\":\"object\"},{\"properties\":{\"op\":{\"const\":\"unset\",\"type\":\"string\"},\"path\":{\"items\":{\"type\":\"string\"},\"type\":\"array\"}},\"required\":[\"op\",\"path\"],\"type\":\"object\"}]},\"type\":\"array\"}",
            },
            ArgSpec {
                wire: "expectedRevision",
                required: false,
                schema: "{\"type\":\"unknown\"}",
            },
        ],
    },
    EndpointSpec {
        namespace: "settings",
        method: "openAgentPresetDirectory",
        args: &[ArgSpec {
            wire: "agentPreset",
            required: true,
            schema: "{\"type\":\"string\"}",
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
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "section",
                required: true,
                schema: "{\"$defs\":{\"__schema0\":{\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"type\":\"string\"},{\"type\":\"number\"},{\"const\":false,\"type\":\"boolean\"},{\"const\":true,\"type\":\"boolean\"},{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},{\"type\":\"object\"}]}},\"type\":\"object\"}",
            },
            ArgSpec {
                wire: "expectedRevision",
                required: false,
                schema: "{\"type\":\"unknown\"}",
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
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "patch",
                required: true,
                schema: "{\"$defs\":{\"__schema0\":{\"anyOf\":[{\"const\":null,\"type\":\"null\"},{\"type\":\"string\"},{\"type\":\"number\"},{\"const\":false,\"type\":\"boolean\"},{\"const\":true,\"type\":\"boolean\"},{\"items\":{\"$ref\":\"#/$defs/__schema0\"},\"type\":\"array\"},{\"type\":\"object\"}]}},\"type\":\"object\"}",
            },
            ArgSpec {
                wire: "expectedRevision",
                required: false,
                schema: "{\"type\":\"unknown\"}",
            },
        ],
    },
    EndpointSpec {
        namespace: "skills",
        method: "list",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"sessionId\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "subagents",
        method: "interruptByParent",
        args: &[
            ArgSpec {
                wire: "childSessionId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "parentSessionId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "mode",
                required: true,
                schema: "{\"const\":\"continuable\",\"type\":\"string\"}",
            },
        ],
    },
    EndpointSpec {
        namespace: "subagents",
        method: "list",
        args: &[ArgSpec {
            wire: "parentSessionId",
            required: true,
            schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
        }],
    },
    EndpointSpec {
        namespace: "subagents",
        method: "prompt",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"childSessionId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"clientTimeZone\":{\"type\":\"string\"},\"content\":{\"items\":{\"anyOf\":[{\"properties\":{\"text\":{\"type\":\"string\"},\"type\":{\"const\":\"text\",\"type\":\"string\"}},\"required\":[\"type\",\"text\"],\"type\":\"object\"},{\"properties\":{\"data\":{\"type\":\"string\"},\"mediaType\":{\"anyOf\":[{\"const\":\"image/png\",\"type\":\"string\"},{\"const\":\"image/jpeg\",\"type\":\"string\"},{\"const\":\"image/webp\",\"type\":\"string\"},{\"const\":\"image/gif\",\"type\":\"string\"}]},\"name\":{\"type\":\"string\"},\"type\":{\"const\":\"image\",\"type\":\"string\"}},\"required\":[\"type\",\"mediaType\",\"data\"],\"type\":\"object\"}]},\"type\":\"array\"},\"delivery\":{\"anyOf\":[{\"const\":\"queue\",\"type\":\"string\"},{\"const\":\"steer\",\"type\":\"string\"}]},\"mode\":{\"const\":\"continuable\",\"type\":\"string\"},\"parentSessionId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"requestId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"requestId\",\"parentSessionId\",\"childSessionId\",\"mode\",\"delivery\",\"content\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "workspace",
        method: "archiveSession",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"sessionId\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "workspace",
        method: "create",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"path\":{\"type\":\"string\"}},\"required\":[\"path\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "workspace",
        method: "delete",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"workspaceId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"workspaceId\"],\"type\":\"object\"}",
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
            schema: "{\"properties\":{\"beforeWorkspaceId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"workspaceId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"workspaceId\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "workspace",
        method: "insertSessionBefore",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"beforeSessionId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"sessionId\":{\"allOf\":[{\"type\":\"string\"},{}]},\"workspaceId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"workspaceId\",\"sessionId\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "workspace",
        method: "rename",
        args: &[ArgSpec {
            wire: "request",
            required: true,
            schema: "{\"properties\":{\"title\":{\"type\":\"string\"},\"workspaceId\":{\"allOf\":[{\"type\":\"string\"},{}]}},\"required\":[\"workspaceId\",\"title\"],\"type\":\"object\"}",
        }],
    },
    EndpointSpec {
        namespace: "workspaceFiles",
        method: "changes",
        args: &[ArgSpec {
            wire: "workspaceFileScopeId",
            required: true,
            schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
        }],
    },
    EndpointSpec {
        namespace: "workspaceFiles",
        method: "list",
        args: &[
            ArgSpec {
                wire: "workspaceFileScopeId",
                required: true,
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "path",
                required: true,
                schema: "{\"type\":\"string\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "path",
                required: true,
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "range",
                required: true,
                schema: "{\"properties\":{\"limit\":{\"type\":\"number\"},\"offset\":{\"type\":\"number\"}},\"type\":\"object\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "path",
                required: true,
                schema: "{\"type\":\"string\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "path",
                required: true,
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "range",
                required: true,
                schema: "{\"properties\":{\"length\":{\"type\":\"number\"},\"offset\":{\"type\":\"number\"}},\"type\":\"object\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "path",
                required: true,
                schema: "{\"type\":\"string\"}",
            },
            ArgSpec {
                wire: "relativePath",
                required: true,
                schema: "{\"type\":\"string\"}",
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
                schema: "{\"allOf\":[{\"type\":\"string\"},{}]}",
            },
            ArgSpec {
                wire: "path",
                required: true,
                schema: "{\"type\":\"string\"}",
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
