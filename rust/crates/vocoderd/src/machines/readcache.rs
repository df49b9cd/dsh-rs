//! Read-through filesystem cache for Sans-I/O machines.
//!
//! A machine may not touch the filesystem (docs/architecture.md), so a read is
//! *requested* as an effect and the answer lands here. Two consequences shape
//! this module:
//!
//! - **One datum per resume.** A handler that needs three files suspends three
//!   times. That is deliberate: it keeps each `handle` step small and makes the
//!   effect stream a readable trace, at the cost of re-running the handler.
//! - **The cache only grows.** Every re-run therefore gets strictly further,
//!   which is what guarantees termination without the machine tracking a
//!   program counter. `requested`/`published` are what stop a re-run from
//!   re-asking for a datum it already asked for — without them a missing file
//!   would spin until the effect cap.
//!
//! Suspension is engaged automatically: a handler returns `Err(outs)` from its
//! effect-request path, and [`Pending`] records which method to re-run.

use std::collections::{BTreeMap, BTreeSet};

use vocoder_cordis::{EffectId, EffectResult, MachineOut, RealizeRequest};

use crate::rpc;

/// What a single outstanding effect was for, so its answer can be attributed.
///
/// Exactly one effect is in flight at a time, which is why this can be a plain
/// `Option` rather than a map.
pub enum InFlight {
    /// A file's contents, keyed by absolute path.
    Read(String),
    /// A path resolution, keyed by `stat:<path>`.
    Stat(String),
    /// A ranged read, keyed by `range:<path>:<offset>:<limit>`.
    Range(String),
    /// A detailed directory listing, keyed by the directory path.
    DirDetailed(String),
    /// A file being published. The path is already in `published`, so the
    /// variant carries no payload — it exists to attribute the answer.
    Write,
}

/// Everything a machine has asked the driver for, plus the bookkeeping that
/// makes re-runs terminate.
#[derive(Default)]
pub struct FsCache {
    /// Every file beneath the walk root, from one `ListTree`.
    pub tree: Option<Vec<String>>,
    /// The walk root, remembered so invalidation can clear its `requested`
    /// marker and let the walk be re-requested.
    pub root: Option<String>,
    /// File contents by absolute path.
    pub files: BTreeMap<String, Vec<u8>>,
    /// Paths already requested, so a re-run never asks for the same datum
    /// twice (which would spin against the effect cap).
    pub requested: BTreeSet<String>,
    /// Requests that came back failed, by key. Kept so a re-run treats them as
    /// resolved instead of re-requesting — and keeping the `EffectError` rather
    /// than its message preserves the absent/other distinction, which is what
    /// decides between `workspace-file/not-found` and a gateway error.
    pub failed: BTreeMap<String, vocoder_cordis::EffectError>,
    /// The effect currently in flight.
    pub in_flight: Option<InFlight>,
    /// `Stat` answers, keyed by `stat:<path>`.
    pub stats: BTreeMap<String, EffectResult>,
    /// Ranged-read answers, keyed by `range:<path>:<offset>:<limit>`.
    pub ranges: BTreeMap<String, EffectResult>,
    /// Detailed directory listings, keyed by directory path.
    pub dirs: BTreeMap<String, Vec<vocoder_cordis::DirEntry>>,
    /// Paths this machine has already asked the driver to publish, so a re-run
    /// cannot double-write the same generation.
    pub published: BTreeSet<String>,
    /// The write this operation has already committed to.
    ///
    /// A re-run must not re-derive it: the *version number* in the path comes
    /// from the filesystem, which the write itself changes, so re-deriving
    /// would pick version N+1 and publish forever. Deciding once and reusing is
    /// what makes "suspend, then re-run" safe for a state-mutating operation.
    pub chosen_write: Option<(String, Vec<u8>)>,
    /// Other choices a suspended operation must reuse on re-run, by key.
    /// Same reason as `chosen_write`: a re-rolled session id would name a
    /// different session than the one already written.
    pub chosen: BTreeMap<String, serde_json::Value>,
}

