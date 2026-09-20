//! The client-module pipeline: the boot graph the web shell reads.
//!
//! Upstream does not ship a static web app. `apps/web/dist` holds only the Vite
//! shell — `index.html`, one app chunk, one vendor chunk, CSS. The shell's entry
//! (`packages/client/web/src/boot.ts:63`) then reads two globals a Node-side
//! service composed at runtime: `window.__ModuleLoader__`, a registration queue
//! the plugin bundles push into, and `window.__DSH_BOOT__`, the entry graph they
//! are resolved through. Neither is in the dist, so a host that serves the dist
//! as static files hands the browser a shell that throws
//! `web boot: window.__ModuleLoader__ bootstrap facade is missing` before mount.
//!
//! This module is that composer: it scans the dsh package tree for packages
//! declaring `dsh.client`, builds the entry graph, serves each package's built
//! client bundle through a combo route, and generates the index injection table.
//!
//! ## What this reproduces, and what it does not
//!
//! The authority is a **capture** of the real control host's `GET /`
//! (`dsh web`), not a reading of the TypeScript. The roster rule, the batch
//! partition, the combo URL format, the facade script, and the graph fields were
//! each checked against it, and the tests re-check them.
//!
//! Two things are deliberately *not* reproduced:
//!
//! - **Entry order.** `orderByModuleGraph` orders rows so a requested package
//!   precedes its consumers, but only two rows carry `external` edges, so its
//!   output depends on the input sequence — which upstream takes from the
//!   composed config's insertion order after a 971-line two-layer YAML merge.
//!   That order is not boot-observable: `bootClient` creates every row through
//!   `Promise.all` (`packages/client/web/src/boot-client.ts:57`) and the loader
//!   resolves through `inject` edges, not array position; and the graph `rev`,
//!   which is the only thing that could encode the order, is never read by the
//!   client (only per-row `rev` is, for reload). So the roster is emitted sorted
//!   by name and then ordered by the module graph, which is deterministic and
//!   still respects the edges.
//! - **`rev` values.** Upstream's per-entry rev is a per-boot random nonce
//!   (`randomBytes(8)-N`) and its graph rev is a sha1 over the graph JSON. Both
//!   are opaque cache keys: the combo route looks responses up by the exact URL
//!   string it was asked for, so any value this host hands out resolves against
//!   its own table. They are computed here with a dependency-free hash rather
//!   than pulled-in sha1, precisely because the value is not a wire contract.
//!
//! ## HMR is out of scope
//!
//! Upstream keeps two response generations (`previousBatchResponses`) so a
//! request racing a rebuild still resolves. There is no rebuild path here: the
//! graph is composed once at startup. `client-hmr` is in the roster and will
//! fetch its bundle, but nothing watches the filesystem.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The bootstrap package: its own client bundle supplies the module system
/// implementation, so it is the one parser-blocking `bootstrap` batch.
/// Mirrors `CLIENT_MODULES_ID` in `packages/client/modules/src/index.ts:425`.
const CLIENT_MODULES_ID: &str = "@deepseek-ai/dsh-client-modules";

/// Hex length of a revision. Mirrors `HASH_REVISION_LENGTH`.
const HASH_REVISION_LENGTH: usize = 12;

/// The longest projected combo URL (`partitionComboRecords`). Mirrors
/// `MAX_COMBO_URL_BYTES` = 3 * 1024.
const MAX_COMBO_URL_BYTES: usize = 3 * 1024;

/// The fixed relative filename a bundle's source map is read from.
const SOURCE_MAP_SUFFIX: &str = ".map";

/// Module specifiers the shell itself provides, so a row requesting one is
/// closed without a graph edge. Mirrors `getStaticModules`
/// (`packages/client/web/src/seed.ts`). Consumed by the closure check in the
/// tests; the loader resolves these names at runtime and never asks this host.
#[cfg_attr(not(test), allow(dead_code))]
pub const STATIC_MODULES: &[&str] = &[
    "react",
    "react/jsx-runtime",
    "react-dom",
    "react-dom/client",
    "@deepseek-ai/cordis",
    "@deepseek-ai/dsh-client-store",
    "@deepseek-ai/dsh-client-ui-slots",
    "@deepseek-ai/dsh-client-ui-primitives",
    "@deepseek-ai/dsh-client-ui-dockkit",
];

/// Which concrete directory-picker backend this boot mounts.
///
/// The `host-directory-picker-auto` row resolves one of two backends from host
/// facts at boot (`packages/host/directory-picker-auto/src/resolve.ts`) and the
/// chosen one is a client package, so it joins the graph without any bundle
/// patch naming it — which is why the roster is not purely patch-derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerBackend {
    Native,
    Browse,
}

impl PickerBackend {
    /// The client package this backend contributes.
    pub fn package(self) -> &'static str {
        match self {
            Self::Native => "@deepseek-ai/dsh-client-ui-directory-picker-native",
            Self::Browse => "@deepseek-ai/dsh-client-ui-directory-picker-browse",
        }
    }
}

/// Host facts the picker resolution is a pure function of.
#[derive(Debug, Clone)]
pub struct PickerFacts {
    /// Effective webserver bind host.
    pub bind_host: String,
    /// Host process platform, as `std::env::consts::OS`.
    pub platform: String,
    /// Launched over SSH (the inherited process layer, not `.env`).
    pub ssh: bool,
    /// A Linux display is set (`DISPLAY` or `WAYLAND_DISPLAY`, non-blank).
    pub display: bool,
    /// A Linux chooser binary (zenity/kdialog) is on PATH.
    pub linux_chooser: bool,
}

impl PickerFacts {
    /// Sample the current process's facts.
    pub fn detect(bind_host: &str) -> Self {
        let present = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        Self {
            bind_host: bind_host.to_string(),
            platform: std::env::consts::OS.to_string(),
            ssh: present("SSH_CONNECTION") || present("SSH_CLIENT"),
            display: present("DISPLAY") || present("WAYLAND_DISPLAY"),
            linux_chooser: on_path("zenity") || on_path("kdialog"),
        }
    }
}

