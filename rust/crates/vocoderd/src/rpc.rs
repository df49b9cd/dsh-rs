//! Shared RPC-call plumbing for business namespace machines.
//!
//! The driver decodes the HTTP envelope and delivers one generic event to
//! the owning namespace machine: EventName = "vocoder/{namespace}/call",
//! payload = {"method": "<m>", "args": {…}}. The machine answers with one
//! rpc.result Realize. Args mirror the Typert wire: payload.args keyed by
//! the parameter's wire name (packages/api/gateway invokes remotes with
//! {args}).

use vocoder_cordis::{MachineOut, RealizeRequest};

/// The event name a namespace machine listens on.
pub fn call_event(namespace: &str) -> String {
    format!("vocoder/{namespace}/call")
}

/// Success result output.
pub fn ok(value: serde_json::Value) -> Vec<MachineOut> {
    vec![MachineOut::Realize(RealizeRequest::Raw(serde_json::json!({
        "kind": "rpc.result",
        "result": { "ok": true, "value": value },
    })))]
}

/// Failure result; code is a Typert RemoteError code.
pub fn err(code: &str, message: impl Into<String>) -> Vec<MachineOut> {
    vec![MachineOut::Realize(RealizeRequest::Raw(serde_json::json!({
        "kind": "rpc.result",
        "result": { "ok": false, "error": { "code": code, "message": message.into() } },
    })))]
}

/// Failure with a typed details payload (RemoteErrorDetailsMap entries).
pub fn err_details(
    code: &str,
    message: impl Into<String>,
    details: serde_json::Value,
) -> Vec<MachineOut> {
    vec![MachineOut::Realize(RealizeRequest::Raw(serde_json::json!({
        "kind": "rpc.result",
        "result": { "ok": false, "error": { "code": code, "message": message.into(), "details": details } },
    })))]
}

/// Extract a string arg.
pub fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// Extract a u64 arg.
pub fn arg_u64(args: &serde_json::Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| v.as_u64())
}

/// Deep-merge patch onto target (settings update semantics): plain objects
/// merge recursively; every other value — arrays included — replaces
/// wholesale. Undefined entries are simply not present in the patch.
pub fn deep_merge(target: &mut serde_json::Value, patch: &serde_json::Value) {
    match (target, patch) {
        (serde_json::Value::Object(t), serde_json::Value::Object(p)) => {
            for (k, v) in p {
                match t.get_mut(k) {
                    Some(slot) => deep_merge(slot, v),
                    None => {
                        t.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, v) => *slot = v.clone(),
    }
}

/// Apply one settings mutate op ({op:"set"|"unset", path: string[], value?}).
pub fn apply_mutate_op(
    section: &mut serde_json::Value,
    op: &serde_json::Value,
) -> Result<(), String> {
    let kind = op.get("op").and_then(|v| v.as_str()).unwrap_or_default();
    let path_value = op.get("path").cloned().unwrap_or(serde_json::Value::Null);
    let mut path: Vec<String> = Vec::new();
    match &path_value {
        serde_json::Value::Array(items) => {
            for item in items {
                match item.as_str() {
                    Some(s) => path.push(s.to_string()),
                    None => return Err("mutate op path segments must be strings".into()),
                }
            }
        }
        _ => return Err("mutate op requires a string-array path".into()),
    }
    match kind {
        "set" => {
            let value = op.get("value").cloned().unwrap_or(serde_json::Value::Null);
            set_path(section, &path, value)
        }
        "unset" => {
            unset_path(section, &path);
            Ok(())
        }
        other => Err(format!("unsupported mutate op: {other}")),
    }
}

fn set_path(
    root: &mut serde_json::Value,
    path: &[String],
    value: serde_json::Value,
) -> Result<(), String> {
    if path.is_empty() {
        if !value.is_object() {
            return Err("replacing the section root requires a plain object".into());
        }
        *root = value;
        return Ok(());
    }
    let mut node = root;
    for (i, seg) in path.iter().enumerate() {
        let last = i == path.len() - 1;
        let obj = node
            .as_object_mut()
            .ok_or_else(|| "mutate path traverses a non-object value".to_string())?;
        if last {
            obj.insert(seg.clone(), value.clone());
            return Ok(());
        }
        let next = obj.entry(seg.clone()).or_insert_with(|| serde_json::json!({}));
        if !next.is_object() {
            *next = serde_json::json!({});
        }
        node = next;
    }
    Ok(())
}

fn unset_path(root: &mut serde_json::Value, path: &[String]) {
    if path.is_empty() {
        *root = serde_json::Value::Null;
        return;
    }
    let mut node: &mut serde_json::Value = root;
    for seg in &path[..path.len() - 1] {
        match node.get_mut(seg) {
            Some(next) if next.is_object() => node = next,
            _ => return, // absent intermediate path is satisfied
        }
    }
    if let Some(obj) = node.as_object_mut() {
        obj.remove(path.last().unwrap());
    }
}

/// Process-unique, non-guessable id fragment.
pub fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    static C: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let c = C.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:x}-{:x}-{:x}", nanos, std::process::id(), c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_replaces_arrays_merges_objects() {
        let mut t = serde_json::json!({"a": {"x": 1, "y": 2}, "b": [1, 2], "c": 3});
        deep_merge(&mut t, &serde_json::json!({"a": {"y": 20}, "b": [9]}));
        assert_eq!(t, serde_json::json!({"a": {"x": 1, "y": 20}, "b": [9], "c": 3}));
    }

    #[test]
    fn mutate_set_unset() {
        let mut s = serde_json::json!({});
        apply_mutate_op(&mut s, &serde_json::json!({"op": "set", "path": ["a", "b"], "value": 3}))
            .unwrap();
        assert_eq!(s, serde_json::json!({"a": {"b": 3}}));
        apply_mutate_op(&mut s, &serde_json::json!({"op": "unset", "path": ["a", "b"]})).unwrap();
        assert_eq!(s, serde_json::json!({"a": {}}));
        // Absent unset path is satisfied.
        apply_mutate_op(&mut s, &serde_json::json!({"op": "unset", "path": ["x", "y"]})).unwrap();
    }

    #[test]
    fn mutate_root_set_requires_object() {
        let mut s = serde_json::json!({"k": 1});
        assert!(
            apply_mutate_op(&mut s, &serde_json::json!({"op": "set", "path": [], "value": {"n": 2}}))
                .is_ok()
        );
        assert_eq!(s, serde_json::json!({"n": 2}));
        assert!(
            apply_mutate_op(&mut s, &serde_json::json!({"op": "set", "path": [], "value": 4}))
                .is_err()
        );
    }

    #[test]
    fn event_name_format() {
        assert_eq!(call_event("settings"), "vocoder/settings/call");
    }
}