/// A suspended operation: the method to re-run once the effect answers.
pub struct Pending {
    /// Filled by [`FsCache::next_effect`]. `None` after a run that requested
    /// nothing, which is how the dispatcher knows the call completed.
    pub effect: Option<EffectId>,
    pub method: String,
    pub req: serde_json::Value,
}

impl FsCache {
    /// A value the operation must decide once and keep across re-runs.
    ///
    /// Use for anything nondeterministic — a generated id, a timestamp, a
    /// version number — that also appears in an effect the machine has already
    /// issued. Re-rolling it on resume would make the re-run describe a
    /// different operation than the one the driver performed.
    pub fn choose<T>(&mut self, key: &str, decide: impl FnOnce() -> T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        if let Some(v) = self.chosen.get(key)
            && let Ok(v) = serde_json::from_value(v.clone())
        {
            return v;
        }
        let v = decide();
        if let Ok(json) = serde_json::to_value(&v) {
            self.chosen.insert(key.to_string(), json);
        }
        v
    }

    /// Forget per-operation choices; called when an operation completes and no
    /// longer needs to be reproducible.
    pub fn end_operation(&mut self) {
        self.chosen_write = None;
        self.chosen.clear();
    }

    /// Drop the cached walk so the next read re-walks the tree.
    ///
    /// Needed by a **read-only** machine: the automatic invalidation below
    /// fires only when *this* machine publishes a write, so a machine that
    /// never writes keeps its first walk forever and never sees files another
    /// machine created. `subagents` is exactly that shape — it enumerates
    /// sessions the `session` machine writes — so it must invalidate per call.
    ///
    /// Clears `requested` for the root as well, or the re-walk would
    /// short-circuit to an empty tree.
    pub fn invalidate_tree(&mut self) {
        self.tree = None;
        if let Some(root) = self.root.clone() {
            self.requested.remove(&root);
        }
    }

    /// Remember a value the post-write re-run must replay.
    ///
    /// Distinct from [`Self::choose`]: that pins a *decision* so a re-run makes
    /// the same one, whereas this carries a computed payload (the event a write
    /// already recorded) forward so the re-run can re-broadcast it. Without it
    /// a re-run sees its own committed row and concludes the work was a
    /// duplicate by another caller.
    pub fn remember_pending(&mut self, key: &str, value: &serde_json::Value) {
        self.chosen.insert(key.to_string(), value.clone());
    }

    /// The value remembered under `key`, if this operation is being re-run.
    pub fn recall_pending(&self, key: &str) -> Option<serde_json::Value> {
        self.chosen.get(key).cloned()
    }

    /// Claim the next effect id, arming `pending` with it.
    pub fn next_effect(&mut self, pending: &mut Option<Pending>, effects: &mut u64) -> EffectId {
        let id = EffectId::nth(*effects);
        *effects += 1;
        if let Some(p) = pending {
            p.effect = Some(id);
        }
        id
    }

    /// The walk root's file list, requesting it once if not cached.
    ///
    /// A root that does not exist is an empty tree, not an error: the sessions
    /// directory is absent until the first session is created.
    pub fn tree(
        &mut self,
        root: &str,
        pending: &mut Option<Pending>,
        effects: &mut u64,
    ) -> Result<Vec<String>, Vec<MachineOut>> {
        if let Some(tree) = &self.tree {
            return Ok(tree.clone());
        }
        if self.requested.contains(root) {
            return Ok(Vec::new());
        }
        self.root = Some(root.to_string());
        self.requested.insert(root.to_string());
        let id = self.next_effect(pending, effects);
        Err(vec![rpc::effect(
            id,
            RealizeRequest::ListTree {
                path: root.to_string(),
            },
        )])
    }

