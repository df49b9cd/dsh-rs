//! Cross-machine shared stores. The router itself does not support one
//! machine synchronously awaiting another machine's event output (dispatch
//! results are scheduled asynchronously), so namespaces that must consult
//! each other's authoritative state share a typed in-memory + persisted
//! store injected at mount. This mirrors how dsh plugins resolve cross-
//! service lookups through host services.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

/// The workspace registry, shared between the workspace machine (owner) and
/// the session machine (reader for workspaceId resolution + list view).
pub struct WorkspaceRegistryStore {
    file: PathBuf,
    inner: Mutex<WorkspaceRegistryData>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRegistryData {
    #[serde(default)]
    pub initialized: bool,
    #[serde(default)]
    pub workspace_ids: Vec<String>,
    #[serde(default)]
    pub archived_session_ids: Vec<String>,
    #[serde(default)]
    pub records: BTreeMap<String, WorkspaceRecord>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    pub path: String,
    pub title: String,
    #[serde(default)]
    pub session_ids: Vec<String>,
    pub created_at: f64,
    pub updated_at: f64,
}

impl WorkspaceRegistryStore {
    /// Load from the durable file; an absent file starts empty. Handles for
    /// the same home (canonicalized) share one in-memory state, so a writer
    /// under one machine is immediately visible to a reader under another —
    /// mirrors dsh's single host service instance.
    pub fn open(home: &Path) -> std::sync::Arc<Self> {
        let file = home.join("workspaces.json");
        let key_home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
        let key = key_home.join("workspaces.json");
        static STORES: std::sync::OnceLock<
            Mutex<BTreeMap<PathBuf, std::sync::Weak<WorkspaceRegistryStore>>>,
        > = std::sync::OnceLock::new();
        let mut stores = STORES.get_or_init(|| Mutex::new(BTreeMap::new())).lock();
        // Reap dead entries while we're here.
        stores.retain(|_, w| w.strong_count() > 0);
        if let Some(existing) = stores.get(&key).and_then(|w| w.upgrade()) {
            return existing;
        }
        let inner: WorkspaceRegistryData = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let store = std::sync::Arc::new(Self {
            file,
            inner: Mutex::new(inner),
        });
        stores.insert(key, std::sync::Arc::downgrade(&store));
        store
    }

    /// Mutate the registry and persist; the lock is held through both so a
    /// writer never interleaves with another namespace's read between the
    /// mutation and the fsync.
    pub fn mutate<R>(&self, f: impl FnOnce(&mut WorkspaceRegistryData) -> R) -> Result<R, String> {
        let mut data = self.inner.lock();
        let out = f(&mut data);
        self.persist_locked(&data)?;
        Ok(out)
    }

    pub fn read<R>(&self, f: impl FnOnce(&WorkspaceRegistryData) -> R) -> R {
        let data = self.inner.lock();
        f(&data)
    }

    fn persist_locked(&self, data: &WorkspaceRegistryData) -> Result<(), String> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&self.file, serde_json::to_string_pretty(data).unwrap())
            .map_err(|e| e.to_string())
    }

    /// Resolve a workspaceId to its canonical path; None when unknown.
    pub fn path_of(&self, workspace_id: &str) -> Option<String> {
        self.read(|d| d.records.get(workspace_id).map(|r| r.path.clone()))
    }

    /// All workspaces whose path exactly equals `cwd` (rare: normally zero
    /// or one since create is canonical-path idempotent).
    pub fn find_by_path(&self, cwd: &str) -> Vec<String> {
        self.read(|d| {
            d.records
                .iter()
                .filter(|(_, r)| r.path == cwd)
                .map(|(id, _)| id.clone())
                .collect()
        })
    }
}
