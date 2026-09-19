//! Business namespace machines.
//!
//! Each namespace is one Sans-I/O plugin machine; the driver feeds it
//! "vocoder/<ns>/call" events and realizes its rpc.result outputs.

pub mod agent_presets;
pub mod commands;
pub mod credentials;
pub mod directory_picker;
pub mod events;
pub mod file_references;
pub mod goals;
pub mod message_feedback;
pub mod plugin_inventory;
pub mod readcache;
pub mod session;
pub mod session_feedback;
pub mod session_references;
pub mod settings;
pub mod skills;
pub mod workspace;
pub mod workspace_files;

/// Epoch milliseconds (f64 like JS).
pub fn session_now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}
