//! The `skills` namespace: a session-addressed, cold-readable skill catalog.
//!
//! Upstream (`dsh/packages/api/session-controller/src/skill-catalog.ts`) is a
//! thin Remote over the composed `ctx.skills` registry, which
//! `dsh-skill-filesystem` populates by scanning roots for `SKILL.md` files.
//! vocoderd mirrors the filesystem provider, which is what a self-hosted
//! deployment composes.
//!
//! **Cold-readable** is the load-bearing word: `skills/list` resolves the
//! session's `cwd` from its log rather than activating an agent, so listing
//! skills never runs a turn.
//!
//! **The roots**, in precedence order (an earlier root wins a duplicate name):
//!
//! ```text
//! <project>/.dsh/skills      project-dsh
//! <project>/.agents/skills   project-agents
//! $DSH_HOME/skills           user-dsh     (`.system` skipped)
//! ~/.agents/skills           user-agents
//! ```
//!
//! where `<project>` is the nearest ancestor of the session's cwd holding a
//! `.git` entry. A project whose ancestors hold none falls back to the cwd
//! itself, which is what makes a bare temp directory still discover its own
//! `.dsh/skills`.
//!
//! **The entry shape** comes from parsing each `SKILL.md`'s YAML frontmatter:
//! `name` and `description` are required and `name` must be kebab-case, or the
//! file is skipped (matching upstream, which warns and ignores rather than
//! failing the whole listing). Invocation flags invert twice — the frontmatter
//! spells the *negations*: `disable-model-invocation` and `user-invocable`.
//! Only user-invocable skills are listed here, since this catalog feeds the
//! human-facing composer.

use std::collections::BTreeMap;

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine, RealizeRequest};

use crate::machines::readcache::{FsCache, InFlight, Pending};
use crate::machines::session::SessionStore;
use crate::rpc;

/// The kebab-case skill-name grammar (`dsh/packages/skill/skill`).
fn is_skill_name(name: &str) -> bool {
    let mut segments = name.split('-');
    let Some(first) = segments.next() else {
        return false;
    };
    if first.is_empty()
        || !first
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return false;
    }
    segments.all(|s| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    })
}

/// One parsed skill, before projection to the wire entry.
struct Skill {
    name: String,
    description: String,
    when_to_use: Option<String>,
    model_invocable: bool,
    user_invocable: bool,
    path: String,
}

/// Parse a `SKILL.md`'s YAML frontmatter into a [`Skill`].
///
/// Returns `None` for anything upstream warns-and-ignores: no frontmatter, a
/// malformed block, a missing `name`/`description`, a name outside the
/// kebab-case grammar, or a legacy invocation key. None of these are errors —
/// a bad skill file must not take the catalog down with it.
fn parse_skill(path: &str, text: &str) -> Option<Skill> {
    let rest = text.strip_prefix("---")?;
    // The block ends at the next line that is exactly `---`.
    let mut frontmatter = String::new();
    let mut body_at = None;
    for (i, line) in rest.split('\n').enumerate() {
        // The opening `---` was on the first line, so its remainder is empty;
        // a non-empty remainder means the file did not start with a bare fence.
        if i == 0 && !line.trim().is_empty() {
            return None;
        }
        if i > 0 && line.trim_end() == "---" {
            body_at = Some(i + 1);
            break;
        }
        if i > 0 {
            frontmatter.push_str(line);
            frontmatter.push('\n');
        }
    }
    // No closing fence: not a frontmatter block.
    body_at?;
    let data: serde_yaml::Value = serde_yaml::from_str(&frontmatter).ok()?;
    let map = data.as_mapping()?;

    let field =
        |key: &str| -> Option<&str> { map.get(key).and_then(|v| v.as_str()).map(str::trim) };
    // The canonical negated flags are refused loudly upstream.
    for legacy in ["disableModelInvocation", "modelInvocable", "userInvocable"] {
        if map.contains_key(legacy) {
            return None;
        }
    }
    let name = field("name").filter(|n| !n.is_empty())?;
    if !is_skill_name(name) {
        return None;
    }
    let description = field("description").filter(|d| !d.is_empty())?;

    // Defaults are both-enabled; the frontmatter spells the negations.
    let model_invocable = !frontmatter_bool(map, "disable-model-invocation").unwrap_or(false);
    let user_invocable = frontmatter_bool(map, "user-invocable").unwrap_or(true);

    Some(Skill {
        name: name.to_string(),
        description: description.to_string(),
        when_to_use: field("whenToUse")
            .filter(|w| !w.is_empty())
            .map(str::to_string),
        model_invocable,
        user_invocable,
        path: path.to_string(),
    })
}

