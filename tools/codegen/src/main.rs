//! (Structs below mirror extracted spec JSON; we read a subset.)
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// spec/typert/remote.json Mirrors
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteSpec {
    endpoints: Vec<Endpoint>,
    lookup_keys: Vec<String>,
    context_keys: Vec<String>,
    error_detail_codes: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint {
    id: String,
    service: String,
    namespace: String,
    method: String,
    parameters: Vec<Parameter>,
    result: Option<Codec>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Parameter {
    name: String,
    wire: String,
    codec: Option<Codec>,
    lookup: Option<String>,
    /// The descriptor's `acceptsUndefined`: this wire field may be **absent**.
    ///
    /// Not the same as "the schema permits null" — it is the extractor's record
    /// of `T | undefined` on the source parameter, and an absent field is what
    /// the control accepts. A parameter without it is a required arg.
    #[serde(default, rename = "acceptsUndefined")]
    accepts_undefined: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Codec {
    mode: Option<String>,
    type_symbol: Option<String>,
    schema: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("generate");
    match mode {
        "coverage-report" => coverage_report(),
        _ => generate(),
    }
}

fn repo_root() -> Result<PathBuf> {
    let mut d = std::env::current_dir()?;
    loop {
        if d.join("spec").is_dir() && d.join("rust").is_dir() {
            return Ok(d);
        }
        d = d.parent().context("cannot locate repo root")?.to_path_buf();
    }
}

/// Write a generated Rust source file, then format it with rustfmt.
///
/// Generated output is committed and CI diffs it (`just codegen` must be a
/// no-op on a clean tree), so it has to be byte-stable under the repo's own
/// formatting. Rather than teaching the emitter rustfmt's line-breaking rules,
/// emit roughly and let rustfmt be the single source of formatting truth — the
/// same way `cargo fmt` owns hand-written files.
fn write_rust_src(path: &std::path::Path, contents: &str) -> Result<()> {
    fs::write(path, contents)?;
    let status = std::process::Command::new(
        std::env::var("RUSTFMT").unwrap_or_else(|_| "rustfmt".into()),
    )
    .args(["--edition", "2024", "--emit", "files"])
    .arg(path)
    .status()
    .context("run rustfmt (is the rustfmt component installed?)")?;
    if !status.success() {
        anyhow::bail!("rustfmt failed on {}", path.display());
    }
    Ok(())
}

fn generate() -> Result<()> {
    let root = repo_root()?;
    let spec: RemoteSpec = serde_json::from_str(
        &fs::read_to_string(root.join("spec/typert/remote.json")).context("read remote.json")?,
    )?;

    let out_dir = root.join("rust/crates/vocoder-spec-api/src/generated");
    fs::create_dir_all(&out_dir)?;

    // --- error codes ------------------------------------------------------
    let mut codes_rs = String::from(
        "// GENERATED from spec/typert/remote.json — do not edit.\n\
         /// Well-known RemoteError codes.\n\
         pub mod codes {\n",
    );
    for code in &spec.error_detail_codes {
        let ident = const_ident(code);
        codes_rs.push_str(&format!("    pub const {ident}: &str = {code:?};\n"));
    }
    codes_rs.push_str("}\n");
    write_rust_src(&out_dir.join("error_codes.rs"), &codes_rs)?;

    // --- DTOs from JSON schemas -------------------------------------------
    let mut types_rs = String::from(
        "// GENERATED from spec/schemas (via spec/typert/remote.json) — do not edit.\n\
         #![allow(clippy::all)]\n\
         use serde::{Deserialize, Serialize};\n\n",
    );
    let mut emitted_types: BTreeMap<String, String> = BTreeMap::new();
    for ep in &spec.endpoints {
        for p in &ep.parameters {
            if let Some(codec) = &p.codec {
                if let (Some(sym), Some(schema)) = (&codec.type_symbol, &codec.schema) {
                    emit_type(&mut types_rs, &mut emitted_types, sym, schema);
                }
            }
        }
        if let Some(result) = &ep.result {
            if let (Some(sym), Some(schema)) = (&result.type_symbol, &result.schema) {
                emit_type(&mut types_rs, &mut emitted_types, sym, schema);
            }
        }
    }
    write_rust_src(&out_dir.join("types.rs"), &types_rs)?;

    // --- traits per namespace ---------------------------------------------
    let mut traits_rs = String::from(
        "// GENERATED from spec/typert/remote.json — do not edit.\n\
         #![allow(clippy::all)]\n\
         use super::types::*;\n\
         use crate::RemoteError;\n\n",
    );
    let mut by_ns: BTreeMap<String, Vec<&Endpoint>> = BTreeMap::new();
    for ep in &spec.endpoints {
        by_ns.entry(ep.namespace.clone()).or_default().push(ep);
    }
    for (ns, eps) in by_ns {
        let trait_name = to_pascal_case(&format!("{ns}_service"));
        traits_rs.push_str(&format!("/// `{ns}` namespace — {} endpoint(s).\n", eps.len()));
        traits_rs.push_str(&format!(
            "#[async_trait::async_trait]\npub trait {trait_name}: Send + Sync {{\n"
        ));
        for ep in eps {
            let method = to_snake_case(&ep.method);
            let params: Vec<String> = ep
                .parameters
                .iter()
                .map(|p| {
                    let ty = p
                        .codec
                        .as_ref()
                        .and_then(|c| c.type_symbol.as_deref())
                        .map(type_name_from_symbol)
                        .unwrap_or_else(|| "serde_json::Value".into());
                    format!("{}: {}", to_snake_case(&p.name), ty)
                })
                .collect();
            let result_ty = ep
                .result
                .as_ref()
                .and_then(|r| r.type_symbol.as_deref())
                .map(type_name_from_symbol)
                .unwrap_or_else(|| "serde_json::Value".into());
            traits_rs.push_str(&format!(
                "    async fn {method}({params}) -> Result<{result_ty}, RemoteError>;\n",
                // `&self` is required even with no endpoint parameters: these
                // are trait-object methods on the service.
                params = if params.is_empty() {
                    "&self".to_string()
                } else {
                    format!("&self, {}", params.join(", "))
                }
            ));
        }
        traits_rs.push_str("}\n\n");
    }
    write_rust_src(&out_dir.join("traits.rs"), &traits_rs)?;

    // --- per-endpoint argument descriptors ---------------------------------
    //
    // A machine's `handle` is sync and pure, so it cannot deserialize a DTO
    // per call without either doing untyped JSON poking (what this replaces)
    // or inverting the machine contract to `async_trait`. Instead the *spec*
    // becomes data: this table says, for each endpoint, which wire args exist,
    // which are required, and what shape each value must have. The dispatch
    // boundary consults it before a machine sees the args, which is where the
    // control host's own boundary checks live.
    //
    // The layers this encodes are the two the spec actually carries:
    //   1. arg names vs the descriptor      → gateway/arguments-invalid
    //   2. a value vs its codec's schema    → gateway/input-invalid
    // A third layer exists on the control (`gateway/bad-request` with
    // `details.issues` for ids violating a `min(1)` the *zod* schema declares)
    // but the extracted JSON Schema does not carry `minLength`, so it is not
    // derivable here and lives in the machines that need it.
    let mut validate_rs = String::from(
        "// GENERATED from spec/typert/remote.json — do not edit.\n\
         //! Per-endpoint argument descriptors, used by the dispatch boundary.\n\
         //!\n\
         //! See `vocoderd/src/validate.rs` for how these are applied.\n\
         #![allow(clippy::all)]\n\n",
    );
    validate_rs.push_str(
        "/// The shape a wire value must have to satisfy its codec's schema.\n\
         #[derive(Debug, Clone, Copy, PartialEq, Eq)]\n\
         pub enum WireShape {\n\
         \x20   /// Any JSON value (`unknown` / no `type`).\n\
         \x20   Any,\n\
         \x20   String,\n\
         \x20   Number,\n\
         \x20   Boolean,\n\
         \x20   Array,\n\
         \x20   Object,\n\
         \x20   /// `const` — the one value it may take.\n\
         \x20   Const(&'static str),\n\
         \x20   /// An `enum` of string values.\n\
         \x20   Enum(&'static [&'static str]),\n\
         \x20   /// An `anyOf`/`oneOf` union; the value must satisfy at least one.\n\
         \x20   Union(&'static [WireShape]),\n\
         }\n\n",
    );
    validate_rs.push_str(
        "/// One wire argument of one endpoint.\n\
         #[derive(Debug, Clone, Copy)]\n\
         pub struct ArgSpec {\n\
         \x20   /// The argument's **wire** name (not always its source name).\n\
         \x20   pub wire: &'static str,\n\
         \x20   pub required: bool,\n\
         \x20   pub shape: WireShape,\n\
         }\n\n",
    );
    validate_rs.push_str(
        "/// One endpoint's argument list.\n\
         #[derive(Debug, Clone, Copy)]\n\
         pub struct EndpointSpec {\n\
         \x20   pub namespace: &'static str,\n\
         \x20   pub method: &'static str,\n\
         \x20   pub args: &'static [ArgSpec],\n\
         }\n\n",
    );

    // Shape literals must outlive the table, so each distinct one is emitted as
    // a `const` and referenced by name.
    let mut shape_consts: BTreeMap<String, String> = BTreeMap::new();
    let mut const_id = 0usize;
    fn shape_expr(
        schema: &serde_json::Value,
        consts: &mut BTreeMap<String, String>,
        id: &mut usize,
    ) -> String {
        const STRING: &str = "WireShape::String";
        const NUMBER: &str = "WireShape::Number";
        const BOOLEAN: &str = "WireShape::Boolean";
        const ARRAY: &str = "WireShape::Array";
        const OBJECT: &str = "WireShape::Object";
        const ANY: &str = "WireShape::Any";

        // `allOf: [x, {}]` is how the extractor wraps branded types; the first
        // member carries the real shape.
        if let Some(inner) = schema.get("allOf").and_then(|v| v.as_array()) {
            let first = inner.iter().find(|v| v.as_object().is_some_and(|o| !o.is_empty()));
            return match first {
                Some(f) => shape_expr(f, consts, id),
                None => ANY.to_string(),
            };
        }
        if let Some(c) = schema.get("const").and_then(|v| v.as_str()) {
            let name = format!("C{}", *id);
            *id += 1;
            consts.insert(name.clone(), format!("WireShape::Const({c:?})"));
            return format!("/*shape*/{name}");
        }
        if let Some(vals) = schema.get("enum").and_then(|v| v.as_array()) {
            let listed: Vec<String> = vals
                .iter()
                .filter_map(|v| v.as_str())
                .map(|v| format!("{v:?}"))
                .collect();
            let name = format!("C{}", *id);
            *id += 1;
            consts.insert(
                name.clone(),
                format!("WireShape::Enum(&[{}])", listed.join(", ")),
            );
            return format!("/*shape*/{name}");
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(vals) = schema.get(key).and_then(|v| v.as_array()) {
                let parts: Vec<String> =
                    vals.iter().map(|v| shape_expr(v, consts, id)).collect();
                let name = format!("C{}", *id);
                *id += 1;
                consts.insert(
                    name.clone(),
                    format!("WireShape::Union(&[{}])", parts.join(", ")),
                );
                return format!("/*shape*/{name}");
            }
        }
        match schema.get("type").and_then(|t| t.as_str()) {
            Some("string") => STRING.to_string(),
            Some("number") | Some("integer") => NUMBER.to_string(),
            Some("boolean") => BOOLEAN.to_string(),
            Some("array") => ARRAY.to_string(),
            Some("object") => OBJECT.to_string(),
            _ => ANY.to_string(),
        }
    }

    let mut table = String::new();
    let mut endpoints_sorted: Vec<&Endpoint> = spec.endpoints.iter().collect();
    endpoints_sorted.sort_by(|a, b| {
        (&a.namespace, &a.method).cmp(&(&b.namespace, &b.method))
    });
    for ep in &endpoints_sorted {
        let args: Vec<String> = ep
            .parameters
            .iter()
            .map(|p| {
                let shape = p
                    .codec
                    .as_ref()
                    .and_then(|c| c.schema.as_ref())
                    .map(|s| shape_expr(s, &mut shape_consts, &mut const_id))
                    .unwrap_or_else(|| "WireShape::Any".to_string());
                format!(
                    "ArgSpec {{ wire: {wire:?}, required: {req}, shape: {shape} }}",
                    wire = p.wire,
                    req = !p.accepts_undefined,
                    shape = shape.replace("/*shape*/", ""),
                )
            })
            .collect();
        table.push_str(&format!(
            "    EndpointSpec {{ namespace: {ns:?}, method: {m:?}, args: &[{args}] }},\n",
            ns = ep.namespace,
            m = ep.method,
            args = args.join(", "),
        ));
    }

    // Shape consts first, then the table that references them.
    validate_rs.push_str("// Shape literals (referenced by the table below).\n");
    for (name, value) in &shape_consts {
        validate_rs.push_str(&format!("const {name}: WireShape = {value};\n"));
    }
    validate_rs.push_str("\n/// Every endpoint the spec declares, sorted by namespace then method.\n");
    validate_rs.push_str("pub static ENDPOINTS: &[EndpointSpec] = &[\n");
    validate_rs.push_str(&table);
    validate_rs.push_str("];\n\n");
    validate_rs.push_str(
        "/// Look up one endpoint's argument descriptor.\n\
         pub fn endpoint(namespace: &str, method: &str) -> Option<&'static EndpointSpec> {\n\
         \x20   ENDPOINTS\n\
         \x20       .iter()\n\
         \x20       .find(|e| e.namespace == namespace && e.method == method)\n\
         }\n",
    );
    write_rust_src(&out_dir.join("validate.rs"), &validate_rs)?;

    // --- mod.rs ------------------------------------------------------------
    let mod_rs = "// GENERATED — do not edit.\npub mod error_codes;\npub mod traits;\npub mod types;\npub mod validate;\n";
    write_rust_src(&out_dir.join("mod.rs"), mod_rs)?;

    // Summary
    println!(
        "codegen: {} endpoints, {} namespaces, {} error codes, {} types",
        spec.endpoints.len(),
        by_ns_count(&spec),
        spec.error_detail_codes.len(),
        emitted_types.len()
    );
    Ok(())
}

fn by_ns_count(spec: &RemoteSpec) -> usize {
    let mut set = std::collections::BTreeSet::new();
    for e in &spec.endpoints {
        set.insert(&e.namespace);
    }
    set.len()
}

fn coverage_report() -> Result<()> {
    let root = repo_root()?;
    let spec: RemoteSpec = serde_json::from_str(
        &fs::read_to_string(root.join("spec/typert/remote.json")).context("read remote.json")?,
    )?;

    // Implemented unary/stream endpoints: scan the vocoderd machines' match
    // arms. This is a projection of the code, not the tests — CI runs the
    // wire cells for the runtime verdict; the report answers "what shape
    // do we claim" vs the spec.
    // One source file per implemented namespace. Keyed by the wire namespace
    // (not the file name: `workspace_files.rs` implements `workspaceFiles`).
    let machine_sources: Vec<(&str, String)> = [
        ("session", "session.rs"),
        ("workspace", "workspace.rs"),
        ("settings", "settings.rs"),
        ("goals", "goals.rs"),
        ("workspaceFiles", "workspace_files.rs"),
        ("directoryPicker", "directory_picker.rs"),
        ("credentials", "credentials.rs"),
        ("skills", "skills.rs"),
        ("fileReferences", "file_references.rs"),
        ("commands", "commands.rs"),
        ("agentPresets", "agent_presets.rs"),
        ("messageFeedback", "message_feedback.rs"),
        ("sessionFeedback", "session_feedback.rs"),
        ("sessionReferenceResolver", "session_references.rs"),
        ("pluginInventory", "plugin_inventory.rs"),
        ("llm", "llm.rs"),
        ("subagents", "subagents.rs"),
        ("agentTeams", "agent_teams.rs"),
        ("dynamicCordisRunner", "dynamic_cordis_runner.rs"),
    ]
    .into_iter()
    .map(|(ns, file)| {
        let path = root.join("rust/crates/vocoderd/src/machines").join(file);
        let src = fs::read_to_string(&path)
            .with_context(|| format!("{ns} machine source ({})", path.display()))?;
        Ok((ns, src))
    })
    .collect::<Result<Vec<_>>>()?;

    // Every wire method name this source matches on. Handles the
    // `"a" | "b" | "c" => …` alternation form: reading only the first literal
    // would report `pause`/`resume` as unimplemented while their arm is right
    // there, which is how this went unnoticed.
    let methods_of = |src: &str| -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        for line in src.lines() {
            let t = line.trim();
            if !t.starts_with('"') {
                continue;
            }
            let mut rest = t;
            // Consume one or more quoted literals joined by `|`.
            loop {
                let Some(after_open) = rest.strip_prefix('"') else {
                    break;
                };
                let Some(end) = after_open.find('"') else { break };
                if rest[end + 1..].contains("=>") {
                    out.insert(after_open[..end].to_string());
                }
                let tail = &after_open[end + 1..];
                match tail.trim_start().strip_prefix('|') {
                    Some(next) => rest = next.trim_start(),
                    None => {
                        if tail.contains("=>") {
                            out.insert(after_open[..end].to_string());
                        }
                        break;
                    }
                }
            }
        }
        out
    };
    let implemented: std::collections::BTreeMap<&str, std::collections::BTreeSet<String>> =
        machine_sources
            .iter()
            .map(|(ns, src)| (*ns, methods_of(src)))
            .collect();

    let mut md = String::new();
    md.push_str("# Spec coverage report\n\n");
    md.push_str(
        "Generated by `just coverage-report`. `implemented` = the vocoderd machine's match arm exists; '…^` marks stream endpoints covered only by streams_spec cells; `(M4)` flags behavior gated on the agent core, and `(native OS)` flags behavior gated on a native picker this host does not have.\n\n",
    );
    md.push_str("| Endpoint | Mode | Implemented by vocoderd |\n|---|---|---|\n");

    let mut total = 0;
    let mut have = 0;
    // Endpoints whose *behavior* is a stub. Two distinctions matter here,
    // because lumping them together overstates the gap:
    //
    // - **Pending the agent core.** These need a live turn (or an agent
    //   registry) to mean anything, so they answer a typed refusal or a no-op
    //   acceptance until M4 lands. `session/cancel` was in this list and is not
    //   any more: it reaches the live turn now (the agent machine latches the
    //   cancel; the session machine refuses an unattached or subagent address).
    // - **Pending OS integration.** Nothing about these depends on an agent
    //   loop; they need a native picker that a loopback host does not have.
    //   `session/modelCatalog` is neither — it is fully implemented, reading
    //   the llm machine's registry, and was stale in this list.
    let m4_stubs = [
        "session/attachment",
        "session/selectModel",
        "session/updateQueue",
    ];
    let os_stubs = ["session/openWorkspacePath", "session/canOpenWorkspacePath"];
    for ep in &spec.endpoints {
        let fq = format!("{}/{}", ep.namespace, ep.method);
        let implemented = implemented
            .get(ep.namespace.as_str())
            .is_some_and(|methods| methods.contains(ep.method.as_str()));
        total += 1;
        if implemented {
            have += 1;
        }
        let mark = if m4_stubs.contains(&fq.as_str()) {
            "yes, stub (M4)"
        } else if os_stubs.contains(&fq.as_str()) {
            "yes, stub (native OS)"
        } else if implemented {
            "yes"
        } else {
            "no"
        };
        let mode = if ep.id.contains("#$") { "internal" } else { "rpc" };
        md.push_str(&format!("| `{fq}` | {mode} | {mark} |\n"));
    }
    md.push_str(&format!(
        "\n{have} / {total} spec endpoints answered by vocoderd machines.\n"
    ));
    fs::write(root.join("docs/spec-coverage.md"), md)?;
    println!("wrote docs/spec-coverage.md ({have}/{total} endpoints implemented)");
    Ok(())
}

// ---------------------------------------------------------------------------
// Type emission (minimal structural JSON Schema → Rust)
// ---------------------------------------------------------------------------

fn type_name_from_symbol(sym: &str) -> String {
    // Strip a leading "@scope/pkg/subpath#" anchor if present; then take the
    // last path segment and sanitize into a Rust identifier.
    let after_anchor = sym.rsplit('#').next().unwrap_or(sym);
    // Build the name from the full tail (e.g. "agentPresets/copy:from" ->
    // "AgentPresetsCopyFrom") so distinct inline types never collide.
    let parts: Vec<String> = after_anchor
        .split(|c: char| !(c.is_ascii_alphanumeric()))
        .filter(|p| !p.is_empty())
        .map(|p| to_pascal_case(p))
        .collect();
    let mut ident = parts.concat();
    if ident.is_empty() {
        return "Anon".to_string();
    }
    if ident.chars().next().unwrap().is_ascii_digit() {
        ident.insert(0, '_');
    }
    ident
}

fn emit_type(
    out: &mut String,
    emitted: &mut BTreeMap<String, String>,
    symbol: &str,
    schema: &serde_json::Value,
) {
    let name = type_name_from_symbol(symbol);
    if emitted.contains_key(&name) {
        return;
    }
    emitted.insert(name.clone(), symbol.to_string());

    let ty = schema.get("type").and_then(|t| t.as_str()).unwrap_or("unknown");
    match ty {
        "object" => {
            out.push_str(&format!(
                "/// Wire type for `{symbol}`.\n#[derive(Debug, Clone, Serialize, Deserialize)]\n#[serde(rename_all = \"camelCase\")]\npub struct {name} {{\n"
            ));
            if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
                let required: Vec<&str> = schema
                    .get("required")
                    .and_then(|r| r.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                for (prop, sub) in props {
                    let field = to_snake_case(prop);
                    let rust_ty = rust_ty_for(sub);
                    let field_ty = if required.contains(&prop.as_str()) {
                        rust_ty
                    } else {
                        format!("Option<{rust_ty}>")
                    };
                    out.push_str(&format!("    pub {field}: {field_ty},\n"));
                }
            }
            out.push_str("}\n\n");
        }
        "string" => {
            if let Some(vals) = schema.get("enum").and_then(|e| e.as_array()) {
                out.push_str(&format!(
                    "/// Wire enum for `{symbol}`.\n#[derive(Debug, Clone, Serialize, Deserialize)]\npub enum {name} {{\n"
                ));
                for v in vals {
                    if let Some(s) = v.as_str() {
                        out.push_str(&format!("    #[serde(rename = {s:?})]\n    {},\n", to_pascal_case(s)));
                    }
                }
                out.push_str("}\n\n");
            } else {
                out.push_str(&format!("/// Wire alias for `{symbol}`.\npub type {name} = String;\n\n"));
            }
        }
        _ => {
            out.push_str(&format!(
                "/// Wire alias for `{symbol}` (structural: {}).\npub type {name} = serde_json::Value;\n\n",
                schema.get("type").and_then(|t| t.as_str()).unwrap_or("unknown")
            ));
        }
    }
}

fn rust_ty_for(schema: &serde_json::Value) -> String {
    let ty = schema.get("type").and_then(|t| t.as_str());
    match ty {
        Some("string") => "String".into(),
        Some("integer") => "i64".into(),
        Some("number") => "f64".into(),
        Some("boolean") => "bool".into(),
        Some("array") => {
            let items = schema
                .get("items")
                .map(rust_ty_for)
                .unwrap_or_else(|| "serde_json::Value".into());
            format!("Vec<{items}>")
        }
        Some("object") => "serde_json::Value".into(), // nested objects stay dynamic for v1
        _ => "serde_json::Value".into(),
    }
}

// ---------------------------------------------------------------------------
// Ident helpers
// ---------------------------------------------------------------------------

fn to_pascal_case(s: &str) -> String {
    s.split(|c: char| !(c.is_ascii_alphanumeric()))
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut c = p.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>()).unwrap_or_default()
                + c.as_str()
        })
        .collect()
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    escape_rust_ident(&out)
}

/// Rust keywords/idents that cannot be used as-is.
fn escape_rust_ident(s: &str) -> String {
    match s {
        "ref" | "type" | "async" | "await" | "loop" | "move" | "match" | "mod" | "pub"
        | "fn" | "in" | "impl" | "trait" | "struct" | "enum" | "where" | "use" | "let"
        | "mut" | "const" | "static" | "self" | "super" | "crate" | "dyn" | "box" => {
            format!("{s}_")
        }
        _ => s.to_string(),
    }
}

fn const_ident(code: &str) -> String {
    code.to_uppercase().replace(['/', '-', '.'], "_")
}