/// Whether a bare program name resolves to an executable on `PATH`.
///
/// A real `PATH` search, unlike `driver::program_is_executable` (which answers
/// about the path *as given* and so rejects a bare name). The picker resolver's
/// question is "can this host drive a chooser", which is exactly a `PATH` lookup.
fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        std::fs::metadata(&candidate)
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// Resolve which picker backend this boot mounts.
///
/// A port of `resolveDirectoryPickerBackend`. `native` requires every signal
/// that the operator can see the host display (a loopback bind, no SSH launch,
/// and a servable display session); anything ambiguous resolves to `browse`,
/// which works everywhere.
pub fn resolve_picker_backend(facts: &PickerFacts) -> PickerBackend {
    if facts.bind_host != "127.0.0.1" {
        return PickerBackend::Browse;
    }
    if facts.ssh {
        return PickerBackend::Browse;
    }
    if facts.platform == "macos" || facts.platform == "windows" {
        return PickerBackend::Native;
    }
    if facts.platform != "linux" || !facts.linux_chooser {
        return PickerBackend::Browse;
    }
    if facts.display {
        PickerBackend::Native
    } else {
        PickerBackend::Browse
    }
}

/// One package's `dsh.client` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DshClientDecl {
    /// The only accepted value today; carried for the validation parity.
    pub platform: String,
    /// Package rows whose factories must arrive before this row materializes.
    pub inject: Vec<String>,
    /// Non-baseline module specifiers this row requests.
    pub external: Vec<String>,
    /// Stage-one prefetch mark.
    pub immediately: bool,
}

/// One composed graph entry: a package and how to load it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebBootEntry {
    pub id: String,
    pub url: String,
    pub rev: String,
    pub inject: Vec<String>,
    pub immediately: bool,
    pub external: Vec<String>,
}

/// One initial combo script and the entries it registers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebBootBatch {
    /// `bootstrap` is parser-blocking; `application` is preloaded.
    pub phase: &'static str,
    pub url: String,
    pub rev: String,
    pub entries: Vec<String>,
}

/// The composed graph, served as `window.__DSH_BOOT__`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebBootGraph {
    pub rev: String,
    pub entries: Vec<WebBootEntry>,
    pub batches: Vec<WebBootBatch>,
}