    /// Every generation file the walked tree mentions whose bytes are not yet
    /// cached, requested, or known-failed.
    ///
    /// Four machines now read the session log through this cache, and each one
    /// wrote this same filter. It is worth sharing precisely because it is easy
    /// to get subtly wrong: dropping the `requested` test makes a re-run ask
    /// for the same file forever, and dropping `failed` makes an unreadable
    /// generation retry until the effect cap instead of reporting absence.
    ///
    /// The order is the tree's, which is stable, so two machines issuing reads
    /// against one tree request them in the same sequence.
    pub fn unread_generations(&self, tree: &[String]) -> Vec<String> {
        tree.iter()
            .filter(|p| {
                let Some(name) = p.rsplit('/').next() else {
                    return false;
                };
                vocoder_session::parse_generation_filename(name).is_some()
                    && !self.files.contains_key(*p)
                    && !self.requested.contains(*p)
                    && !self.failed.contains_key(*p)
            })
            .cloned()
            .collect()
    }

    /// The rows of one session directory's latest generation, once its bytes
    /// are cached; `None` when there is no readable generation.
    ///
    /// Pair this with [`Self::unread_generations`]: read those first, then ask.
    /// The two steps are separate because the cache only grows — a caller
    /// requests one file, is re-run, and asks again.
    pub fn session_rows(&self, dir: &str, tree: &[String]) -> Option<Vec<serde_json::Value>> {
        let (_, path) = crate::machines::session::SessionStore::latest_generation_in(dir, tree)?;
        let bytes = self.files.get(&path)?;
        vocoder_session::decode_generation(
            bytes,
            crate::machines::session::SessionStore::is_compressed(&path),
        )
        .ok()
    }

    /// Ask the driver to publish bytes at `path`.
    ///
    /// Records the path as published *before* returning, so a re-run triggered
    /// by the write's own answer cannot request the same write again.
    /// `published` is only cleared by an explicit invalidation, because the
    /// caller's re-run still has to recognize its own completed write.
    pub fn request_write(
        &mut self,
        path: &str,
        contents: Vec<u8>,
        pending: &mut Option<Pending>,
        effects: &mut u64,
    ) -> Vec<MachineOut> {
        let id = self.next_effect(pending, effects);
        self.published.insert(path.to_string());
        self.in_flight = Some(InFlight::Write);
        vec![rpc::effect(
            id,
            RealizeRequest::WriteBytes {
                path: path.to_string(),
                contents,
            },
        )]
    }

    /// Decide the target of this operation's write, once.
    ///
    /// Returns the path and bytes chosen the first time this operation asked,
    /// so a re-run after the write completes recognizes its own work instead
    /// of picking the next generation number.
    pub fn write_target(
        &mut self,
        decide: impl FnOnce() -> (String, Vec<u8>),
    ) -> Result<(String, Vec<u8>), Vec<MachineOut>> {
        if let Some(chosen) = &self.chosen_write {
            return Ok(chosen.clone());
        }
        let target = decide();
        self.chosen_write = Some(target.clone());
        Ok(target)
    }

    /// Request a file's bytes unless cached, in flight, or known-failed.
    ///
    /// Uses `ReadBytes`: the files this machine reads are session generations,
    /// which are zstd frames and therefore not valid UTF-8.
    pub fn request_read(
        &mut self,
        path: &str,
        pending: &mut Option<Pending>,
        effects: &mut u64,
    ) -> Vec<MachineOut> {
        let id = self.next_effect(pending, effects);
        self.in_flight = Some(InFlight::Read(path.to_string()));
        vec![rpc::effect(
            id,
            RealizeRequest::ReadBytes {
                path: path.to_string(),
            },
        )]
    }

