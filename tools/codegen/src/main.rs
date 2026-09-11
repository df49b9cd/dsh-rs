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
    fs::write(out_dir.join("error_codes.rs"), codes_rs)?;

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
    fs::write(out_dir.join("types.rs"), types_rs)?;

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
                "    async fn {method}(&self, {}) -> Result<{result_ty}, RemoteError>;\n",
                params.join(", ")
            ));
        }
        traits_rs.push_str("}\n\n");
    }
    fs::write(out_dir.join("traits.rs"), traits_rs)?;

    // --- mod.rs ------------------------------------------------------------
    let mod_rs = "// GENERATED — do not edit.\npub mod error_codes;\npub mod traits;\npub mod types;\n";
    fs::write(out_dir.join("mod.rs"), mod_rs)?;

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
    let mut by_ns: BTreeMap<String, usize> = BTreeMap::new();
    for ep in &spec.endpoints {
        *by_ns.entry(ep.namespace.clone()).or_default() += 1;
    }
    let mut md = String::new();
    md.push_str("# Spec coverage report\n\n");
    md.push_str("Generated by `just coverage-report`.\n\n");
    md.push_str("| Namespace | Endpoints |\n|---|---|\n");
    let mut total = 0;
    for (ns, n) in &by_ns {
        md.push_str(&format!("| `{ns}` | {n} |\n"));
        total += n;
    }
    md.push_str(&format!("| **total** | **{total}** |\n"));
    fs::write(root.join("docs/spec-coverage.md"), md)?;
    println!("wrote docs/spec-coverage.md ({total} endpoints)");
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