impl WebBootGraph {
    /// The wire form: the `__DSH_BOOT__` value.
    pub fn to_json(&self) -> Value {
        json!({
            "rev": self.rev,
            "entries": self.entries.iter().map(|e| {
                let mut row = json!({ "id": e.id, "url": e.url, "rev": e.rev });
                if !e.inject.is_empty() {
                    row["inject"] = json!(e.inject);
                }
                if e.immediately {
                    row["immediately"] = json!(true);
                }
                if !e.external.is_empty() {
                    row["external"] = json!(e.external);
                }
                row
            }).collect::<Vec<_>>(),
            "batches": self.batches.iter().map(|b| json!({
                "phase": b.phase,
                "url": b.url,
                "rev": b.rev,
                "entries": b.entries,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Why boot composition failed. Every variant is fatal: a partially composed
/// graph would hand the browser a shell that throws on the first missing row.
#[derive(Debug, thiserror::Error)]
pub enum BootError {
    /// The dsh package tree could not be read.
    #[error("client-modules: reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A `package.json` was not valid JSON.
    #[error("client-modules: {path} is not valid JSON: {source}")]
    Manifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// A `dsh.client` declaration was malformed. Mirrors `parseDshClient`.
    #[error("client-modules: {0}")]
    BadDeclaration(String),
    /// A bundle patch was unreadable YAML.
    #[error("client-modules: reading {path}: {source}")]
    Patch {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    /// A declared package has no built client bundle. Mirrors
    /// `MissingClientBundleError`: loud, because a silently skipped row leaves
    /// the shell throwing on an unresolved import instead.
    #[error(
        "client-modules: client bundle not found; run `pnpm run build` before launch:\n  \
         package: {package}\n  path: {path}"
    )]
    MissingBundle { package: String, path: PathBuf },
    /// The module graph has a cycle, or a row requests its own package.
    #[error("client-modules: {0}")]
    ModuleGraph(String),
    /// A combo URL would exceed the protocol limit.
    #[error("client-modules: {0} exceeds the {MAX_COMBO_URL_BYTES}-byte combo URL limit")]
    ComboTooLong(String),
}

/// The dsh source tree, read for package manifests and bundle patches.
pub struct DshTree {
    root: PathBuf,
}

/// A composed graph plus the per-entry bundle paths, ready to serve.
pub struct Boot {
    /// The graph served as `window.__DSH_BOOT__`.
    pub graph: WebBootGraph,
    /// Entry id → absolute path of its built `lib/client.js`.
    pub bundles: BTreeMap<String, PathBuf>,
}

impl DshTree {
    /// A tree rooted at `root` (the `dsh/` checkout).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Compose the boot graph.
    ///
    /// Reads the package manifests and the two bundle patches, selects the
    /// roster, and builds the graph. Every failure is fatal and named.
    pub fn compose(&self, backend: PickerBackend) -> Result<Boot, BootError> {
        let manifests = self.package_manifests()?;
        let enabled = self.enabled_patch_names()?;

        // The roster: a package declaring `dsh.client`, named by an enabled
        // bundle-patch row, plus the resolved picker backend (which no patch
        // names — the auto row contributes it at runtime).
        let mut roster: BTreeSet<String> = BTreeSet::new();
        let mut bundles: BTreeMap<String, PathBuf> = BTreeMap::new();
        let mut decls: BTreeMap<String, DshClientDecl> = BTreeMap::new();
        for (name, dir, manifest) in &manifests {
            let Some(client) = manifest.pointer("/dsh/client") else {
                continue;
            };
            let decl = parse_dsh_client(name, client)?;
            let bundle = dir.join("lib").join("client.js");
            // Only packages that both declare and are selected need a bundle;
            // a declared-but-unselected package (a disabled row) may be unbuilt.
            if !enabled.contains(name) {
                continue;
            }
            if !bundle.is_file() {
                return Err(BootError::MissingBundle {
                    package: name.clone(),
                    path: bundle,
                });
            }
            bundles.insert(name.clone(), bundle);
            decls.insert(name.clone(), decl);
            roster.insert(name.clone());
        }
        // The picker backend joins the roster, and its bundle must exist too.
        let picker = backend.package().to_string();
        if let Some((_, dir, manifest)) = manifests.iter().find(|(n, _, _)| *n == picker)
            && manifest.pointer("/dsh/client").is_some()
            && !roster.contains(&picker)
        {
            let decl = parse_dsh_client(&picker, manifest.pointer("/dsh/client").unwrap())?;
            let bundle = dir.join("lib").join("client.js");
            if !bundle.is_file() {
                return Err(BootError::MissingBundle {
                    package: picker.clone(),
                    path: bundle,
                });
            }
            bundles.insert(picker.clone(), bundle);
            decls.insert(picker.clone(), decl);
            roster.insert(picker);
        }

        // Build rows in a deterministic order, then order by the module graph
        // (see the module docs on why the exact upstream order is not matched).
        let mut entries: Vec<WebBootEntry> = roster
            .iter()
            .map(|id| {
                let decl = &decls[id];
                graph_row(id, &entry_rev(id), decl)
            })
            .collect();
        entries = order_by_module_graph(entries)?;

        let batches = partition_batches(&entries)?;
        let rev = short_hash(&serde_json::to_string(&json!({
            "entries": entries.iter().map(|e| json!({ "id": e.id, "rev": e.rev })).collect::<Vec<_>>(),
            "batches": batches.iter().map(|b| json!({ "phase": b.phase, "url": b.url })).collect::<Vec<_>>(),
        })).unwrap_or_default());

        Ok(Boot {
            graph: WebBootGraph {
                rev,
                entries,
                batches,
            },
            bundles,
        })
    }

    /// Every package manifest under `packages/*/*/package.json`, as
    /// `(name, dir, manifest)`.
    fn package_manifests(&self) -> Result<Vec<(String, PathBuf, Value)>, BootError> {
        let mut out = Vec::new();
        let packages = self.root.join("packages");
        let groups = read_dir(&packages)?;
        for group in groups {
            if !group.is_dir() {
                continue;
            }
            for pkg_dir in read_dir(&group)? {
                let manifest_path = pkg_dir.join("package.json");
                if !manifest_path.is_file() {
                    continue;
                }
                let text =
                    std::fs::read_to_string(&manifest_path).map_err(|source| BootError::Io {
                        path: manifest_path.clone(),
                        source,
                    })?;
                let value: Value =
                    serde_json::from_str(&text).map_err(|source| BootError::Manifest {
                        path: manifest_path.clone(),
                        source,
                    })?;
                let Some(name) = value.get("name").and_then(Value::as_str) else {
                    continue;
                };
                out.push((name.to_string(), pkg_dir, value));
            }
        }
        Ok(out)
    }

    /// The names of every `name:` row in the two bundle patches whose final
    /// occurrence is not `disabled: true`.
    ///
    /// Last-occurrence-wins, because a patch replaces the row it targets: a
    /// name enabled in one layer and disabled in a later one is disabled.
    fn enabled_patch_names(&self) -> Result<BTreeSet<String>, BootError> {
        let patches = [
            self.root.join("packages/bundle/base/cordis.patch.yml"),
            self.root.join("packages/bundle/web-app/cordis.patch.yml"),
        ];
        // Ordered last-wins across files: `web-app` is applied after `base`.
        let mut state: BTreeMap<String, bool> = BTreeMap::new();
        for path in patches {
            let text = std::fs::read_to_string(&path).map_err(|source| BootError::Io {
                path: path.clone(),
                source,
            })?;
            let value: serde_yaml::Value =
                serde_yaml::from_str(&text).map_err(|source| BootError::Patch {
                    path: path.clone(),
                    source,
                })?;
            let mut rows = Vec::new();
            collect_rows(&value, &mut rows);
            for row in rows {
                let (Some(name), disabled) = (
                    row.get("name").and_then(|v| v.as_str()),
                    row.get("disabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                ) else {
                    continue;
                };
                state.insert(name.to_string(), !disabled);
            }
        }
        Ok(state
            .into_iter()
            .filter_map(|(name, enabled)| enabled.then_some(name))
            .collect())
    }
}

impl Boot {
    /// The combo response body for `ids`: the script, or its indexed source map.
    ///
    /// `map` selects the `.map` form. The script form concatenates each bundle
    /// with its own source-map trailer stripped and one absolute combo map URL
    /// appended; the map form is an **indexed** Source Map v3 (`{version, file,
    /// sections}`) whose sections are the per-bundle maps with their `sources`
    /// relocated, or an identity map for a bundle that shipped none — it is not
    /// the bundles' maps concatenated, which no consumer could read.
    pub fn combo(&self, ids: &[String], rev: &str, map: bool) -> Result<Vec<u8>, BootError> {
        if map {
            self.combo_map(ids)
        } else {
            self.combo_script(ids, rev)
        }
    }

    /// The executable combo: prepared bundles, then one absolute map URL.
    fn combo_script(&self, ids: &[String], rev: &str) -> Result<Vec<u8>, BootError> {
        let mut out: Vec<u8> = Vec::new();
        for id in ids {
            let prepared = self.prepare(id)?;
            out.extend_from_slice(prepared.source.as_bytes());
            out.extend_from_slice(b";\n");
        }
        out.extend_from_slice(
            format!("//# sourceMappingURL={}\n", combo_url(ids, rev, true)).as_bytes(),
        );
        Ok(out)
    }

    /// The indexed map: one section per bundle, offset to its line in the script.
    fn combo_map(&self, ids: &[String]) -> Result<Vec<u8>, BootError> {
        let mut sections: Vec<Value> = Vec::new();
        let mut line = 0usize;
        for id in ids {
            let prepared = self.prepare(id)?;
            let section = match self.record_map(id)? {
                Some(map) => relocate_section(id, map),
                // A bundle with no map still needs a section, or its lines would
                // shift every later section's offsets.
                None => identity_section(&prepared.source, &prepared.fallback_source),
            };
            sections.push(json!({ "offset": { "line": line, "column": 0 }, "map": section }));
            // The script writes `source` then `;\n`, so the next bundle starts
            // one line past the source's own newline count.
            line += newline_count(&prepared.source) + 1;
        }
        let body = json!({ "version": 3, "file": "client.js", "sections": sections });
        let mut out = serde_json::to_vec(&body).unwrap_or_default();
        out.push(b'\n');
        Ok(out)
    }

    /// Read a bundle and strip its trailers, plus the fallback source URL the
    /// identity-map branch needs.
    fn prepare(&self, id: &str) -> Result<Prepared, BootError> {
        let path = self
            .bundles
            .get(id)
            .ok_or_else(|| BootError::ModuleGraph(format!("unknown bundle id {id}")))?;
        let bytes = std::fs::read(path).map_err(|source| BootError::Io {
            path: path.clone(),
            source,
        })?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        // The URL trailer, then the map trailer, removed in that order
        // (`comboSource` replaces SOURCE_URL then SOURCE_MAP).
        let (after_url, source_url) = match strip_trailer(&text, SOURCE_URL_PREFIX) {
            Some((rest, url)) => (rest.to_string(), Some(url.to_string())),
            None => (text.clone(), None),
        };
        let source = match strip_trailer(&after_url, SOURCE_MAP_PREFIX) {
            Some((rest, _)) => rest.to_string(),
            None => after_url,
        };
        let mut source = source;
        if !source.ends_with('\n') {
            source.push('\n');
        }
        // An absent trailer falls back to the row's own script URL; one that is
        // relative to the bundle is re-rooted at the site root.
        let fallback_source = match source_url.as_deref() {
            None => format!("/plugins/{id}/client.js"),
            Some(url) if url.starts_with('/') || has_url_scheme(url) => url.to_string(),
            Some(url) => format!("/{url}"),
        };
        Ok(Prepared {
            source,
            fallback_source,
        })
    }

    /// A bundle's own source map, parsed, when one shipped beside it.
    fn record_map(&self, id: &str) -> Result<Option<Value>, BootError> {
        let bundle = self
            .bundles
            .get(id)
            .ok_or_else(|| BootError::ModuleGraph(format!("unknown bundle id {id}")))?;
        let path = PathBuf::from(format!("{}{SOURCE_MAP_SUFFIX}", bundle.display()));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(BootError::Io { path, source }),
        };
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|source| BootError::Manifest {
                path: path.clone(),
                source,
            })?;
        Ok(Some(value))
    }

    /// The combo content type for the script or map form.
    pub fn combo_content_type(map: bool) -> &'static str {
        if map {
            "application/json; charset=utf-8"
        } else {
            "text/javascript; charset=utf-8"
        }
    }
}

/// Parse one `dsh.client` declaration. Mirrors `parseDshClient`: `platform` is
/// required, the array fields must be arrays of strings when present, and
/// `immediately` must be a boolean when present.
pub fn parse_dsh_client(pkg: &str, value: &Value) -> Result<DshClientDecl, BootError> {
    let Some(obj) = value.as_object() else {
        return Err(BootError::BadDeclaration(format!(
            "{pkg} has a non-object dsh.client declaration"
        )));
    };
    let Some(platform) = obj.get("platform").and_then(Value::as_str) else {
        return Err(BootError::BadDeclaration(format!(
            "{pkg} dsh.client.platform must be a string"
        )));
    };
    let inject = optional_string_array(pkg, "dsh.client.inject", obj.get("inject"))?;
    let external = optional_string_array(pkg, "dsh.client.external", obj.get("external"))?;
    let immediately = match obj.get("immediately") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(BootError::BadDeclaration(format!(
                "{pkg} dsh.client.immediately must be a boolean"
            )));
        }
    };
    Ok(DshClientDecl {
        platform: platform.to_string(),
        inject,
        external,
        immediately,
    })
}

/// An optional string-array field: absent is empty, present-but-wrong is an
/// error. Mirrors `optionalStringArray`.
fn optional_string_array(
    pkg: &str,
    field: &str,
    value: Option<&Value>,
) -> Result<Vec<String>, BootError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    BootError::BadDeclaration(format!("{pkg} {field} must contain only strings"))
                })
            })
            .collect(),
        Some(_) => Err(BootError::BadDeclaration(format!(
            "{pkg} {field} must be an array of strings"
        ))),
    }
}