    /// Resolve a path via the driver, or `None` when it does not exist.
    pub fn canonicalize(
        &mut self,
        path: &str,
        pending: &mut Option<Pending>,
        effects: &mut u64,
    ) -> Result<Option<String>, Vec<MachineOut>> {
        let key = stat_key(path);
        if let Some(EffectResult::Stat { canonical, .. }) = self.stats.get(&key) {
            return Ok(Some(canonical.clone()));
        }
        if self.failed.contains_key(&key) || self.requested.contains(&key) {
            return Ok(None);
        }
        self.requested.insert(key.clone());
        let id = self.next_effect(pending, effects);
        self.in_flight = Some(InFlight::Stat(key));
        Err(vec![rpc::effect(
            id,
            RealizeRequest::Stat {
                path: path.to_string(),
            },
        )])
    }

    /// Record an effect answer. Returns `true` when the answer was an
    /// unattributable failure, which the caller reports as a protocol error.
    pub fn absorb(&mut self, result: EffectResult) -> bool {
        match result {
            EffectResult::Paths(paths) => {
                self.tree = Some(paths);
                false
            }
            EffectResult::Text(text) => {
                if let Some(InFlight::Read(path)) = self.in_flight.take() {
                    self.files.insert(path, text.into_bytes());
                }
                false
            }
            EffectResult::Bytes(bytes) => {
                if let Some(InFlight::Read(path)) = self.in_flight.take() {
                    self.files.insert(path, bytes);
                }
                false
            }
            EffectResult::Stat { .. } => {
                if let Some(InFlight::Stat(key)) = self.in_flight.take() {
                    self.stats.insert(key, result);
                }
                false
            }
            EffectResult::Range { .. } => {
                if let Some(InFlight::Range(key)) = self.in_flight.take() {
                    self.ranges.insert(key, result);
                }
                false
            }
            EffectResult::DirEntries(entries) => {
                if let Some(InFlight::DirDetailed(dir)) = self.in_flight.take() {
                    self.dirs.insert(dir, entries);
                }
                false
            }
            // Neither of these is a filesystem observation, so neither is ever
            // in flight here: a re-run cannot answer a subprocess from a cached
            // file read, and pretending otherwise would replay a command with
            // stale output.
            EffectResult::ProcessDone { .. } | EffectResult::Probe { .. } => false,
            // A detached process's lifecycle answers are likewise not cache
            // material: a background read is a delta, and caching it would
            // freeze "what the job printed" at its first poll.
            EffectResult::ProcessStarted { .. } | EffectResult::ProcessChunk { .. } => false,
            // A failure is remembered against whatever was in flight, so the
            // re-run treats it as absent rather than re-requesting forever.
            EffectResult::Failed(e) => match self.in_flight.take() {
                Some(InFlight::Read(path)) => {
                    self.failed.insert(path, e);
                    false
                }
                Some(InFlight::Stat(key))
                | Some(InFlight::Range(key))
                | Some(InFlight::DirDetailed(key)) => {
                    self.failed.insert(key, e);
                    false
                }
                // A failed publish must not be marked done, or the caller
                // would report success for a write that never landed.
                Some(InFlight::Write) => true,
                None => true,
            },
            // A completed publish changes the tree, so the cached walk is
            // stale. Drop it *and* un-request it, so the next read re-walks and
            // sees the new file; without clearing `requested` the walk would
            // short-circuit to an empty tree and a create-then-list sequence
            // would never find its own session.
            EffectResult::Done => {
                if let Some(InFlight::Write) = self.in_flight.take() {
                    self.tree = None;
                    if let Some(root) = self.root.clone() {
                        self.requested.remove(&root);
                    }
                }
                false
            }
            EffectResult::Entries(_) => false,
            // The cache is a filesystem cache and never issues a fetch, so an
            // HTTP answer cannot belong to it. Attributing this to a pending
            // read would poison that path with a bogus result.
            EffectResult::HttpResponse { .. } => true,
        }
    }
}

/// The cache key for a `Stat` on `path`.
pub fn stat_key(path: &str) -> String {
    format!("stat:{path}")
}
