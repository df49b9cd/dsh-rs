//! Business namespace machines.
//!
//! Each namespace is one Sans-I/O plugin machine; the driver feeds it
//! "vocoder/<ns>/call" events and realizes its rpc.result outputs.

pub mod agent;
pub mod agent_inbox;
pub mod agent_loop;
pub mod agent_presets;
pub mod agent_teams;
pub mod approval;
pub mod assistant_stream;
pub mod commands;
pub mod credentials;
pub mod directory_picker;
pub mod dynamic_cordis_runner;
pub mod events;
pub mod file_references;
pub mod goals;
pub mod llm;
pub mod llm_replay;
pub mod markdown;
pub mod message_feedback;
pub mod plugin_inventory;
pub mod provider;
pub mod readcache;
pub mod sandbox;
pub mod sandbox_runner;
pub mod session;
pub mod session_feedback;
pub mod session_references;
pub mod settings;
pub mod skills;
pub mod subagents;
pub mod tool;
pub mod tool_bash;
pub mod tool_exec;
pub mod workspace;
pub mod workspace_files;

/// Epoch milliseconds (f64 like JS).
pub fn session_now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}