/// Build one graph entry. Mirrors `graphRow`.
fn graph_row(id: &str, rev: &str, decl: &DshClientDecl) -> WebBootEntry {
    WebBootEntry {
        id: id.to_string(),
        url: combo_url(&[id.to_string()], rev, false),
        rev: rev.to_string(),
        inject: decl.inject.clone(),
        immediately: decl.immediately,
        external: decl.external.clone(),
    }
}

/// A deterministic per-entry revision.
///
/// Upstream uses a per-boot random nonce; the value is an opaque cache key the
/// combo route resolves by exact URL, so a hash of the id is equally valid and
/// stable across restarts.
fn entry_rev(id: &str) -> String {
    short_hash(id)
}

/// Order rows so every requested package precedes its consumers. Mirrors
/// `orderByModuleGraph`; a cycle or a self-request is an error.
fn order_by_module_graph(entries: Vec<WebBootEntry>) -> Result<Vec<WebBootEntry>, BootError> {
    // Adjacency by owned index, so the DFS borrows nothing from `entries`.
    let index_of: BTreeMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.as_str(), i))
        .collect();
    let deps: Vec<Vec<usize>> = entries
        .iter()
        .map(|e| {
            e.external
                .iter()
                .filter_map(|name| {
                    index_of
                        .get(name.as_str())
                        .or_else(|| index_of.get(strip_client_suffix(name)))
                        .copied()
                })
                .collect()
        })
        .collect();
    // A row that requests its own package is a declaration error, not a cycle.
    for (i, e) in entries.iter().enumerate() {
        for name in &e.external {
            if index_of.get(name.as_str()).copied() == Some(i)
                || index_of.get(strip_client_suffix(name)).copied() == Some(i)
            {
                return Err(BootError::ModuleGraph(format!(
                    "\"{}\" requests module \"{name}\" that it answers itself — a row must not \
                     declare its own package in dsh.client.external",
                    e.id
                )));
            }
        }
    }

    let mut placed = vec![false; entries.len()];
    let mut open: Vec<usize> = Vec::new();
    let mut ordered: Vec<usize> = Vec::new();
    // Iterative post-order DFS: `(node, next dependency)` on the work stack,
    // entering a node on its first frame and emitting it once its deps are done.
    for root in 0..entries.len() {
        if placed[root] {
            continue;
        }
        let mut stack = vec![(root, 0usize)];
        while let Some((node, next)) = stack.pop() {
            if next == 0 {
                if placed[node] {
                    continue;
                }
                if let Some(start) = open.iter().position(|n| *n == node) {
                    let cycle: Vec<String> = open[start..]
                        .iter()
                        .map(|n| entries[*n].id.clone())
                        .chain(std::iter::once(entries[node].id.clone()))
                        .collect();
                    return Err(BootError::ModuleGraph(format!(
                        "module graph cycle {} — a requested package row must precede its \
                         consumers, and factory-form CJS cannot deliver partial exports",
                        cycle.join(" -> ")
                    )));
                }
                open.push(node);
            }
            if next < deps[node].len() {
                stack.push((node, next + 1));
                let dep = deps[node][next];
                if !placed[dep] {
                    stack.push((dep, 0));
                }
                continue;
            }
            open.pop();
            placed[node] = true;
            ordered.push(node);
        }
    }
    let mut by_index: Vec<Option<WebBootEntry>> = entries.into_iter().map(Some).collect();
    Ok(ordered
        .into_iter()
        .filter_map(|i| by_index[i].take())
        .collect())
}

