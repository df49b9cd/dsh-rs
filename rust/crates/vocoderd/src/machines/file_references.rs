//! The `fileReferences` namespace: `@`-mention path completion for a session's
//! working directory.
//!
//! Upstream (`dsh/packages/api/session-controller/src/file-references.ts`) is a
//! Remote adapter over `ctx.fileReferences`, implemented for local filesystems
//! by `dsh-file-reference-local`, whose `WorkspaceFileSearch` this mirrors.
//!
//! **Paths only.** The candidate list carries a path and a kind; file contents
//! stay behind the model-facing `read` tool, because a completion popup must
//! never pull a file into the host to render a row.
//!
//! **Two query shapes**, and which one runs is decided by the query itself:
//!
//! - With a `/` (or empty): *directory listing*. The caller is walking a tree,
//!   so the answer is that directory's live contents, name-filtered by the
//!   fragment after the last slash. Only one directory is read.
//! - Without a `/`: *fuzzy search*. The caller is typing a bare name, so the
//!   whole workspace is walked once and ranked.
//!
//! **Ranking is deterministic**, because autocomplete that reorders between
//! keystrokes is unusable. Ties break by kind (directories first), then by
//! path length, then lexicographically.
//!
//! **The walk is bounded** (`maxEntries`) and skips a fixed set of directory
//! basenames — version-control and dependency stores plus build outputs.
//! Generated files carry the basenames of the sources that produced them, so
//! an unfiltered tree spends the budget twice and ranks `dist/x.js` beside
//! `src/x.ts`. `lib` is deliberately *not* excluded: Ruby gems and many npm
//! packages keep sources there.
//!
//! **Hidden entries** are offered only when the query asks for them — the
//! query starts with a dot, or reaches into a dot-directory — matching a
//! picker that would otherwise be flooded with `.cache` and `.git`.

use std::collections::BTreeSet;

use vocoder_cordis::{EffectResult, MachineIn, MachineOut, PluginMachine, RealizeRequest};

use crate::machines::readcache::{FsCache, InFlight, Pending};
use crate::machines::session::SessionStore;
use crate::rpc;

/// Default maximum candidates rendered for one query.
const DEFAULT_MAX_RESULTS: usize = 20;
/// Default maximum entries retained in one workspace walk.
const DEFAULT_MAX_ENTRIES: usize = 50_000;

/// Directory basenames never traversed or offered (upstream's default).
const EXCLUDED_DIRECTORIES: &[&str] = &[
    ".git",
    "node_modules",
    "dist",
    "build",
    "out",
    "coverage",
    "target",
    ".next",
    ".nuxt",
    ".turbo",
    ".venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".gradle",
];

/// One candidate path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    /// Workspace-relative, `/`-separated.
    path: String,
    /// `"file"` | `"directory"` — the wire vocabulary.
    kind: &'static str,
}

impl Candidate {
    fn is_dir(&self) -> bool {
        self.kind == "directory"
    }
}

pub struct FileReferencesMachine {
    sessions: SessionStore,
    cache: FsCache,
    pending: Option<Pending>,
    effects: u64,
    /// Workspace-relative paths discovered by the one workspace walk, plus
    /// whether the walk has run. Cached per call, not across calls: a query
    /// must see what is on disk now.
    index: Option<Vec<Candidate>>,
    /// The walk root, so the index can be attributed.
    index_root: Option<String>,
}

impl FileReferencesMachine {
    pub fn new(sessions_root: std::path::PathBuf) -> Self {
        Self {
            sessions: SessionStore::new(sessions_root),
            cache: FsCache::default(),
            pending: None,
            effects: 0,
            index: None,
            index_root: None,
        }
    }
}

