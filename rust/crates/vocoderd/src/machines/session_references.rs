//! The `sessionReferenceResolver` namespace: `@`-mention completion for *other
//! sessions*, as opposed to `fileReferences`' paths.
//!
//! Upstream (`dsh/packages/context/session-reference/src/index.ts`) is a
//! `TypertRemoteService` that injects `sessionQuery`. This mirrors the
//! `candidates` remote, which is the only endpoint of the namespace that is a
//! read: the rest of the service prepares cross-session context for a model
//! step, which is M4 work.
//!
//! **Self is excluded, and the cwd drives ranking.** A session referencing
//! itself is refused upstream outright; here it is simply never a candidate.
//! The requesting session's `cwd` ranks the others: same working directory
//! first, then sessions with no recorded cwd, then everything else. Within a
//! rank the listing order is preserved, so the popup does not reshuffle between
//! keystrokes.
//!
//! **Every candidate carries its canonical mention.** The client inserts
//! `mention` into the prompt draft verbatim, so the encoding is part of the
//! wire contract rather than a rendering detail: `@[label](dsh-session:<payload>)`
//! where the payload is the session id's JSON encoding in unpadded base64url,
//! and the label escapes `\` and `]`. A host that re-encoded it differently
//! would mint mentions the parser cannot read back.
//!
//! **A label falls back to the session id.** Upstream labels from a projection
//! snapshot and falls back when no projection holds a title. vocoderd folds
//! `session/title` rows out of the log, which is the same value by a different
//! route, and falls back identically — a session that was never renamed is
//! labeled by its id.

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::machines::readcache::{FsCache, Pending};
use crate::machines::session::SessionStore;
use crate::rpc;

/// Upstream's `DEFAULT_CANDIDATE_LIMIT`.
const DEFAULT_CANDIDATE_LIMIT: usize = 50;

/// One candidate, before projection to the wire shape.
struct Candidate {
    session_id: String,
    label: String,
    cwd: Option<String>,
    created_at: f64,
}

pub struct SessionReferencesMachine {
    sessions: SessionStore,
    cache: FsCache,
    pending: Option<Pending>,
    effects: u64,
}

impl SessionReferencesMachine {
    pub fn new(sessions_root: std::path::PathBuf) -> Self {
        Self {
            sessions: SessionStore::new(sessions_root),
            cache: FsCache::default(),
            pending: None,
            effects: 0,
        }
    }
}

impl PluginMachine for SessionReferencesMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::EffectResult { result, .. } = ev {
            if self.pending.is_none() {
                return vec![];
            }
            if self.cache.absorb(result) {
                return rpc::err(
                    "gateway/internal",
                    "sessionReferenceResolver: effect failed",
                );
            }
            let method = self.pending.as_ref().unwrap().method.clone();
            let req = self.pending.as_ref().unwrap().req.clone();
            return self.dispatch(&method, &req);
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("sessionReferenceResolver") {
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

impl SessionReferencesMachine {
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
            "candidates" => self.candidates(args),
            other => Ok(rpc::err(
                "gateway/bad-request",
                format!("unsupported sessionReferenceResolver method: {other}"),
            )),
        }
    }

    fn candidates(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        // `agentId` is a lookup parameter delivered beside the JSON body, so it
        // sits on `args` rather than under `request`.
        let target = rpc::arg_str(args, "agentId")
            .map(str::to_string)
            .or_else(|| {
                rpc::arg_str(
                    args.get("request").unwrap_or(&serde_json::Value::Null),
                    "sessionId",
                )
                .map(str::to_string)
            });
        let Some(target) = target else {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                "invalid payload for sessionReferenceResolver.candidates",
                serde_json::json!({}),
            ));
        };
        let query = rpc::arg_str(args, "query")
            .or_else(|| {
                rpc::arg_str(
                    args.get("request").unwrap_or(&serde_json::Value::Null),
                    "query",
                )
            })
            .unwrap_or_default()
            .to_lowercase();

        let listed = self.list_sessions()?;
        let target_cwd = listed
            .iter()
            .find(|(s, _)| s.id == target)
            .and_then(|(s, _)| s.cwd());

        // Rank first, then filter: the index that breaks ties is the listing
        // position, so filtering must not renumber it.
        let mut ranked: Vec<(u8, usize, Candidate)> = Vec::new();
        for (index, (session, title)) in listed.iter().enumerate() {
            if session.id == target {
                continue;
            }
            let cwd = session.cwd();
            let label = title.clone().unwrap_or_else(|| session.id.clone());
            if !query.is_empty() {
                let matches = session.id.to_lowercase().contains(&query)
                    || cwd
                        .as_ref()
                        .is_some_and(|c| c.to_lowercase().contains(&query))
                    || label.to_lowercase().contains(&query);
                if !matches {
                    continue;
                }
            }
            ranked.push((
                candidate_rank(cwd.as_deref(), target_cwd.as_deref()),
                index,
                Candidate {
                    session_id: session.id.clone(),
                    label,
                    cwd,
                    created_at: session.created_at(),
                },
            ));
        }
        ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        let items: Vec<serde_json::Value> = ranked
            .into_iter()
            .take(DEFAULT_CANDIDATE_LIMIT)
            .map(|(_, _, c)| {
                let mention = format_session_reference_mention(&c.label, &c.session_id);
                let mut v = serde_json::json!({
                    "mention": mention,
                    "sessionId": c.session_id,
                    "label": c.label,
                    "sameWorkspace": c.cwd.is_some() && c.cwd == target_cwd,
                    "createdAt": c.created_at,
                });
                if let Some(cwd) = c.cwd {
                    v["cwd"] = cwd.into();
                }
                v
            })
            .collect();
        Ok(rpc::ok(serde_json::Value::Array(items)))
    }

    /// Every session the log knows, with its folded title, in listing order.
    fn list_sessions(
        &mut self,
    ) -> Result<Vec<(crate::machines::session::StoredSession, Option<String>)>, Vec<MachineOut>>
    {
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
        let out = SessionStore::stateless_scan(&tree, &self.cache.files)
            .into_iter()
            .map(|session| {
                let dir = session.dir.to_string_lossy().to_string();
                let title = self
                    .cache
                    .session_rows(&dir, &tree)
                    .and_then(|rows| folded_title(&rows));
                (session, title)
            })
            .collect();
        Ok(out)
    }
}