/// `<id>/client` names the same exports as `<id>`. Mirrors `stripClientSuffix`.
fn strip_client_suffix(spec: &str) -> &str {
    spec.strip_suffix("/client").unwrap_or(spec)
}

/// Partition rows into batches: `client-modules` alone bootstraps, the rest are
/// preloaded; a batch splits when its map-form URL would exceed the limit.
fn partition_batches(entries: &[WebBootEntry]) -> Result<Vec<WebBootBatch>, BootError> {
    let bootstrap: Vec<&WebBootEntry> = entries
        .iter()
        .filter(|e| e.id == CLIENT_MODULES_ID)
        .collect();
    let application: Vec<&WebBootEntry> = entries
        .iter()
        .filter(|e| e.id != CLIENT_MODULES_ID)
        .collect();
    let mut out = Vec::new();
    for (phase, group) in [("bootstrap", bootstrap), ("application", application)] {
        if group.is_empty() {
            continue;
        }
        for chunk in partition_combo(&group)? {
            let ids: Vec<String> = chunk.iter().map(|e| e.id.clone()).collect();
            let url = combo_url(&ids, "", false);
            let rev = short_hash(&url);
            // The URL carries the placeholder rev that `combo_url` fills with
            // an empty string; the served rev is this batch's own hash.
            let url = combo_url(&ids, &rev, false);
            out.push(WebBootBatch {
                phase,
                url,
                rev,
                entries: ids,
            });
        }
    }
    Ok(out)
}

/// Split rows so no generated map-form URL exceeds the limit. Mirrors
/// `partitionComboRecords`.
fn partition_combo<'a>(rows: &[&'a WebBootEntry]) -> Result<Vec<Vec<&'a WebBootEntry>>, BootError> {
    let mut chunks = Vec::new();
    let mut current: Vec<&WebBootEntry> = Vec::new();
    for row in rows {
        let mut candidate = current.clone();
        candidate.push(row);
        if projected_combo_bytes(&candidate) <= MAX_COMBO_URL_BYTES {
            current = candidate;
            continue;
        }
        if current.is_empty() {
            return Err(BootError::ComboTooLong(row.id.clone()));
        }
        chunks.push(std::mem::take(&mut current));
        if projected_combo_bytes(&[row]) > MAX_COMBO_URL_BYTES {
            return Err(BootError::ComboTooLong(row.id.clone()));
        }
        current.push(row);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    Ok(chunks)
}

/// The longest (map-form, placeholder-rev) URL a row list would generate.
fn projected_combo_bytes(rows: &[&WebBootEntry]) -> usize {
    let ids: Vec<String> = rows.iter().map(|e| e.id.clone()).collect();
    combo_url(&ids, &"0".repeat(HASH_REVISION_LENGTH), true).len()
}

/// The combo route URL for an ordered id list. Mirrors `comboUrl`.
fn combo_url(ids: &[String], rev: &str, map: bool) -> String {
    let resources = ids
        .iter()
        .map(|id| {
            if map {
                format!("{id}/client.js{SOURCE_MAP_SUFFIX}")
            } else {
                format!("{id}/client.js")
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("/plugins/??{resources}&rev={rev}")
}

/// A bundle prepared for the combo script: trailers stripped, one trailing
/// newline, and the debugger source URL the identity map needs.
struct Prepared {
    source: String,
    fallback_source: String,
}

/// Prefix of the tsdown source-map trailer (`SOURCE_MAP_TRAILER`).
const SOURCE_MAP_PREFIX: &str = "//# sourceMappingURL=";
/// Prefix of the debugger source-name trailer (`SOURCE_URL_TRAILER`).
const SOURCE_URL_PREFIX: &str = "//# sourceURL=";

/// Split a trailing `//# <prefix><value>` line off `text`.
///
/// Mirrors the anchored regexes: the trailer is the final line, optionally
/// preceded by a newline (which the removal consumes) and optionally followed by
/// one. Returns the remaining text and the captured value.
fn strip_trailer<'a>(text: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    let body = body.strip_suffix('\r').unwrap_or(body);
    let line_start = body.rfind('\n').map_or(0, |i| i + 1);
    let value = body[line_start..].strip_prefix(prefix)?;
    // Drop the newline that preceded the trailer, matching the regex's optional
    // leading `\r?\n`; a trailer on its own first line leaves nothing before it.
    let remainder = if line_start == 0 {
        ""
    } else {
        &text[..line_start - 1]
    };
    Some((remainder, value))
}

/// Whether `url` begins with a scheme (`[A-Za-z][A-Za-z\d+.-]*:`), mirroring the
/// absolute-URL arm of the fallback-source test.
fn has_url_scheme(url: &str) -> bool {
    let mut chars = url.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')) {
            return false;
        }
    }
    false
}

/// Relocate a bundle map's `sources` to their combo URLs and drop its
/// `sourceRoot`. Mirrors `comboSectionMap`.
fn relocate_section(id: &str, mut map: Value) -> Value {
    let base = format!("/plugins/{id}/client.js{SOURCE_MAP_SUFFIX}");
    let Some(object) = map.as_object_mut() else {
        return map;
    };
    let source_root = object
        .get("sourceRoot")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let Some(sources) = object.get("sources").and_then(Value::as_array).cloned() else {
        return map;
    };
    let relocated: Vec<Value> = sources
        .iter()
        .map(|source| {
            let source = source.as_str().unwrap_or("");
            let separator = if !source_root.is_empty()
                && !source_root.ends_with('/')
                && !source.starts_with('/')
            {
                "/"
            } else {
                ""
            };
            json!(resolve_against(
                &base,
                &format!("{source_root}{separator}{source}")
            ))
        })
        .collect();
    object.insert("sources".to_string(), Value::Array(relocated));
    object.remove("sourceRoot");
    map
}

/// The identity map for a bundle with no map of its own. Mirrors
/// `identitySectionMap`: every generated line maps back to the same line of the
/// bundle, which is carried whole as `sourcesContent`.
fn identity_section(source: &str, source_url: &str) -> Value {
    let mappings = (0..newline_count(source))
        .map(|index| if index == 0 { "AAAA" } else { "AACA" })
        .collect::<Vec<_>>()
        .join(";");
    json!({
        "version": 3,
        "names": [],
        "sources": [source_url],
        "sourcesContent": [source],
        "mappings": mappings,
    })
}

/// The number of newlines in `text`.
fn newline_count(text: &str) -> usize {
    text.bytes().filter(|b| *b == b'\n').count()
}