impl PluginMachine for FileReferencesMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::EffectResult { result, .. } = ev {
            if self.pending.is_none() {
                return vec![];
            }
            // The walk answer is what builds the index, so it is absorbed into
            // the index rather than only into the cache.
            if let EffectResult::Paths(paths) = &result
                && let Some(root) = self.index_root.clone()
            {
                self.index = Some(self.index_from_paths(&root, paths));
            }
            if self.cache.absorb(result) {
                return rpc::err("gateway/internal", "fileReferences: effect failed");
            }
            let method = self.pending.as_ref().unwrap().method.clone();
            let req = self.pending.as_ref().unwrap().req.clone();
            return self.dispatch(&method, &req);
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("fileReferences") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let args = payload.get("args").cloned().unwrap_or_default();
        self.dispatch(&method, &args)
    }
}

impl FileReferencesMachine {
    fn dispatch(&mut self, method: &str, args: &serde_json::Value) -> Vec<MachineOut> {
        self.pending = Some(Pending {
            effect: None,
            method: method.to_string(),
            req: args.clone(),
        });
        let outs = match self.run(method, args) {
            Ok(outs) => outs,
            Err(effect) => effect,
        };
        if self.pending.as_ref().is_some_and(|p| p.effect.is_none()) {
            self.pending = None;
            self.cache.end_operation();
            // The index is per-call: a query must observe the tree as it is
            // now, and the next call re-walks.
            self.index = None;
            self.index_root = None;
        }
        outs
    }

    fn run(
        &mut self,
        method: &str,
        args: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        match method {
            "list" => self.list(args),
            other => Ok(rpc::err(
                "gateway/bad-request",
                format!("unsupported fileReferences method: {other}"),
            )),
        }
    }

    fn list(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        // `agentId` is a lookup parameter, delivered beside `request`.
        let session_id = rpc::arg_str(args, "agentId")
            .or_else(|| rpc::arg_str(args, "query").map(|_| ""))
            .unwrap_or_default()
            .to_string();
        let query = rpc::arg_str(args, "query")
            .or_else(|| {
                rpc::arg_str(
                    args.get("request").unwrap_or(&serde_json::Value::Null),
                    "query",
                )
            })
            .unwrap_or_default()
            .to_string();

        let Some(root) = self.workspace_root(&session_id)? else {
            return Ok(rpc::err_details(
                "session/not-found",
                format!("no such session: {session_id}"),
                serde_json::json!({ "sessionId": session_id }),
            ));
        };

        // Backslashes are normalised so a Windows-style query still splits on
        // its separator, matching upstream.
        let query = query.replace('\\', "/");
        let slash = query.rfind('/');
        match slash {
            // A bare name: fuzzy search over the whole walk.
            None if !query.is_empty() => {
                let index = self.index_of(&root)?;
                let visible: Vec<Candidate> = index
                    .into_iter()
                    .filter(|c| visible_for_global_query(&c.path, &query))
                    .collect();
                Ok(rpc::ok(rank(visible, &query, DEFAULT_MAX_RESULTS)))
            }
            // A directory listing, or the empty query (the bare `@`).
            _ => {
                let (display_dir, fragment) = match slash {
                    Some(i) => (&query[..i + 1], &query[i + 1..]),
                    None => ("", ""),
                };
                self.list_directory(&root, display_dir, fragment)
            }
        }
    }

    /// The session's `cwd`, or `None` when the session is unknown.
    fn workspace_root(&mut self, session_id: &str) -> Result<Option<String>, Vec<MachineOut>> {
        let root = self.sessions.root();
        let tree = self
            .cache
            .tree(&root, &mut self.pending, &mut self.effects)?;
        if let Some(path) = self.cache.unread_generations(&tree).first().cloned() {
            self.cache.requested.insert(path.clone());
            return Err(self
                .cache
                .request_read(&path, &mut self.pending, &mut self.effects));
        }
        Ok(SessionStore::stateless_scan(&tree, &self.cache.files)
            .into_iter()
            .find(|s| s.id == session_id)
            .and_then(|s| s.cwd()))
    }