/// The latest `session/title` value in a log, or `None` when never renamed.
///
/// A rename appends, so the last such row wins — the same fold a projection
/// snapshot would hold. A session renamed twice is titled by the second name.
pub fn folded_title(rows: &[serde_json::Value]) -> Option<String> {
    rows.iter()
        .filter(|row| row.get("type").and_then(|v| v.as_str()) == Some("session/title"))
        .filter_map(|row| {
            row.get("data")
                .and_then(|d| rpc::arg_str(d, "title"))
                .map(str::to_string)
        })
        .next_back()
}

/// Upstream's `candidateRank`: same workspace, then no workspace, then other.
fn candidate_rank(candidate_cwd: Option<&str>, target_cwd: Option<&str>) -> u8 {
    match (candidate_cwd, target_cwd) {
        (Some(c), Some(t)) if c == t => 0,
        (None, _) => 1,
        _ => 2,
    }
}

/// The canonical prompt mention for one session.
///
/// `@[label](dsh-session:<payload>)`, where the payload is the session id's
/// JSON encoding — quotes included — in unpadded base64url, and the label
/// escapes the two characters that would end the mention early.
fn format_session_reference_mention(label: &str, session_id: &str) -> String {
    let payload = base64url(
        serde_json::Value::String(session_id.to_string())
            .to_string()
            .as_bytes(),
    );
    let label = label.replace('\\', "\\\\").replace(']', "\\]");
    format!("@[{label}](dsh-session:{payload})")
}

/// Unpadded base64url, as `Buffer.toString('base64url')` produces.
///
/// Written out rather than pulled in: this is the only call site, the alphabet
/// is fixed by the wire format, and a dependency would carry a decoder and an
/// error type nothing here uses.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        // The last chunk emits only the characters its bytes fill; the
        // remainder is dropped rather than padded, which is what makes this
        // base64*url* as Node spells it.
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoder against Node's own output for the same inputs, so a change
    /// that still "looks like base64" fails here rather than at a client that
    /// cannot parse the mention.
    #[test]
    fn base64url_matches_node() {
        // `Buffer.from(JSON.stringify(s)).toString('base64url')` for each.
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        // Bytes whose encodings land on the url-safe alphabet's last two
        // characters (`-` and `_`), where standard base64 would emit `+`/`/`.
        assert_eq!(base64url(&[0xfb, 0xff, 0xbf]), "-_-_");
        assert_eq!(base64url(&[0xff, 0xff, 0xff]), "____");
        assert_eq!(base64url(&[0x00, 0x00, 0x00]), "AAAA");
    }

    /// A session id is encoded as its JSON string, quotes and all — which is
    /// what makes the payload decode back to the id rather than to a bare word.
    #[test]
    fn mention_encodes_the_id_as_a_json_string() {
        let mention = format_session_reference_mention("My session", "session-abc");
        assert_eq!(mention, "@[My session](dsh-session:InNlc3Npb24tYWJjIg)");

        // The payload is base64url over `"session-abc"` — the JSON encoding,
        // so a decoder recovers the quoted string and unquotes to the id.
        let payload = mention
            .rsplit_once("dsh-session:")
            .unwrap()
            .1
            .trim_end_matches(')');
        assert_eq!(payload, base64url(br#""session-abc""#));
    }

    /// A label that would otherwise close the mention early is escaped.
    #[test]
    fn mention_escapes_the_label() {
        assert_eq!(
            format_session_reference_mention("a]b", "s"),
            "@[a\\]b](dsh-session:InMi)"
        );
        assert_eq!(
            format_session_reference_mention("a\\b", "s"),
            "@[a\\\\b](dsh-session:InMi)"
        );
    }

    #[test]
    fn ranking_prefers_the_same_workspace() {
        assert_eq!(candidate_rank(Some("/w"), Some("/w")), 0);
        assert_eq!(candidate_rank(None, Some("/w")), 1);
        assert_eq!(candidate_rank(Some("/other"), Some("/w")), 2);
        // A target with no cwd still ranks an absent-cwd candidate first.
        assert_eq!(candidate_rank(None, None), 1);
        assert_eq!(candidate_rank(Some("/w"), None), 2);
    }

    #[test]
    fn title_folds_to_the_latest_rename() {
        let rows = vec![
            serde_json::json!({ "type": "session/title", "data": { "title": "first" } }),
            serde_json::json!({ "type": "user/message", "data": {} }),
            serde_json::json!({ "type": "session/title", "data": { "title": "second" } }),
        ];
        assert_eq!(folded_title(&rows).as_deref(), Some("second"));
        assert_eq!(folded_title(&[]), None);
        // A log that was never renamed has no title to fall back to.
        let untouched = vec![serde_json::json!({ "type": "session", "data": {} })];
        assert_eq!(folded_title(&untouched), None);
    }

    #[test]
    fn unknown_method_is_a_typed_bad_request() {
        let mut m = SessionReferencesMachine::new(std::path::PathBuf::from("/s"));
        let outs = m.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("sessionReferenceResolver")),
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
