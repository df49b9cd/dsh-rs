//! Golden round-trip tests: fixtures generated from the authoritative JS
//! shapes in dsh; Rust must decode and re-encode them identically.
//! (Decode-then-reencode equality over canonical JSON trees, not bytes:
//! key order is not normative.)

use serde_json::Value;
use vocoder_typert::*;

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

macro_rules! golden {
    ($name:literal, $ty:ty, $decode:ident, $encode:ident) => {
        let gold = fixture($name);
        let bytes = serde_json::to_vec(&gold).unwrap();
        let decoded: $ty = $decode(&bytes).unwrap_or_else(|e| panic!("decode {}: {e:?}", $name));
        let re: Value = serde_json::from_slice(&$encode(&decoded)).unwrap();
        assert_eq!(re, gold, "re-encode round-trip mismatch for {}", $name);
    };
}

#[test]
fn golden_rpc_client_request() {
    golden!(
        "rpc-client-request",
        ClientRequest,
        decode_rpc_client_request,
        encode_rpc_client_request
    );
}

#[test]
fn golden_mux_client_open() {
    golden!(
        "mux-client-open",
        StreamClientMessage,
        decode_stream_client,
        encode_stream_client
    );
}

#[test]
fn golden_mux_client_cancel() {
    golden!(
        "mux-client-cancel",
        StreamClientMessage,
        decode_stream_client,
        encode_stream_client
    );
}

#[test]
fn golden_mux_server_end() {
    golden!(
        "mux-server-end",
        StreamServerMessage,
        decode_stream_server,
        encode_stream_server
    );
}

#[test]
fn golden_mux_server_error() {
    golden!(
        "mux-server-error",
        StreamServerMessage,
        decode_stream_server,
        encode_stream_server
    );
}

#[test]
fn golden_rpc_server_ok() {
    golden!(
        "rpc-server-ok",
        ServerResponse,
        decode_rpc_server_response,
        encode_rpc_server_response
    );
}

#[test]
fn golden_rpc_server_err() {
    golden!(
        "rpc-server-err",
        ServerResponse,
        decode_rpc_server_response,
        encode_rpc_server_response
    );
}

#[test]
fn golden_mux_item_round_trips_via_serde() {
    // Item frames ride inside a value field; test the type directly.
    let gold = fixture("mux-server-item");
    let decoded: StreamServerMessage = serde_json::from_value(gold.clone()).unwrap();
    assert_eq!(serde_json::to_value(&decoded).unwrap(), gold);
}

#[test]
fn golden_event_frames() {
    for name in [
        "event-ready",
        "event-emit",
        "event-waterfall",
        "event-cancel",
    ] {
        let gold = fixture(name);
        let decoded: RemoteEventDownlinkFrame = serde_json::from_value(gold.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            gold,
            "round-trip {name}"
        );
    }
}

#[test]
fn decode_rejects_unknown_discriminant() {
    let bytes = br#"{"type":"nope","rpcId":"r-1"}"#;
    let err = decode_rpc_client_request(bytes).unwrap_err();
    assert!(matches!(err, FrameError::UnknownDiscriminant(_)));
}

#[test]
fn decode_rejects_malformed_json() {
    let bytes = b"{ not json";
    assert!(matches!(
        decode_rpc_client_request(bytes),
        Err(FrameError::BadJson(_))
    ));
}

#[test]
fn decode_mux_item_missing_value_is_fine() {
    // item frames may omit `value` (e.g. ready/Cancel markers ride distinct
    // shapes); decode must accept absence.
    let decoded: StreamServerMessage =
        decode_stream_server(br#"{"type":"item","streamId":"s-1"}"#).unwrap();
    assert!(matches!(
        decoded,
        StreamServerMessage::Item { value: None, .. }
    ));
}