/// Resolve `reference` against the absolute `base` URL and return the resulting
/// path. Mirrors `new URL(reference, base)` followed by `.pathname` for the
/// same-origin case, which every source-map entry is.
fn resolve_against(base: &str, reference: &str) -> String {
    if has_url_scheme(reference) {
        return reference.to_string();
    }
    // A `?query` / `#hash` suffix is preserved but does not participate in path
    // resolution.
    let cut = reference.find(['?', '#']).unwrap_or(reference.len());
    let (path, suffix) = reference.split_at(cut);
    let merged = if path.starts_with('/') {
        path.to_string()
    } else {
        let dir = match base.rfind('/') {
            Some(i) => &base[..=i],
            None => "/",
        };
        format!("{dir}{path}")
    };
    format!("{}{suffix}", remove_dot_segments(&merged))
}

/// RFC 3986 §5.2.4 path normalization.
fn remove_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let absolute = path.starts_with('/');
    let trailing_slash = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    let mut result = String::new();
    if absolute {
        result.push('/');
    }
    result.push_str(&out.join("/"));
    if trailing_slash && !result.ends_with('/') {
        result.push('/');
    }
    result
}

/// A short hex digest. FNV-1a rather than sha1: see the module docs — the value
/// is an opaque cache key, never a wire contract, so a dependency is not earned.
fn short_hash(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")[..HASH_REVISION_LENGTH].to_string()
}

/// Read a directory, sorted, as paths.
fn read_dir(path: &Path) -> Result<Vec<PathBuf>, BootError> {
    let entries = std::fs::read_dir(path).map_err(|source| BootError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| BootError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        out.push(entry.path());
    }
    out.sort();
    Ok(out)
}

/// Collect every mapping under `value` that could be a patch row (has a `name`
/// or `id`), descending through `insert`/`patch` lists. The patch format nests
/// new rows under `insert:` and edits under the top level.
fn collect_rows(value: &serde_yaml::Value, out: &mut Vec<BTreeMap<String, serde_yaml::Value>>) {
    match value {
        serde_yaml::Value::Sequence(items) => {
            for item in items {
                collect_rows(item, out);
            }
        }
        serde_yaml::Value::Mapping(map) => {
            let mut row = BTreeMap::new();
            for (k, v) in map {
                if let Some(key) = k.as_str() {
                    row.insert(key.to_string(), v.clone());
                }
            }
            if row.contains_key("name") || row.contains_key("id") {
                out.push(row);
            }
            for key in ["insert", "patch"] {
                if let Some(child) = map.get(serde_yaml::Value::String(key.to_string())) {
                    collect_rows(child, out);
                }
            }
        }
        _ => {}
    }
}

/// The module-system facade, byte-identical to upstream's
/// (`packages/client/modules/src/index.ts`, `bootInjections`' `queue` row).
///
/// Copied rather than paraphrased because it *is* the wire: it installs the
/// `__ModuleLoader__` the shell reads, and the error strings inside it are what
/// a broken boot reports. A paraphrase that drifted would move the failure the
/// browser shows without changing anything a test here could see.
pub const FACADE_SCRIPT: &str = r#"(()=>{
const pendingQueue=[]
window.__ModuleLoader__={
  mode:"queue",
  pendingQueue,
  load(registration){pendingQueue.push(registration)},
  create(options){
    if(this.mode!=="queue")throw new Error("client-modules: window.__ModuleLoader__.create called after module-system boot")
    const index=pendingQueue.findIndex(registration=>registration.id==="@deepseek-ai/dsh-client-modules")
    const registration=pendingQueue[index]
    if(registration===undefined)throw new Error("client-modules: HTML did not preload @deepseek-ai/dsh-client-modules/client.js")
    pendingQueue.splice(index,1)
    const exports=registration.factory(specifier=>{
      throw new Error('client-modules: @deepseek-ai/dsh-client-modules/client.js requested external "'+specifier+'" before the module system existed')
    })
    if(typeof exports!=="object"||exports===null||typeof exports.createClientModuleSystem!=="function"||typeof exports.apply!=="function"){
      throw new Error("client-modules: @deepseek-ai/dsh-client-modules/client.js did not export the bootstrap module face")
    }
    return exports.createClientModuleSystem(this,{id:registration.id,exports},options)
  }
}
})()"#;

/// The theme bootstrap script for the pre-plugin interval, as upstream's
/// `bootThemeInjection` renders it at the default preference and font size
/// (`packages/client/ui-theme/src/boot-theme.ts`).
pub fn theme_script() -> String {
    // Defaults: `system` preference, 14px content font
    // (`theme-settings.ts`'s `DEFAULT_PREFERENCE` / `DEFAULT_FONT_SIZE`).
    let preference = "system";
    let font_px = "14px";
    format!(
        r#"(() => {{
  const preference = {preference:?}
  const systemDark = preference === 'system'
    && typeof matchMedia !== 'undefined'
    && matchMedia('(prefers-color-scheme: dark)').matches
  const dark = preference === 'dark' || systemDark
  document.documentElement.style.colorScheme = dark ? 'dark' : 'light'
  document.body.toggleAttribute('data-ds-dark-theme', dark)
  document.body.style.setProperty('--dsh-content-font-size', {font_px:?})
}})()"#
    )
}

/// The connection-recovery global, at the schema defaults
/// (`packages/client/connection/src/recovery-config.ts`).
pub fn connection_recovery() -> Value {
    json!({
        "backoffBaseMs": 500,
        "backoffFactor": 2,
        "backoffMaxMs": 10000,
        "generationReadyWarnMs": 3000,
        "generationReadyTimeoutMs": 15000,
    })
}

/// The boot-readiness tail. Mirrors `READY_MARKUP` in
/// `packages/host/webserver/src/injections.ts`.
const READY_MARKUP: &str =
    "<script>(globalThis.__DSH_BOOT_READY__ ??= Promise.withResolvers()).resolve()</script>";

/// Escape a value for a quoted HTML attribute. Mirrors `escapeHtmlAttribute`.
fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The JSON for a `globalThis[...]` assignment, with `<` escaped so a
/// row-controlled string cannot close the script element. Mirrors the `global`
/// arm of `renderRow`.
fn global_assignment(name: &str, value: &Value) -> String {
    let name_json = serde_json::to_string(name)
        .unwrap_or_default()
        .replace('<', "\\u003c");
    let value_json = serde_json::to_string(value)
        .unwrap_or_default()
        .replace('<', "\\u003c");
    format!("<script>globalThis[{name_json}] = {value_json}</script>")
}