/// A frontmatter boolean, accepting the same spellings upstream does.
fn frontmatter_bool(map: &serde_yaml::Mapping, key: &str) -> Option<bool> {
    let v = map.get(key)?;
    if let Some(b) = v.as_bool() {
        return Some(b);
    }
    if let Some(n) = v.as_i64() {
        return match n {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
    }
    let s = v.as_str()?.to_ascii_lowercase();
    match s.as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

pub struct SkillsMachine {
    sessions: SessionStore,
    cache: FsCache,
    pending: Option<Pending>,
    effects: u64,
    /// `$DSH_HOME`, for the user skill root.
    home: String,
    /// The `~/.agents` root.
    agents_home: String,
}

impl SkillsMachine {
    pub fn new(sessions_root: std::path::PathBuf, home: String, agents_home: String) -> Self {
        Self {
            sessions: SessionStore::new(sessions_root),
            cache: FsCache::default(),
            pending: None,
            effects: 0,
            home,
            agents_home,
        }
    }
}

impl PluginMachine for SkillsMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::EffectResult { result, .. } = ev {
            if self.pending.is_none() {
                return vec![];
            }
            if self.cache.absorb(result) {
                return rpc::err("gateway/internal", "skills: effect failed");
            }
            let method = self.pending.as_ref().unwrap().method.clone();
            let req = self.pending.as_ref().unwrap().req.clone();
            return self.dispatch(&method, &req);
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("skills") {
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

impl SkillsMachine {
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
                format!("unsupported skills method: {other}"),
            )),
        }
    }

    fn list(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        // Typert delivers the request body under `request`.
        let request = args.get("request").cloned().unwrap_or_else(|| args.clone());
        let session_id = rpc::arg_str(&request, "sessionId")
            .unwrap_or_default()
            .to_string();
        if session_id.is_empty() {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                "invalid payload for skills.list",
                serde_json::json!({}),
            ));
        }
        // Resolve the session's cwd from its log — cold, no agent activation.
        let Some(cwd) = self.session_cwd(&session_id)? else {
            return Ok(rpc::err_details(
                "session/not-found",
                format!("session \"{session_id}\" not found"),
                serde_json::json!({ "sessionId": session_id }),
            ));
        };
        if cwd.is_empty() {
            return Ok(rpc::err(
                "gateway/internal",
                format!("session \"{session_id}\" has no project cwd"),
            ));
        }

        let roots = self.skill_roots(&cwd)?;
        self.collect(&roots)
    }

    /// The session's recorded `cwd`, or `None` when the session is unknown.
    fn session_cwd(&mut self, session_id: &str) -> Result<Option<String>, Vec<MachineOut>> {
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

    /// The four skill roots for a cwd, in precedence order.
    fn skill_roots(
        &mut self,
        cwd: &str,
    ) -> Result<Vec<(String, &'static str, bool)>, Vec<MachineOut>> {
        // The project root is the nearest ancestor holding a `.git` entry,
        // falling back to the cwd so a non-repository directory still
        // discovers its own `.dsh/skills`. One `ListDirDetailed` per level,
        // via the shared cache.
        let mut ancestors: Vec<String> = Vec::new();
        {
            let mut current = std::path::PathBuf::from(cwd);
            loop {
                let here = current.to_string_lossy().to_string();
                let entries = self.dir_of(&here)?;
                if entries.iter().any(|e| e.name == ".git") {
                    ancestors.push(here);
                    break;
                }
                match current.parent() {
                    Some(p) if !p.as_os_str().is_empty() => current = p.to_path_buf(),
                    _ => break,
                }
            }
        }
        let project = ancestors
            .first()
            .cloned()
            .unwrap_or_else(|| cwd.to_string());
        Ok(vec![
            (format!("{project}/.dsh/skills"), "project-dsh", false),
            (format!("{project}/.agents/skills"), "project-agents", false),
            (format!("{}/skills", self.home), "user-dsh", true),
            (format!("{}/skills", self.agents_home), "user-agents", false),
        ])
    }

    /// The root's immediate entries, via the shared cache.
    fn dir_of(&mut self, dir: &str) -> Result<Vec<vocoder_cordis::DirEntry>, Vec<MachineOut>> {
        if let Some(hit) = self.cache.dirs.get(dir) {
            return Ok(hit.clone());
        }
        if self.cache.failed.contains_key(dir) {
            return Ok(Vec::new());
        }
        if self.cache.requested.contains(dir) {
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

    /// Scan the roots in precedence order, first name wins.
    fn collect(
        &mut self,
        roots: &[(String, &'static str, bool)],
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
        // Deterministic root order: an earlier root keeps a name it claimed.
        for (path, _source, skip_system) in roots {
            let dirs = self.dir_of(path)?;
            for entry in dirs {
                if *skip_system && entry.name == ".system" {
                    continue;
                }
                // A directory skill is `<name>/SKILL.md`; a flat `.md` file is
                // a skill itself.
                let file = if entry.kind == "dir" {
                    format!("{path}/{}/SKILL.md", entry.name)
                } else if entry.name.ends_with(".md") {
                    format!("{path}/{}", entry.name)
                } else {
                    continue;
                };
                let Some(text) = self.read_text(&file)? else {
                    continue;
                };
                let Some(skill) = parse_skill(&file, &text) else {
                    continue;
                };
                by_name.entry(skill.name.clone()).or_insert(skill);
            }
        }
        let skills: Vec<serde_json::Value> = by_name
            .into_values()
            // User-invocable only: this catalog feeds the human composer.
            .filter(|s| s.user_invocable)
            .map(|s| {
                let mut v = serde_json::json!({
                    "name": s.name,
                    "description": s.description,
                    "modelInvocable": s.model_invocable,
                    "path": s.path,
                });
                if let Some(w) = s.when_to_use {
                    v["whenToUse"] = serde_json::Value::String(w);
                }
                v
            })
            .collect();
        Ok(rpc::ok(serde_json::json!({ "skills": skills })))
    }

    /// A file's text, or `None` when it is absent. One effect per file.
    /// A file's text, or `None` when it is absent or unreadable.
    ///
    /// Every failure is `None`: a skill file that cannot be read is a skill
    /// that does not exist, and upstream warns-and-skips rather than failing
    /// the whole catalog.
    fn read_text(&mut self, path: &str) -> Result<Option<String>, Vec<MachineOut>> {
        if let Some(bytes) = self.cache.files.get(path) {
            return Ok(Some(String::from_utf8_lossy(bytes).to_string()));
        }
        if self.cache.failed.contains_key(path) || self.cache.requested.contains(path) {
            return Ok(None);
        }
        self.cache.requested.insert(path.to_string());
        Err(self
            .cache
            .request_read(path, &mut self.pending, &mut self.effects))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_names_follow_the_kebab_grammar() {
        for good in ["goal", "my-skill", "a1-b2", "abc"] {
            assert!(is_skill_name(good), "{good}");
        }
        for bad in [
            "My-Skill", "my_skill", "-lead", "trail-", "a--b", "", "Ünicode",
        ] {
            assert!(!is_skill_name(bad), "{bad}");
        }
    }

    fn fm(block: &str) -> Option<Skill> {
        parse_skill("/x/SKILL.md", &format!("---\n{block}\n---\nBody here.\n"))
    }

    #[test]
    fn frontmatter_yields_a_skill() {
        let s = fm("name: my-skill\ndescription: does a thing").unwrap();
        assert_eq!(s.name, "my-skill");
        assert_eq!(s.description, "does a thing");
        // Both surfaces are enabled unless the negations say otherwise.
        assert!(s.model_invocable);
        assert!(s.user_invocable);
        assert!(s.when_to_use.is_none());
    }

    /// The frontmatter spells the negations, so the mapping inverts twice.
    #[test]
    fn invocation_flags_invert_from_their_negations() {
        let s = fm("name: s\ndescription: d\nuser-invocable: false").unwrap();
        assert!(!s.user_invocable);
        assert!(s.model_invocable);

        let s = fm("name: s\ndescription: d\ndisable-model-invocation: true").unwrap();
        assert!(!s.model_invocable);
        assert!(s.user_invocable);

        // The boolean accepts upstream's spellings.
        for truthy in ["true", "yes", "on", "1"] {
            let s = fm(&format!(
                "name: s\ndescription: d\nuser-invocable: {truthy}"
            ))
            .unwrap();
            assert!(s.user_invocable, "{truthy}");
        }
        for falsy in ["false", "no", "off", "0"] {
            let s = fm(&format!("name: s\ndescription: d\nuser-invocable: {falsy}")).unwrap();
            assert!(!s.user_invocable, "{falsy}");
        }
    }

    #[test]
    fn when_to_use_is_carried_when_present() {
        let s = fm("name: s\ndescription: d\nwhenToUse: when you need it").unwrap();
        assert_eq!(s.when_to_use.as_deref(), Some("when you need it"));
    }

    /// A bad skill file is skipped, never fatal: one broken file must not take
    /// the catalog down.
    #[test]
    fn malformed_or_incomplete_files_are_skipped() {
        // No frontmatter at all.
        assert!(parse_skill("/x", "just text").is_none());
        // Unclosed fence.
        assert!(parse_skill("/x", "---\nname: a\n").is_none());
        // Missing each required field.
        assert!(fm("name: only-name").is_none());
        assert!(fm("description: only-desc").is_none());
        // A name outside the grammar.
        assert!(fm("name: Not_Kebab\ndescription: d").is_none());
        // Empty values count as missing.
        assert!(fm("name: \"\"\ndescription: d").is_none());
        // Malformed YAML.
        assert!(parse_skill("/x", "---\nname: [unclosed\n---\n").is_none());
        // A legacy invocation key is refused rather than guessed at.
        assert!(fm("name: s\ndescription: d\nmodelInvocable: false").is_none());
    }

    #[test]
    fn unknown_method_is_a_typed_bad_request() {
        let mut m = SkillsMachine::new(
            std::path::PathBuf::from("/sessions"),
            "/home".into(),
            "/home/.agents".into(),
        );
        let outs = m.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("skills")),
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