    /// The whole-workspace walk, requested once per call.
    fn index_of(&mut self, root: &str) -> Result<Vec<Candidate>, Vec<MachineOut>> {
        if let Some(index) = &self.index {
            return Ok(index.clone());
        }
        self.index_root = Some(root.to_string());
        let tree = self
            .cache
            .tree(root, &mut self.pending, &mut self.effects)?;
        Ok(self.index_from_paths(root, &tree))
    }

    /// Project the walked absolute file paths into workspace-relative
    /// candidates, adding the directories on the way.
    ///
    /// `ListTree` reports files only, so each file's ancestor directories are
    /// derived from its path — which is also what makes the result a complete
    /// tree view without a per-directory effect.
    fn index_from_paths(&self, root: &str, paths: &[String]) -> Vec<Candidate> {
        let root = root.trim_end_matches('/');
        let mut out: Vec<Candidate> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for abs in paths {
            let Some(rel) = abs.strip_prefix(&format!("{root}/")) else {
                continue;
            };
            // An excluded directory anywhere in the path is pruned, matching
            // the traversal's own rule.
            let segments: Vec<&str> = rel.split('/').collect();
            if segments
                .iter()
                .take(segments.len().saturating_sub(1))
                .any(|s| EXCLUDED_DIRECTORIES.contains(s))
            {
                continue;
            }
            // Every ancestor directory is a candidate too.
            for i in 1..segments.len() {
                let dir = segments[..i].join("/");
                if seen.insert(dir.clone()) {
                    out.push(Candidate {
                        path: dir,
                        kind: "directory",
                    });
                }
            }
            if seen.insert(rel.to_string()) {
                out.push(Candidate {
                    path: rel.to_string(),
                    kind: "file",
                });
            }
        }
        out.truncate(DEFAULT_MAX_ENTRIES);
        out
    }

    /// One directory's live contents, name-filtered by `fragment`.
    fn list_directory(
        &mut self,
        root: &str,
        display_dir: &str,
        fragment: &str,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        // A query reaching into an excluded directory offers nothing, rather
        // than walking it.
        if display_dir
            .split('/')
            .any(|seg| !seg.is_empty() && EXCLUDED_DIRECTORIES.contains(&seg))
        {
            return Ok(rpc::ok(serde_json::Value::Array(Vec::new())));
        }
        let root = root.trim_end_matches('/');
        let abs = if display_dir.is_empty() {
            root.to_string()
        } else {
            format!("{root}/{}", display_dir.trim_end_matches('/'))
        };
        // Refuse an escape rather than resolving it: a query naming `../`
        // must not walk out of the workspace.
        if display_dir.split('/').any(|seg| seg == "..") {
            return Ok(rpc::ok(serde_json::Value::Array(Vec::new())));
        }

        let entries = self.dir_of(&abs)?;
        let showing_hidden = fragment.starts_with('.');
        let mut candidates: Vec<Candidate> = Vec::new();
        for e in entries {
            if e.name.starts_with('.') && !showing_hidden {
                continue;
            }
            if EXCLUDED_DIRECTORIES.contains(&e.name.as_str()) {
                continue;
            }
            let kind = if e.kind == "dir" { "directory" } else { "file" };
            candidates.push(Candidate {
                path: format!("{display_dir}{}", e.name),
                kind,
            });
        }
        Ok(rpc::ok(rank(candidates, fragment, DEFAULT_MAX_RESULTS)))
    }

    fn dir_of(&mut self, dir: &str) -> Result<Vec<vocoder_cordis::DirEntry>, Vec<MachineOut>> {
        if let Some(hit) = self.cache.dirs.get(dir) {
            return Ok(hit.clone());
        }
        if self.cache.failed.contains_key(dir) || self.cache.requested.contains(dir) {
            return Ok(Vec::new());
        }
        self.cache.requested.insert(dir.to_string());
        let id = self.cache.next_effect(&mut self.pending, &mut self.effects);
        self.cache.in_flight = Some(InFlight::DirDetailed(dir.to_string()));
        Err(vec![rpc::effect(
            id,
            RealizeRequest::ListDirDetailed {
                path: dir.to_string(),
            },
        )])
    }
}