/// Splice the boot injection table into an index.html body, and return it.
///
/// The rows land exactly where upstream's `renderIndexInjections` puts them:
/// head rows immediately after `<head>` (or prepended when the page has none),
/// body rows immediately after `<body>` (or appended), and the readiness tail
/// after the last body row. `<base href="/">` is added first, mirroring
/// `frontend-static`'s `renderIndex`: the dist is built with a relative base, so
/// a deep SPA-fallback path would otherwise resolve asset URLs under the
/// request directory.
pub fn inject_into_index(html: &str, graph: &WebBootGraph) -> String {
    // The order upstream emits, from `bootInjections`:
    //   facade → application preloads → bootstrap scripts → graph global.
    let mut head = String::new();
    head.push_str(&format!("<script>{FACADE_SCRIPT}</script>"));
    for batch in graph.batches.iter().filter(|b| b.phase == "application") {
        head.push_str(&format!(
            "<link rel=\"preload\" as=\"script\" href=\"{}\">",
            escape_attr(&batch.url)
        ));
    }
    for batch in graph.batches.iter().filter(|b| b.phase == "bootstrap") {
        head.push_str(&format!(
            "<script src=\"{}\"></script>",
            escape_attr(&batch.url)
        ));
    }
    head.push_str(&global_assignment("__DSH_BOOT__", &graph.to_json()));
    head.push_str(&global_assignment(
        "__DSH_CONNECTION_RECOVERY__",
        &connection_recovery(),
    ));

    let mut body = String::new();
    body.push_str(&format!("<script>{}</script>", theme_script()));
    body.push_str(READY_MARKUP);

    let mut out = html.to_string();
    // Head rows first, then the base tag at the same offset — inserting last
    // lands first, so the base precedes every URL-bearing row.
    if let Some(open) = find_tag(&out, "head") {
        out.insert_str(open, &head);
    } else {
        out.insert_str(0, &head);
    }
    if let Some(open) = find_tag(&out, "head") {
        out.insert_str(open, "<base href=\"/\">");
    }
    if let Some(open) = find_tag(&out, "body") {
        out.insert_str(open, &body);
    } else {
        out.push_str(&body);
    }
    out
}

