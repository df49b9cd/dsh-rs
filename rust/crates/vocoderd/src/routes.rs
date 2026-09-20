//! Route-level tests for the web host: the combo route and the index injection.
//!
//! These drive the handler functions directly rather than through a bound
//! listener. The handlers are the whole surface — the router only names them —
//! and a real socket would add a port and a teardown to every assertion.

use std::sync::Arc;

use axum::http::{StatusCode, header};

use crate::web_boot::{self, Boot, DshTree, PickerBackend};

/// The dsh checkout, as `web_boot`'s own tests locate it.
fn boot() -> Arc<Boot> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../dsh");
    Arc::new(
        DshTree::new(root)
            .compose(PickerBackend::Native)
            .expect("the dsh tree composes"),
    )
}

/// A combo request for `spec`, as the shell sends it: `/plugins/` then the
/// `??`-prefixed specifier, which lands in the request's query string.
async fn combo_response(boot: Arc<Boot>, spec: &str) -> axum::response::Response {
    let uri: axum::http::Uri = format!("/plugins/{spec}").parse().expect("uri");
    crate::serve_combo(uri, boot).await
}

async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collects")
        .into()
}

#[tokio::test]
async fn combo_concatenates_the_named_bundles_with_one_map_url() {
    let boot = boot();
    // Two ids from the bootstrap batch's own list, taken from the graph so the
    // test does not hardcode the roster.
    let ids: Vec<String> = boot
        .graph
        .entries
        .iter()
        .take(2)
        .map(|e| e.id.clone())
        .collect();
    let spec = format!("??{}/client.js,{}/client.js&rev=deadbeef", ids[0], ids[1]);
    let response = combo_response(boot.clone(), &spec).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/javascript; charset=utf-8"
    );
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    let text = String::from_utf8(body_bytes(response).await).expect("utf8");
    // Exactly one sourceMappingURL line, absolute, and carrying the rev.
    assert_eq!(text.matches("//# sourceMappingURL=").count(), 1);
    assert!(text.contains("//# sourceMappingURL=/plugins/??"));
    assert!(text.contains("&rev=deadbeef"));
    // Both named bundles are present: their bodies are the concatenation.
    for id in &ids {
        let on_disk = std::fs::read_to_string(boot.bundles.get(id).unwrap()).unwrap();
        let first_line = on_disk.lines().next().unwrap();
        if !first_line.is_empty() {
            assert!(
                text.contains(first_line),
                "bundle {id}'s first line is served"
            );
        }
    }
}

#[tokio::test]
async fn the_map_form_is_an_indexed_source_map_with_one_section_per_bundle() {
    let boot = boot();
    let ids: Vec<String> = boot
        .graph
        .entries
        .iter()
        .take(2)
        .map(|e| e.id.clone())
        .collect();
    let spec = format!(
        "??{}/client.js.map,{}/client.js.map&rev=deadbeef",
        ids[0], ids[1]
    );
    let response = combo_response(boot.clone(), &spec).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/json; charset=utf-8"
    );
    let text = String::from_utf8(body_bytes(response).await).expect("utf8");
    // Not a concatenation of maps: an indexed map, one section per bundle.
    let map: serde_json::Value = serde_json::from_str(&text).expect("valid JSON map");
    assert_eq!(map["version"], 3);
    assert_eq!(map["file"], "client.js");
    let sections = map["sections"].as_array().expect("sections");
    assert_eq!(sections.len(), ids.len());
    assert_eq!(
        sections[0]["offset"],
        serde_json::json!({"line": 0, "column": 0})
    );
    // The second section's offset is exactly how many lines the first bundle
    // occupies in the served script — that is what makes the indexed map line
    // up. The single-id script is `source` then `;\n` then the map-URL line, so
    // the bundle's own span is one less than the script's newline count.
    let first_script = boot.combo(&ids[..1], "deadbeef", false).expect("first");
    let first_lines = first_script.iter().filter(|b| **b == b'\n').count() - 1;
    let second_line = sections[1]["offset"]["line"].as_u64().expect("line") as usize;
    assert_eq!(
        second_line, first_lines,
        "section offset matches the first bundle's lines"
    );
    // Each section is a Source Map v3 with its own sources.
    for section in sections {
        assert_eq!(section["map"]["version"], 3);
        assert!(section["map"]["sources"].is_array());
        assert!(
            section["map"].get("sourceRoot").is_none(),
            "sourceRoot dropped"
        );
    }
}