/// Whether a bare query may see a dot-prefixed path.
///
/// A query that itself starts with a dot — or reaches into a dot-directory —
/// is asking for hidden entries; anything else is not.
fn visible_for_global_query(path: &str, query: &str) -> bool {
    if query.starts_with('.') || query.contains("/.") {
        return true;
    }
    !path.split('/').any(|seg| seg.starts_with('.'))
}

/// Rank candidates and project them to the wire shape.
fn rank(candidates: Vec<Candidate>, query: &str, limit: usize) -> serde_json::Value {
    let mut scored: Vec<(i64, &Candidate)> = candidates
        .iter()
        .filter_map(|c| score(c, query).map(|s| (s, c)))
        .collect();
    scored.sort_by(|(ls, l), (rs, r)| {
        rs.cmp(ls)
            .then_with(|| kind_rank(l.kind).cmp(&kind_rank(r.kind)))
            .then_with(|| {
                if query.is_empty() {
                    std::cmp::Ordering::Equal
                } else {
                    l.path.len().cmp(&r.path.len())
                }
            })
            .then_with(|| l.path.cmp(&r.path))
    });
    serde_json::Value::Array(
        scored
            .into_iter()
            .take(limit)
            .map(|(_, c)| serde_json::json!({ "path": c.path, "kind": c.kind }))
            .collect(),
    )
}

/// Directories sort before files at equal score.
fn kind_rank(kind: &str) -> u8 {
    if kind == "directory" { 0 } else { 1 }
}

/// A candidate's score, or `None` when it does not match at all.
///
/// The tiers mirror upstream: an exact name beats a prefix, which beats a
/// substring of the name, which beats a substring of the path, which beats a
/// subsequence. Directories carry a flat bonus so a directory and file that
/// match equally well present the directory first.
fn score(candidate: &Candidate, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let path = candidate.path.to_lowercase();
    let name = match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path.as_str(),
    };
    let needle = query.to_lowercase();
    let bonus = if candidate.is_dir() { 25 } else { 0 };
    if name == needle {
        return Some(1000 + bonus);
    }
    if name.starts_with(&needle) {
        return Some(900 + bonus);
    }
    if name.contains(&needle) {
        return Some(700 + bonus);
    }
    if path.contains(&needle) {
        return Some(500 + bonus);
    }
    subsequence_score(&path, &needle).map(|s| 300 + s + bonus)
}

