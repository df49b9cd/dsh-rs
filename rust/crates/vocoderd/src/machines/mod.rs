//! Business namespace machines.
//!
//! Each namespace is one Sans-I/O plugin machine; the driver feeds it
//! "vocoder/<ns>/call" events and realizes its rpc.result outputs.

pub mod events;
pub mod goals;
pub mod session;
pub mod settings;
pub mod workspace;

/// Epoch milliseconds (f64 like JS).
pub fn session_now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}