/// The offset just past the first opening `<tag>` (case-insensitive), or `None`
/// when the document has no such tag. Mirrors `renderIndexInjections`' splice.
fn find_tag(html: &str, tag: &str) -> Option<usize> {
    let lower = html.to_ascii_lowercase();
    let needle = format!("<{tag}");
    let start = lower.find(&needle)?;
    let rest = &html[start..];
    let close = rest.find('>')?;
    Some(start + close + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dsh checkout at the repo root, as every other replay test locates it.
    fn tree() -> DshTree {
        DshTree::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../dsh"))
    }

    fn composed() -> Boot {
        tree()
            .compose(PickerBackend::Native)
            .expect("the dsh tree composes")
    }

    /// The picker is the one roster member no bundle patch names.
    const PICKER: &str = "@deepseek-ai/dsh-client-ui-directory-picker-native";

    #[test]
    fn the_roster_is_the_enabled_patch_intersection_plus_the_resolved_picker() {
        let boot = composed();
        let ids: BTreeSet<&str> = boot.graph.entries.iter().map(|e| e.id.as_str()).collect();
        // The captured control serves 53 entries: 52 named by an enabled patch
        // row and declaring `dsh.client`, plus the runtime-resolved picker.
        assert_eq!(ids.len(), 53, "roster size");
        assert!(ids.contains(PICKER), "the resolved picker joins the roster");
        // Its sibling backend is *not* mounted — only the resolved one is.
        assert!(
            !ids.contains("@deepseek-ai/dsh-client-ui-directory-picker-browse"),
            "only the resolved picker backend is mounted"
        );
    }

    #[test]
    fn a_package_whose_patch_row_is_disabled_is_absent() {
        // `dsh-client-ui-schedule` declares `dsh.client` but its web-app row is
        // `disabled: true`; the disable must win over the declaration.
        let boot = composed();
        let ids: BTreeSet<&str> = boot.graph.entries.iter().map(|e| e.id.as_str()).collect();
        assert!(
            !ids.contains("@deepseek-ai/dsh-client-ui-schedule"),
            "a disabled patch row excludes the package regardless of its declaration"
        );
    }

    #[test]
    fn every_row_has_a_built_bundle() {
        let boot = composed();
        for entry in &boot.graph.entries {
            let path = boot
                .bundles
                .get(&entry.id)
                .unwrap_or_else(|| panic!("no bundle for {}", entry.id));
            assert!(path.is_file(), "bundle {} missing at {path:?}", entry.id);
        }
    }

    #[test]
    fn the_graph_is_closed() {
        // Every `inject` and `external` target either names a row or a module the
        // shell itself provides; an edge to neither is a boot-time throw.
        let boot = composed();
        let ids: BTreeSet<&str> = boot.graph.entries.iter().map(|e| e.id.as_str()).collect();
        let static_modules: BTreeSet<&str> = STATIC_MODULES.iter().copied().collect();
        for entry in &boot.graph.entries {
            for target in entry.inject.iter().chain(entry.external.iter()) {
                let normalized = strip_client_suffix(target);
                assert!(
                    ids.contains(normalized) || static_modules.contains(normalized),
                    "\"{}\" requests \"{target}\" which is neither a row nor a static module",
                    entry.id
                );
            }
        }
    }

    #[test]
    fn every_entry_sits_in_exactly_one_batch_and_the_bootstrap_is_client_modules() {
        let boot = composed();
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for batch in &boot.graph.batches {
            for id in &batch.entries {
                if let Some(previous) = seen.insert(id, batch.phase) {
                    panic!(
                        "\"{id}\" appears in both a {previous} and a {} batch",
                        batch.phase
                    );
                }
            }
        }
        assert_eq!(
            seen.len(),
            boot.graph.entries.len(),
            "every entry batched once"
        );
        // Exactly one bootstrap batch, and it is the module-system package alone.
        let bootstrap: Vec<&WebBootBatch> = boot
            .graph
            .batches
            .iter()
            .filter(|b| b.phase == "bootstrap")
            .collect();
        assert_eq!(bootstrap.len(), 1, "one bootstrap batch");
        assert_eq!(bootstrap[0].entries, vec![CLIENT_MODULES_ID.to_string()]);
        // Both phases are non-empty; the application batch is the rest.
        assert!(boot.graph.batches.iter().any(|b| b.phase == "application"));
    }

    #[test]
    fn generated_combo_urls_stay_within_the_protocol_limit() {
        let boot = composed();
        for batch in &boot.graph.batches {
            let ids: Vec<String> = batch.entries.clone();
            let map_len = combo_url(&ids, &"0".repeat(HASH_REVISION_LENGTH), true).len();
            assert!(
                map_len <= MAX_COMBO_URL_BYTES,
                "batch {} map URL is {map_len} bytes",
                batch.phase
            );
            assert!(batch.url.starts_with("/plugins/??"));
        }
    }

    #[test]
    fn the_module_graph_orders_every_consumer_after_its_dependency() {
        // The graph is topologically sorted, so a row that another row requests
        // through `external` must precede it.
        let boot = composed();
        let position: BTreeMap<&str, usize> = boot
            .graph
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.as_str(), i))
            .collect();
        for entry in &boot.graph.entries {
            for target in &entry.external {
                if let Some(&target_pos) = position.get(strip_client_suffix(target)) {
                    let here = position[entry.id.as_str()];
                    assert!(
                        target_pos < here,
                        "\"{}\" at {here} precedes its dependency \"{target}\" at {target_pos}",
                        entry.id
                    );
                }
            }
        }
    }

    #[test]
    fn the_facade_script_is_byte_identical_to_upstream() {
        // The facade *is* the wire: it installs the `__ModuleLoader__` the shell
        // reads. Extract upstream's template, substitute the same constants, and
        // require an exact match so the two cannot drift.
        let source_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../dsh/packages/client/modules/src/index.ts");
        let source = std::fs::read_to_string(&source_path)
            .unwrap_or_else(|e| panic!("reading {source_path:?}: {e}"));
        let start = source
            .find("const queue = `")
            .expect("upstream queue template")
            + "const queue = `".len();
        let end = start + source[start..].find('`').expect("closing backtick");
        let upstream = source[start..end]
            .replace("${bootstrapId}", &format!("{CLIENT_MODULES_ID:?}"))
            .replace("${CLIENT_MODULES_ID}", CLIENT_MODULES_ID);
        assert_eq!(
            upstream, FACADE_SCRIPT,
            "the facade must not drift from upstream"
        );
    }

    #[test]
    fn inject_into_index_places_the_facade_first_and_the_ready_tail_last() {
        let boot = composed();
        let html = "<!doctype html><html><head><title>t</title></head><body><div id=root></div></body></html>";
        let out = inject_into_index(html, &boot.graph);
        // Base tag precedes every URL-bearing row.
        let base = out.find("<base href=\"/\">").expect("base tag");
        let facade = out.find(FACADE_SCRIPT).expect("facade");
        let graph = out.find("__DSH_BOOT__").expect("graph global");
        let ready = out.find("__DSH_BOOT_READY__").expect("ready tail");
        assert!(base < facade, "base precedes the facade");
        assert!(facade < graph, "facade precedes the graph global");
        assert!(graph < ready, "graph global precedes the ready tail");
        // The graph carries its real contents, not a stub.
        assert_eq!(
            boot.graph.to_json()["entries"].as_array().unwrap().len(),
            boot.graph.entries.len()
        );
    }

    #[test]
    fn combo_concatenates_bundles_and_appends_one_map_url() {
        let boot = composed();
        let ids = vec![CLIENT_MODULES_ID.to_string()];
        let body = boot.combo(&ids, "abc123", false).expect("combo");
        let text = String::from_utf8(body).expect("utf8");
        // The bundle's own relative map trailer is stripped; exactly one
        // absolute combo map URL is appended.
        assert!(
            !text.contains("//# sourceMappingURL=./"),
            "relative map stripped"
        );
        assert!(
            text.contains("//# sourceMappingURL=/plugins/??"),
            "absolute combo map appended"
        );
        assert_eq!(text.matches("//# sourceMappingURL=").count(), 1);
        assert!(text.contains("&rev=abc123"));
    }

    #[test]
    fn combo_of_an_unknown_id_is_an_error() {
        let boot = composed();
        let err = boot
            .combo(&["@deepseek-ai/dsh-not-a-package".to_string()], "", false)
            .expect_err("unknown id");
        assert!(matches!(err, BootError::ModuleGraph(_)), "got {err:?}");
    }

    #[test]
    fn the_map_form_is_served_as_json() {
        let boot = composed();
        let _ = &boot;
        assert_eq!(
            Boot::combo_content_type(false),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            Boot::combo_content_type(true),
            "application/json; charset=utf-8"
        );
    }

    #[test]
    fn the_picker_resolver_table_drives_every_branch() {
        let facts =
            |bind: &str, platform: &str, ssh: bool, display: bool, chooser: bool| PickerFacts {
                bind_host: bind.to_string(),
                platform: platform.to_string(),
                ssh,
                display,
                linux_chooser: chooser,
            };
        // A non-loopback bind never mounts the native picker.
        assert_eq!(
            resolve_picker_backend(&facts("0.0.0.0", "linux", false, true, true)),
            PickerBackend::Browse
        );
        // An SSH launch means the operator cannot see the host display.
        assert_eq!(
            resolve_picker_backend(&facts("127.0.0.1", "linux", true, true, true)),
            PickerBackend::Browse
        );
        // macOS and Windows have native choosers.
        assert_eq!(
            resolve_picker_backend(&facts("127.0.0.1", "macos", false, false, false)),
            PickerBackend::Native
        );
        assert_eq!(
            resolve_picker_backend(&facts("127.0.0.1", "windows", false, false, false)),
            PickerBackend::Native
        );
        // Linux needs a chooser binary, then a display session.
        assert_eq!(
            resolve_picker_backend(&facts("127.0.0.1", "linux", false, true, false)),
            PickerBackend::Browse
        );
        assert_eq!(
            resolve_picker_backend(&facts("127.0.0.1", "linux", false, false, true)),
            PickerBackend::Browse
        );
        assert_eq!(
            resolve_picker_backend(&facts("127.0.0.1", "linux", false, true, true)),
            PickerBackend::Native
        );
    }

    #[test]
    fn a_wrong_typed_declaration_is_an_error_not_a_default() {
        // `platform` is required.
        assert!(parse_dsh_client("p", &json!({})).is_err());
        // Present-but-wrong-type array is an error, not an empty default.
        assert!(parse_dsh_client("p", &json!({"platform": "web", "inject": "x"})).is_err());
        assert!(parse_dsh_client("p", &json!({"platform": "web", "external": [1]})).is_err());
        assert!(parse_dsh_client("p", &json!({"platform": "web", "immediately": "yes"})).is_err());
        // Absent optional fields default cleanly.
        let decl = parse_dsh_client("p", &json!({"platform": "web"})).expect("minimal declaration");
        assert!(decl.inject.is_empty() && decl.external.is_empty() && !decl.immediately);
    }
}