/// How tightly `query` appears as a subsequence of `target`, or `None`.
fn subsequence_score(target: &str, query: &str) -> Option<i64> {
    let target: Vec<char> = target.chars().collect();
    let mut at = 0usize;
    let mut gap: i64 = 0;
    for q in query.chars() {
        let found = target[at..].iter().position(|c| *c == q)? + at;
        gap += (found - at) as i64;
        at = found + 1;
    }
    Some((100 - gap).max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(pairs: &[(&str, &str)]) -> Vec<Candidate> {
        pairs
            .iter()
            .map(|(p, k)| Candidate {
                path: p.to_string(),
                kind: if *k == "directory" {
                    "directory"
                } else {
                    "file"
                },
            })
            .collect()
    }

    fn paths_of(v: &serde_json::Value) -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_string())
            .collect()
    }

    /// Exact name, then prefix, then substring — the tiers upstream defines.
    #[test]
    fn ranking_prefers_the_strongest_match_tier() {
        // The index carries both a `main` *directory* (an exact name match)
        // and files whose names merely contain or start with the term.
        let set = cands(&[
            ("src/deep/other.ts", "file"),
            ("src/main.ts", "file"),
            ("main", "directory"),
            ("src/domain.ts", "file"),
        ]);
        let ranked = paths_of(&rank(set, "main", 20));
        // Exact name (1000 + the directory bonus) wins, then the name prefix.
        assert_eq!(ranked[0], "main", "{ranked:?}");
        assert_eq!(ranked[1], "src/main.ts", "{ranked:?}");
        // A path-substring hit (`src/domain.ts` contains "main") comes last.
        assert_eq!(ranked.last().unwrap(), "src/domain.ts", "{ranked:?}");
    }

    #[test]
    fn directories_win_ties_and_ordering_is_deterministic() {
        let set = cands(&[("abc.ts", "file"), ("abc", "directory")]);
        assert_eq!(paths_of(&rank(set, "abc", 20)), vec!["abc", "abc.ts"]);

        // Same score, same kind, different lengths: the shorter path wins,
        // and repeated runs agree.
        let set = cands(&[("a/x.ts", "file"), ("x.ts", "file")]);
        assert_eq!(
            paths_of(&rank(set.clone(), "x.ts", 20)),
            vec!["x.ts", "a/x.ts"]
        );
        assert_eq!(paths_of(&rank(set, "x.ts", 20)), vec!["x.ts", "a/x.ts"]);
    }

    #[test]
    fn subsequence_matches_and_non_matches() {
        // `smt` is a subsequence of `src/my-thing.ts` but not of `other.md`.
        let set = cands(&[("src/my-thing.ts", "file"), ("other.md", "file")]);
        let ranked = paths_of(&rank(set, "smt", 20));
        assert_eq!(ranked, vec!["src/my-thing.ts"]);
        // A query nothing matches yields nothing, not everything.
        let set = cands(&[("a.ts", "file")]);
        assert!(paths_of(&rank(set, "zzzz", 20)).is_empty());
    }

    #[test]
    fn an_empty_query_lists_everything_within_the_limit() {
        let set = cands(&[("a.ts", "file"), ("b.ts", "file"), ("c.ts", "file")]);
        assert_eq!(paths_of(&rank(set, "", 2)).len(), 2);
    }

    /// A bare query never surfaces hidden paths unless it asks for them.
    #[test]
    fn hidden_paths_need_an_explicit_dot() {
        assert!(!visible_for_global_query(".cache/x", "cac"));
        assert!(!visible_for_global_query("src/.git/config", "conf"));
        // A leading dot, or reaching into a dot-directory, opts in.
        assert!(visible_for_global_query(".cache/x", ".cac"));
        assert!(visible_for_global_query("src/.hidden/y", "src/.hid"));
        // An ordinary path is always visible.
        assert!(visible_for_global_query("src/main.ts", "main"));
    }

    #[test]
    fn the_walk_derives_directories_and_prunes_excluded_trees() {
        let m = FileReferencesMachine::new(std::path::PathBuf::from("/s"));
        let tree = vec![
            "/w/src/main.ts".to_string(),
            "/w/src/deep/x.ts".to_string(),
            "/w/node_modules/pkg/index.js".to_string(),
            "/w/README.md".to_string(),
        ];
        let index = m.index_from_paths("/w", &tree);
        // Compare as a set: insertion order is an implementation detail, and
        // sorting the rendered strings would order `src/deep/x.ts` before
        // `src/deep:directory` on the separator's byte value alone.
        let got: BTreeSet<String> = index
            .iter()
            .map(|c| format!("{}:{}", c.path, c.kind))
            .collect();
        let want: BTreeSet<String> = [
            "README.md:file",
            "src:directory",
            "src/deep:directory",
            "src/deep/x.ts:file",
            "src/main.ts:file",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(got, want);
        // The pruned tree contributed nothing.
        assert!(!got.iter().any(|p| p.contains("node_modules")), "{got:?}");
    }

    #[test]
    fn unknown_method_is_a_typed_bad_request() {
        let mut m = FileReferencesMachine::new(std::path::PathBuf::from("/s"));
        let outs = m.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("fileReferences")),
            payload: serde_json::json!({ "method": "nope", "args": {} }),
        });
        let v = outs
            .iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.to_wire_json()),
                _ => None,
            })
            .unwrap();
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
    }
}