#[tokio::test]
async fn an_unknown_id_is_not_found() {
    let boot = boot();
    let response = combo_response(boot, "??@deepseek-ai/dsh-nope/client.js&rev=x").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_malformed_combo_specifier_is_not_found() {
    let boot = boot();
    // Not a `??` specifier at all.
    let response = combo_response(boot.clone(), "assets/index.js").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // A resource that is not a client.js.
    let response = combo_response(boot, "??@deepseek-ai/dsh-client-modules/index.ts").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn serve_index_injects_the_real_graph() {
    let boot = boot();
    let dist = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dist.path().join("index.html"),
        "<!doctype html><html><head></head><body><div id=root></div></body></html>",
    )
    .expect("index");
    let response = crate::serve_index(dist.path().to_path_buf(), boot.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(body_bytes(response).await).expect("utf8");
    // The facade is installed, the graph global carries every entry, and the
    // stub the old host wrote is gone.
    assert!(text.contains("window.__ModuleLoader__"));
    assert!(text.contains("__DSH_BOOT__"));
    assert!(
        !text.contains("\"kind\":\"vocoder\""),
        "the old stub is gone"
    );
    let entries = boot.graph.entries.len();
    let graph_json = boot.graph.to_json().to_string();
    assert!(text.contains(&graph_json[..graph_json.len().min(64)]));
    assert_eq!(
        boot.graph.to_json()["entries"].as_array().map(Vec::len),
        Some(entries)
    );
}

#[tokio::test]
async fn serve_index_without_a_dist_index_is_not_found() {
    let boot = boot();
    let dist = tempfile::tempdir().expect("tempdir");
    let response = crate::serve_index(dist.path().to_path_buf(), boot).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_composed_graph_serves_as_valid_json() {
    // The `__DSH_BOOT__` global is what the shell parses; it must survive a
    // round-trip through the injected text and be the graph the route serves.
    let boot = boot();
    let html = web_boot::inject_into_index("<html><head></head><body></body></html>", &boot.graph);
    let start = html
        .find("globalThis[\"__DSH_BOOT__\"] = ")
        .expect("global")
        + "globalThis[\"__DSH_BOOT__\"] = ".len();
    let end = start + html[start..].find("</script>").expect("close");
    let parsed: serde_json::Value =
        serde_json::from_str(&html[start..end]).expect("the injected graph is valid JSON");
    assert_eq!(parsed, boot.graph.to_json());
}

#[tokio::test]
async fn the_events_channel_opens_with_the_boot_graph_and_stays_open() {
    // The dev channel's first frame is the connect-time graph snapshot,
    // carrying the same graph global the index does. The stream never ends;
    // this reads the first frame and drops the rest.
    use futures_util::StreamExt;
    let boot = boot();
    let response = crate::serve_plugin_events(boot.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let mut body = response.into_body().into_data_stream();
    let first = body.next().await.expect("a first frame").expect("frame ok");
    let text = String::from_utf8(first.to_vec()).expect("utf8");
    // `data: <json>` — the frame the client parses off the EventSource.
    let payload = text.strip_prefix("data: ").expect("data frame").trim();
    let value: serde_json::Value = serde_json::from_str(payload).expect("valid JSON frame");
    assert_eq!(value["type"], "graph");
    assert_eq!(value["graph"], boot.graph.to_json());
}

#[tokio::test]
async fn the_open_in_app_route_answers_json_no_store() {
    // The route the `ui-open-in-app` client plugin fetches at boot. The id
    // list is host-dependent (it probes PATH/XDG), so the assertion is on the
    // shape — a JSON object with an `apps` array — not on this host's answer.
    let response = crate::open_in_app::handler().await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/json; charset=utf-8"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let text = String::from_utf8(body_bytes(response).await).expect("utf8");
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert!(value["apps"].is_array(), "apps is an array");
}
