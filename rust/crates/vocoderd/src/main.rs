//! vocoderd — the Rust web host. Thin async driver over the Sans-I/O core.

#[cfg(test)]
mod composition;
mod driver;
mod machines;
mod open_in_app;
mod registry;
#[cfg(test)]
mod routes;
mod rpc;
mod validate;
mod web_boot;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Router as AxumRouter,
    body::Bytes,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{any, get, post},
};
use clap::Parser;
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tracing::info;
use vocoder_cordis::{
    EventName, MachineId, MachineIn, PluginMachine, RealizeRequest, RouteIn, RouteOut, Router,
};
use vocoder_typert::{
    ClientRequest, RpcError, RpcResult, ServerResponse, decode_rpc_client_request,
    encode_rpc_server_response, mux::MuxSessionMachine,
};

#[derive(Parser)]
#[command(name = "vocoderd", about = "Pure-Rust dsh backend (spec-driven)")]
enum Cmd {
    /// Serve the web host: HTTP + WS gateway.
    Serve(ServeArgs),
}

#[derive(clap::Args)]
struct ServeArgs {
    #[arg(long, default_value = ".scratch/vocoderd-home")]
    home: std::path::PathBuf,
    #[arg(long, default_value = "spec")]
    spec: std::path::PathBuf,
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
    #[arg(long, default_value_t = 3080)]
    port: u16,
    /// Serve the built dsh web GUI from this directory (expects dist/index.html).
    #[arg(long)]
    web_dist: Option<std::path::PathBuf>,
    /// The dsh checkout the client-module graph is composed from. Only read when
    /// `--web-dist` is set: the boot graph needs the package tree's manifests
    /// and built client bundles, which the dist directory does not contain.
    #[arg(long, default_value = "dsh")]
    dsh_root: std::path::PathBuf,
    /// Workspace-of-first-run display name in the injected boot payload.
    #[arg(long, default_value = "vocoder")]
    host_name: String,
}

/// Shared state across HTTP/WS handlers.
struct AppState {
    /// The machine tree; synchronized because handlers touch it from
    /// connection tasks.
    router: Mutex<Router>,
    /// Namespace ownership lookup for RPC routing (goals, session, ...).
    namespace_registry: Mutex<vocoder_typert::dispatch::NamespaceRegistry>,
    /// Live mux connections: namespace machines address stream frames here
    /// (`stream.*` Realize outputs) and the owning connection task writes
    /// them to the socket.
    connections: Mutex<std::collections::HashMap<String, ConnectionHub>>,
    /// streamId → (conn_id, namespace-owning machine) for driver routing.
    streams: Mutex<std::collections::HashMap<String, StreamRoute>>,
    /// Stable host facts for the $events ready frame.
    home: String,
    /// Serializes whole effect loops. A machine holds one suspended operation
    /// at a time, so two pumps interleaving between an effect request and its
    /// answer would cross their suspensions and deliver an answer to the wrong
    /// operation.
    ///
    /// Deliberately not the router lock: `pump` calls `route_stream_outs`,
    /// which takes the connections lock, and connection tasks take those in
    /// the opposite order — holding the router lock inverts that order and
    /// deadlocks.
    dispatch: Mutex<()>,
}

struct StreamRoute {
    conn: String,
    owner: MachineId,
    namespace: String,
}

/// Per-connection downlink: machines' stream frames arrive tagged with
/// their streamId so the connection task can write them and notice
/// end/error frames for local bookkeeping.
struct ConnectionHub {
    tx: tokio::sync::mpsc::Sender<(String, String)>,
}

impl AppState {
    /// Deliver one input to one machine and run the effect loop to quiescence:
    /// execute each requested effect, feed the answer back, until the machine
    /// produces a `Reply`/`Stream` (or the cap trips).
    ///
    /// Returns the terminal outputs. Machines are pure, so this is the whole
    /// boundary between protocol logic and the world.
    ///
    /// A streaming effect ([`vocoder_cordis::RealizeRequest::FetchStream`])
    /// delivers its bytes back through this loop *while it is still running*:
    /// each read becomes a [`MachineIn::EffectChunk`] fed to the machine before
    /// the final [`MachineIn::EffectResult`]. That is what lets the machine
    /// forward a model's first token as it arrives rather than when the answer
    /// completes.
    ///
    /// The chunk deliveries are made from *inside* the effect, and their stream
    /// frames are routed immediately. Both are load-bearing: a design that
    /// queued the chunks and replayed them after the fetch returned would
    /// produce byte-identical frames at byte-identical offsets and deliver
    /// every one of them at once at the end — the protocol would look right and
    /// the latency would be untouched. Delivering during the read is the whole
    /// feature, so it is done where the bytes are, not where they are
    /// convenient.
    fn pump(&self, to: &MachineId, ev: MachineIn) -> Vec<RouteOut> {
        let mut terminal = Vec::new();
        // Inputs still to deliver, and effects still to run. Two queues rather
        // than one because they drain in a fixed order: every queued input is
        // delivered before the next effect starts. That order is the machine
        // contract — a machine hangs one operation on one outstanding effect —
        // and merging the queues would let a second effect start while the
        // first one's chunks were still being delivered.
        let mut pending: std::collections::VecDeque<MachineIn> = std::collections::VecDeque::new();
        let mut todo: std::collections::VecDeque<(vocoder_cordis::EffectId, RealizeRequest)> =
            std::collections::VecDeque::new();
        pending.push_back(ev);
        // One whole effect loop at a time; see [`AppState::dispatch`].
        let _serialized = self.dispatch.lock();
        // Bounded by *effects*, not by iterations: chunk deliveries are not
        // effects and must not consume the budget that exists to catch a
        // machine looping on I/O. A long model answer is many chunks and one
        // effect, which is exactly the shape this must not mistake for a loop.
        let mut effect_count = 0usize;
        loop {
            if let Some(input) = pending.pop_front() {
                let outs = self.deliver(to, input);
                sort_outs(outs, &mut terminal, &mut todo);
                continue;
            }
            let Some((id, request)) = todo.pop_front() else {
                break;
            };
            if effect_count >= crate::driver::MAX_EFFECTS_PER_INPUT {
                break;
            }
            effect_count += 1;
            // Outputs the chunk deliveries produced, collected because the sink
            // runs inside `realize_with` and cannot hand them back.
            let mut during: Vec<RouteOut> = Vec::new();
            let result = crate::driver::realize_with(request, &mut |bytes| {
                during.extend(self.deliver(to, MachineIn::EffectChunk { id, bytes }));
            })
            .unwrap_or(vocoder_cordis::EffectResult::Done);
            sort_outs(during, &mut terminal, &mut todo);
            pending.push_back(MachineIn::EffectResult { id, result });
        }
        terminal
    }

    /// Feed one input to one machine, routing its stream frames as they are
    /// produced. Stream frames are not answers, so they never reach the caller.
    fn deliver(&self, to: &MachineId, ev: MachineIn) -> Vec<RouteOut> {
        let outs = self
            .router
            .lock()
            .handle(RouteIn::Deliver { to: to.clone(), ev });
        let (streams, rest): (Vec<_>, Vec<_>) = outs
            .into_iter()
            .partition(|o| matches!(o, RouteOut::Stream { .. }));
        self.route_stream_outs(streams);
        rest
    }
}

/// Split one step's outputs into effects still to run, and everything else.
///
/// Stream frames are already routed by [`AppState::deliver`], so they cannot
/// appear here.
fn sort_outs(
    outs: Vec<RouteOut>,
    terminal: &mut Vec<RouteOut>,
    todo: &mut std::collections::VecDeque<(vocoder_cordis::EffectId, RealizeRequest)>,
) {
    for out in outs {
        match out {
            RouteOut::Realize { id, request, .. } => todo.push_back((id, request)),
            other => terminal.push(other),
        }
    }
}

impl AppState {
    /// Deliver one encoded stream frame to the owning connection. Returns
    /// true when the frame reached a live connection task.
    fn deliver_frame(&self, stream_id: &str, text: &str) -> bool {
        let conns = self.connections.lock();
        let streams = self.streams.lock();
        if let Some(route) = streams.get(stream_id)
            && let Some(hub) = conns.get(&route.conn)
        {
            let _ = hub.tx.try_send((stream_id.to_string(), text.to_string()));
            return true;
        }
        false
    }

    /// Route all stream frames from one router step to their owning connection
    /// tasks. Removes the route on end/error.
    fn route_stream_outs(&self, outs: Vec<RouteOut>) {
        for out in outs {
            let RouteOut::Stream { frame, .. } = out else {
                continue;
            };
            let msg = match frame {
                vocoder_cordis::StreamFrame::Item { stream_id, value } => {
                    vocoder_typert::StreamServerMessage::Item {
                        stream_id,
                        value: Some(value),
                    }
                }
                vocoder_cordis::StreamFrame::End { stream_id } => {
                    vocoder_typert::StreamServerMessage::End { stream_id }
                }
                vocoder_cordis::StreamFrame::Error {
                    stream_id,
                    name,
                    code,
                    message,
                    details,
                } => vocoder_typert::StreamServerMessage::Error {
                    stream_id,
                    error: vocoder_typert::StreamFailure {
                        name,
                        code: Some(code),
                        message,
                        details: Some(details).filter(|d| !d.is_null()),
                    },
                },
            };
            let stream_id = match &msg {
                vocoder_typert::StreamServerMessage::Item { stream_id, .. }
                | vocoder_typert::StreamServerMessage::End { stream_id }
                | vocoder_typert::StreamServerMessage::Error { stream_id, .. } => stream_id.clone(),
            };
            let text = String::from_utf8(vocoder_typert::encode_stream_server(&msg)).unwrap();
            let delivered = self.deliver_frame(&stream_id, &text);
            if delivered && !matches!(msg, vocoder_typert::StreamServerMessage::Item { .. }) {
                self.streams.lock().remove(&stream_id);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,vocoderd=debug".into()),
        )
        .init();

    let Cmd::Serve(args) = Cmd::parse();
    info!(home = ?args.home, spec = ?args.spec, "vocoderd starting");

    let sessions_root = args.home.join("sessions");
    let workspace_registry = crate::registry::WorkspaceRegistryStore::open(&args.home);
    // Staged upload receipts are runtime state shared between the `fileUploads`
    // machine that mints them and the `session` machine that resolves them at
    // prompt admission. Upstream keeps the equivalent map on the `fileUploads`
    // service and has the session controller reach it through
    // `ctx.fileUploads.resolve`; the router here cannot make one machine call
    // another, so both mount with a handle to the same store.
    let staged_uploads = std::sync::Arc::new(crate::registry::StagedUploadsStore::new());

    let mut initial_router = Router::new();
    // Business machines mounted at boot (M3+: from profile composition).
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("goals"),
        machine: Box::new(crate::machines::goals::GoalsMachine::default()),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("session"),
        machine: Box::new(
            crate::machines::session::SessionMachine::new(
                sessions_root.clone(),
                workspace_registry.clone(),
            )
            // Prompt admission resolves a staged upload receipt through the same
            // store the `fileUploads` machine stages into.
            .with_staged_uploads(staged_uploads.clone()),
        ),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("workspace"),
        machine: Box::new(crate::machines::workspace::WorkspaceMachine::new(
            workspace_registry.clone(),
            sessions_root.clone(),
        )),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("settings"),
        // The driver reads the document at boot and hands the machine its
        // contents: reading is I/O, so it belongs here, not in the machine.
        machine: Box::new(crate::machines::settings::SettingsMachine::new(
            &args.home,
            read_settings_document(&args.home).as_deref(),
        )),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("workspaceFiles"),
        machine: Box::new(
            crate::machines::workspace_files::WorkspaceFilesMachine::new(sessions_root.clone()),
        ),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("directoryPicker"),
        // The home directory is a process fact the machine may not look up
        // (that would be I/O), so the driver resolves it at mount.
        machine: Box::new(
            crate::machines::directory_picker::DirectoryPickerMachine::new(home_dir()),
        ),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("credentials"),
        // Every layer the machine resolves against is a boot read: the
        // document's text, the launching environment, and the two `.env`
        // fallbacks. Reading them here is what keeps the machine Sans-I/O.
        machine: Box::new({
            let (inherited, project_env, user_env) = credentials_boot(&args.home);
            crate::machines::credentials::CredentialsMachine::new(
                args.home.join(".credentials.yaml").display().to_string(),
                std::fs::read_to_string(args.home.join(".credentials.yaml")).ok(),
                inherited,
                project_env,
                user_env,
            )
        }),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("skills"),
        // Both roots are process facts (`$DSH_HOME` and `~/.agents`), so the
        // driver resolves them and the machine never reads the environment.
        machine: Box::new(crate::machines::skills::SkillsMachine::new(
            sessions_root.clone(),
            args.home.display().to_string(),
            agents_home(),
        )),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("fileReferences"),
        machine: Box::new(
            crate::machines::file_references::FileReferencesMachine::new(sessions_root.clone()),
        ),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("fileUploads"),
        machine: Box::new(crate::machines::file_uploads::FileUploadsMachine::new(
            args.home.clone(),
            sessions_root.clone(),
            staged_uploads.clone(),
        )),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("agentPresets"),
        // The writable root (`$DSH_HOME/.agent-presets`) is derived from the
        // home the driver was given, so the machine never reads the process.
        machine: Box::new(crate::machines::agent_presets::AgentPresetsMachine::new(
            args.home.display().to_string(),
        )),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("commands"),
        machine: Box::new(crate::machines::commands::CommandsMachine::default()),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("messageFeedback"),
        machine: Box::new(
            crate::machines::message_feedback::MessageFeedbackMachine::new(sessions_root.clone()),
        ),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("sessionFeedback"),
        machine: Box::new(
            crate::machines::session_feedback::SessionFeedbackMachine::new(sessions_root.clone()),
        ),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("sessionReferenceResolver"),
        machine: Box::new(
            crate::machines::session_references::SessionReferencesMachine::new(
                sessions_root.clone(),
            ),
        ),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("$events"),
        machine: Box::new(crate::machines::events::EventsMachine::default()),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("approval"),
        machine: Box::new(crate::machines::approval::ApprovalMachine::new()),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("agentTeams"),
        machine: Box::new(crate::machines::agent_teams::AgentTeamsMachine::default()),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("llm"),
        machine: Box::new(crate::machines::llm::LlmMachine),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("subagents"),
        machine: Box::new(crate::machines::subagents::SubagentsMachine::new(
            sessions_root.clone(),
        )),
    });
    // The agent core's driver half. It is driven by `vocoder/agent/call`, which
    // no wire descriptor carries: the client-facing entry point for a turn is
    // `session/prompt`. This machine is what consumes the inbox row that call
    // writes and turns it into `turn/*` rows, so it is mounted here and wired to
    // the session namespace through the shared log rather than through the
    // namespace registry.
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("agent"),
        machine: Box::new(
            crate::machines::agent::AgentMachine::new(
                sessions_root.clone(),
                crate::machines::agent::routes_from_env(),
            )
            // The sandbox chain is probed once, here, because probing spawns
            // processes and a machine may not. `std::env::consts::OS` yields the
            // same names `Runner::chain` matches on (`linux`/`macos`/`windows`),
            // so an unlisted platform gets an empty chain and every confined
            // command fails closed rather than running unconfined.
            .with_sandbox(crate::driver::probe_sandbox(std::env::consts::OS)),
        ),
    });
    let mut registry = vocoder_typert::dispatch::NamespaceRegistry::new();
    registry_owner_register(&mut registry, "goals", "goals");
    registry_owner_register(&mut registry, "session", "session");
    registry_owner_register(&mut registry, "workspace", "workspace");
    registry_owner_register(&mut registry, "workspaceFiles", "workspaceFiles");
    registry_owner_register(&mut registry, "settings", "settings");
    registry_owner_register(&mut registry, "directoryPicker", "directoryPicker");
    registry_owner_register(&mut registry, "credentials", "credentials");
    registry_owner_register(&mut registry, "skills", "skills");
    registry_owner_register(&mut registry, "fileReferences", "fileReferences");
    registry_owner_register(&mut registry, "fileUploads", "fileUploads");
    registry_owner_register(&mut registry, "commands", "commands");
    registry_owner_register(&mut registry, "agentPresets", "agentPresets");
    registry_owner_register(&mut registry, "messageFeedback", "messageFeedback");
    registry_owner_register(&mut registry, "sessionFeedback", "sessionFeedback");
    registry_owner_register(
        &mut registry,
        "sessionReferenceResolver",
        "sessionReferenceResolver",
    );
    registry_owner_register(&mut registry, "$events", "$events");
    registry_owner_register(&mut registry, "agentTeams", "agentTeams");
    registry_owner_register(&mut registry, "llm", "llm");
    registry_owner_register(&mut registry, "subagents", "subagents");

    // `dynamicCordisRunner` is mounted even though this host defines no dynamic
    // plugins: the web GUI calls `inventory` and `syncInspectManifest` at boot,
    // and a bare 404 there is a console error. The empty answers are the
    // control's own empty-registry answers, not a stub. See the module docs.
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("dynamicCordisRunner"),
        machine: Box::new(
            crate::machines::dynamic_cordis_runner::DynamicCordisRunnerMachine::new(),
        ),
    });
    registry_owner_register(&mut registry, "dynamicCordisRunner", "dynamicCordisRunner");

    // `pluginInventory` is mounted last and takes its answer from the two facts
    // that only exist now: the mounted machine tree and the namespace registry.
    // In this host a machine *is* a plugin, so this is the load result of the
    // composition rather than a second, parallel bookkeeping of it.
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("pluginInventory"),
        machine: Box::new(
            crate::machines::plugin_inventory::PluginInventoryMachine::new(
                initial_router
                    .machine_ids()
                    .into_iter()
                    .map(|id| id.0)
                    .collect(),
                registry
                    .namespaces()
                    .filter_map(|ns| {
                        registry
                            .owner_of(ns)
                            .map(|owner| (ns.to_string(), owner.0.clone()))
                    })
                    .collect(),
            ),
        ),
    });
    registry_owner_register(&mut registry, "pluginInventory", "pluginInventory");

    let state = Arc::new(AppState {
        router: Mutex::new(initial_router),
        namespace_registry: Mutex::new(registry),
        connections: Mutex::new(std::collections::HashMap::new()),
        streams: Mutex::new(std::collections::HashMap::new()),
        home: args.home.display().to_string(),
        dispatch: Mutex::new(()),
    });

    let app: AxumRouter<Arc<AppState>> = AxumRouter::new()
        .route("/api/remote.mux", any(ws_upgrade))
        .route("/api/{*endpoint}", post(api_rpc))
        .route("/healthz", get(|| async { StatusCode::OK }));

    let app = if let Some(dist) = args.web_dist.clone() {
        let dist2 = dist.clone();
        info!(dist = ?args.web_dist, "serving web GUI");
        // Compose the client-module boot graph. Fatal on failure: a dist served
        // without the graph is a shell that throws
        // `window.__ModuleLoader__ bootstrap facade is missing` before mount, so
        // booting anyway would only move the failure into the browser console.
        let backend = web_boot::resolve_picker_backend(&web_boot::PickerFacts::detect(&args.bind));
        let boot = std::sync::Arc::new(
            web_boot::DshTree::new(&args.dsh_root)
                .compose(backend)
                .context("composing the client-module boot graph")?,
        );
        info!(
            entries = boot.graph.entries.len(),
            batches = boot.graph.batches.len(),
            backend = ?backend,
            "client-module boot graph composed"
        );
        let boot2 = boot.clone();
        let boot3 = boot.clone();
        let boot_events = boot.clone();
        app.route("/", get(move || serve_index(dist.clone(), boot2.clone())))
            // The combo route must precede the dist fallback, or a
            // `/plugins/...` request would be served as a static miss. The
            // specifier starts with `??`, so the path is exactly `/plugins/`
            // and the whole thing rides in the query string: the handler reads
            // the raw request target (`pathname + search`), which is exactly the
            // key upstream registers its responses under.
            //
            // Two routes, because axum's `{*rest}` requires at least one
            // character: every real combo request is the bare `/plugins/`, so a
            // catch-all alone never matches one and the static fallback would
            // 404 it. Keep both so a future non-combo `/plugins/<name>` path
            // still reaches the handler's own 404 rather than the fallback's.
            .route(
                "/plugins/",
                get(move |uri: axum::http::Uri| serve_combo(uri, boot3.clone())),
            )
            // The client-module dev channel. An exact path, so it wins over the
            // catch-all below (axum matches literal segments ahead of a
            // wildcard) and does not fall into `serve_combo`'s specifier parse.
            .route(
                "/plugins/events",
                get(move || serve_plugin_events(boot_events.clone())),
            )
            .route(
                "/plugins/{*resource}",
                get(move |uri: axum::http::Uri| serve_combo(uri, boot.clone())),
            )
            // The client plugin `ui-open-in-app` fetches this at boot. An exact
            // path registered ahead of the dist fallback, so a missing dist
            // file cannot answer it; the GUI surface only exists with
            // `--web-dist`, hence registration here and not in the base router.
            .route("/open-in-app/apps", get(open_in_app::handler))
            .nest_service(
                "/assets",
                tower_http::services::ServeDir::new(dist2.join("assets")),
            )
            .fallback_service(tower_http::services::ServeDir::new(dist2))
    } else {
        app.route("/", get(index))
    };

    let app = app.with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.bind, args.port).parse()?;
    info!(%addr, "listening");
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> impl IntoResponse {
    axum::response::Html(
        "<!doctype html>
<html><head><title>vocoderd</title></head><body><h1>vocoderd is up</h1></body></html>"
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// /api/remote.mux — WS glue to a fresh mux-session machine per connection
// ---------------------------------------------------------------------------

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_conn(socket, state))
}

async fn ws_conn(mut socket: WebSocket, state: Arc<AppState>) {
    use vocoder_typert::{StreamClientMessage, StreamServerMessage};

    let conn_id = format!("conn-{}", uuid());
    let session_id = MachineId::new(format!("mux-{conn_id}"));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, String)>(256);
    state
        .connections
        .lock()
        .insert(conn_id.clone(), ConnectionHub { tx });
    {
        let mut router = state.router.lock();
        router.handle(RouteIn::Mount {
            id: session_id.clone(),
            machine: Box::new(MuxSessionMachine::new()),
        });
        router.handle(RouteIn::Deliver {
            to: session_id.clone(),
            ev: MachineIn::ServicesReady { keys: vec![] },
        });
    }

    // Stream ids this connection still believes are open (for end/error
    // bookkeeping and teardown).
    let mut mine: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        tokio::select! {
            maybe = socket.recv() => {
                let msg = match maybe {
                    Some(Ok(m)) => m,
                    _ => break,
                };
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                match serde_json::from_str::<StreamClientMessage>(&text) {
                    Ok(StreamClientMessage::Open { stream_id, endpoint, payload }) => {
                        let namespace =
                            endpoint.split('/').next().unwrap_or_default().to_string();
                        let owner = state
                            .namespace_registry
                            .lock()
                            .owner_of(&namespace)
                            .cloned();
                        match owner {
                            Some(owner) => {
                                state.streams.lock().insert(
                                    stream_id.clone(),
                                    StreamRoute {
                                        conn: conn_id.clone(),
                                        owner: owner.clone(),
                                        namespace: namespace.clone(),
                                    },
                                );
                                mine.insert(stream_id.clone());
                                // `args` as sent, so lookup parameters
                                // (`workspaceFileScopeId`) survive alongside
                                // `request` — they are siblings on the wire,
                                // and a machine that needs one cannot see it
                                // otherwise. Machines that want just the
                                // request body read `request` from it.
                                let request = payload
                                    .get("args")
                                    .cloned()
                                    .unwrap_or(payload.clone());
                                // Through the pump, not a bare Deliver: opening a
                                // stream may need filesystem effects (a follow
                                // snapshot reads the session log), and the pump is
                                // what realizes them.
                                let outs = state.pump(
                                    &owner,
                                    MachineIn::Event {
                                        name: EventName::new(
                                            crate::rpc::stream_open_event(&namespace),
                                        ),
                                        payload: serde_json::json!({
                                            "streamId": stream_id,
                                            "method": endpoint
                                                .split('/')
                                                .nth(1)
                                                .unwrap_or_default(),
                                            "request": request,
                                            "hostHome": state.home,
                                        }),
                                    },
                                );
                                state.route_stream_outs(outs);
                            }
                            None => {
                                let msg = StreamServerMessage::Error {
                                    stream_id,
                                    error: vocoder_typert::StreamFailure {
                                        name: "RemoteError".into(),
                                        code: Some(
                                            "gateway/invocation-unavailable".into(),
                                        ),
                                        message: format!(
                                            "typert gateway: {endpoint}: no active Remote method exports this endpoint"
                                        ),
                                        details: Some(serde_json::json!({
                                            "endpoint": endpoint,
                                        })),
                                    },
                                };
                                let text = String::from_utf8(
                                    vocoder_typert::encode_stream_server(&msg),
                                )
                                .unwrap();
                                let _ = socket.send(Message::Text(text.into())).await;
                            }
                        }
                    }
                    Ok(StreamClientMessage::Cancel { stream_id }) => {
                        mine.remove(&stream_id);
                        let route = state.streams.lock().remove(&stream_id);
                        if let Some(route) = route {
                            let outs = state.router.lock().handle(RouteIn::Deliver {
                                to: route.owner,
                                ev: MachineIn::Event {
                                    name: EventName::new(crate::rpc::stream_close_event(
                                        &route.namespace,
                                    )),
                                    payload: serde_json::json!({ "streamId": stream_id.clone() }),
                                },
                            });
                            state.route_stream_outs(outs);
                        }
                        // **No `end` frame in reply.** The cancel *is* the
                        // stream's termination: upstream's pump only sends
                        // `end` when its source completed on its own, guarded
                        // by `if (!active.abort.signal.aborted)`
                        // (`packages/api/gateway/src/stream-server.ts:166`).
                        // Replying `end` after a cancel invents a second
                        // termination for a stream the client already closed.
                    }
                    Err(_) => break, // malformed frame: close connection
                }
            }
            Some((stream_id, text)) = rx.recv() => {
                let _ = socket.send(Message::Text(text.clone().into())).await;
                if let Ok(StreamServerMessage::End { .. } | StreamServerMessage::Error { .. }) =
                    serde_json::from_str::<StreamServerMessage>(&text)
                {
                    mine.remove(&stream_id);
                    state.streams.lock().remove(&stream_id);
                }
            }
        }
    }

    // Teardown: tell machines their streams are gone.
    for stream_id in mine {
        if let Some(route) = state.streams.lock().remove(&stream_id) {
            let _ = state.router.lock().handle(RouteIn::Deliver {
                to: route.owner,
                ev: MachineIn::Event {
                    name: EventName::new(crate::rpc::stream_close_event(&route.namespace)),
                    payload: serde_json::json!({ "streamId": stream_id }),
                },
            });
        }
    }
    state.connections.lock().remove(&conn_id);
    let mut router = state.router.lock();
    router.handle(RouteIn::Unmount { id: session_id });
}

// ---------------------------------------------------------------------------
// POST /api/{*endpoint} — unary RPC over HTTP
// ---------------------------------------------------------------------------

async fn api_rpc(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(_endpoint): axum::extract::Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let req: ClientRequest = match decode_rpc_client_request(&body) {
        Ok(r) => r,
        Err(e) => {
            return respond_err(
                String::new(),
                "gateway/bad-request",
                format!("bad envelope: {e}"),
            );
        }
    };

    // The payload-shape gate runs first, as the control's `remoteRequest`
    // does: an envelope whose payload is not exactly one plain-object `args`
    // field is `gateway/internal` with the control's verbatim message, for any
    // namespace — including one this host does not serve.
    if !crate::validate::payload_shape_ok(&req.payload) {
        return respond_err(
            req.rpc_id.clone(),
            "gateway/internal",
            crate::validate::PAYLOAD_SHAPE_MESSAGE.to_string(),
        );
    }

    // Resolve the owning namespace through the registry, then the machine.
    let namespace = req.method.split('/').next().unwrap_or_default().to_string();
    let method = req.method.split('/').nth(1).unwrap_or_default().to_string();
    // The registry lives in AppState directly for synchronous lookups.
    let owner_id = state
        .namespace_registry
        .lock()
        .owner_of(&namespace)
        .cloned();
    let Some(owner_id) = owner_id else {
        return respond_err(
            req.rpc_id.clone(),
            "gateway/internal",
            format!("no such namespace on this host: {namespace}"),
        );
    };

    // The dispatch boundary: check the args against the spec'd descriptor
    // *before* the machine sees them, which is where the control host does its
    // own checks and why it answers `arguments-invalid`/`input-invalid` rather
    // than whatever the machine would have said about a malformed payload. The
    // payload-shape gate above has already guaranteed `args` is an object.
    let args = req.payload.get("args").cloned().expect("shape gate passed");
    if let Some(rejection) = crate::validate::check(&namespace, &method, &args) {
        return crate::validate::respond(req.rpc_id.clone(), &rejection);
    }

    let outs = state.pump(
        &owner_id,
        MachineIn::Event {
            name: EventName::new(crate::rpc::call_event(&namespace)),
            payload: serde_json::json!({
                "method": method,
                "args": args,
            }),
        },
    );

    // `session/prompt` admits input and answers `{accepted: true}`; the turn it
    // admits then runs on its own. That split is upstream's own contract — the
    // prompt call is a durable acceptance, and the turn streams to whoever is
    // following the session — and it is also a necessity here: a turn makes
    // network calls that take seconds to minutes, and holding the HTTP response
    // open that long would make the client's prompt look hung.
    //
    // Spawned rather than awaited, so the acknowledgement is returned now.
    //
    // The request's fields are read from `args.request`, not from `args`: every
    // session endpoint takes its payload under the wire name `request` (the
    // spec records this, and `session/list` spells it `_request`), which is why
    // reading `args.sessionId` finds nothing and the turn is silently skipped.
    if namespace == "session"
        && method == "prompt"
        && outs.iter().any(|o| {
            matches!(
                o,
                RouteOut::Reply {
                    reply: vocoder_cordis::RpcReply::Ok { .. },
                    ..
                }
            )
        })
        && let Some(request) = args.get("request")
        && let (Some(session_id), Some(request_id)) = (
            request.get("sessionId").and_then(|v| v.as_str()),
            request.get("requestId").and_then(|v| v.as_str()),
        )
    {
        let state = state.clone();
        let session_id = session_id.to_string();
        let request_id = request_id.to_string();
        let content = request
            .get("content")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        tokio::task::spawn_blocking(move || {
            crate::run_agent_turn(&state, &session_id, &request_id, content);
        });
    }

    // `session/cancel` likewise, and **only when the handler accepted it**.
    //
    // The session machine owns the decision — whether the Session exists and is
    // not a subagent child — and the agent machine owns the cancellation. The
    // two are separate machines with separate state, so the acceptance above is
    // what gates the forward: sending a cancel the handler refused would abort a
    // turn for a session the caller was just told it may not touch, and the
    // subagent case would race the parent's own delivery.
    //
    // The agent machine latches the cancel and lets the in-flight model call
    // settle the turn, because forcing the frame closers now would leave the
    // reply with no open step to settle into.
    if namespace == "session"
        && method == "cancel"
        && outs.iter().any(|o| {
            matches!(
                o,
                RouteOut::Reply {
                    reply: vocoder_cordis::RpcReply::Ok { .. },
                    ..
                }
            )
        })
        && let Some(request) = args.get("request")
        && let Some(session_id) = request.get("sessionId").and_then(|v| v.as_str())
    {
        let state = state.clone();
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let _ = state.pump(
                &MachineId::new("agent"),
                MachineIn::Event {
                    name: EventName::new(crate::rpc::call_event("agent")),
                    payload: serde_json::json!({
                        "method": "cancel",
                        "args": { "sessionId": session_id },
                    }),
                },
            );
        });
    }

    respond(outs, req.rpc_id)
}

/// Drive one agent turn to completion, detached from the prompt's response.
///
/// A blocking task rather than an async one: the effect loop is synchronous by
/// design (see `driver`), and a provider call is a blocking `ureq` request, so
/// running it on a runtime worker would stall the reactor for the length of the
/// call instead of parking a dedicated thread.
pub(crate) fn run_agent_turn(
    state: &AppState,
    session_id: &str,
    request_id: &str,
    content: serde_json::Value,
) {
    let payload = serde_json::json!({
        "method": "run",
        "args": {
            "sessionId": session_id,
            "requestId": request_id,
            "content": content,
        },
    });
    let outs = state.pump(
        &MachineId::new("agent"),
        MachineIn::Event {
            name: EventName::new(crate::rpc::call_event("agent")),
            payload,
        },
    );
    // A detached turn has no caller to answer, so a failure is logged rather
    // than dropped. The session log still records it: the turn is closed as an
    // error row before this point.
    for out in outs {
        if let RouteOut::Reply {
            reply: vocoder_cordis::RpcReply::Err { code, message, .. },
            ..
        } = out
        {
            tracing::error!("agent turn {request_id} failed: {code}: {message}");
        }
    }
}

/// A typed gateway failure, as the HTTP response.
///
/// A free function rather than a closure because the dispatch boundary uses it
/// from more than one place now — the envelope check, the namespace lookup, and
/// the argument validator all answer through it.
fn respond_err(rpc_id: String, code: &str, message: String) -> (StatusCode, String) {
    let bytes = encode_rpc_server_response(&ServerResponse {
        rpc_id,
        result: RpcResult::Err {
            ok: vocoder_typert::OkTag(false),
            error: RpcError {
                code: code.into(),
                message,
                details: None,
            },
        },
    });
    (StatusCode::OK, String::from_utf8(bytes).unwrap())
}

/// Turn a machine's outputs into the HTTP response.
fn respond(outs: Vec<RouteOut>, rpc_id: String) -> (StatusCode, String) {
    for out in outs {
        let RouteOut::Reply { reply, .. } = out else {
            continue;
        };
        let result = match reply {
            vocoder_cordis::RpcReply::Ok { value } => vocoder_typert::RpcResult::Ok {
                ok: vocoder_typert::OkTag(true),
                value,
            },
            vocoder_cordis::RpcReply::Err {
                code,
                message,
                details,
            } => {
                let mut error = serde_json::json!({ "code": code, "message": message });
                if let Some(details) = details {
                    error["details"] = details;
                }
                vocoder_typert::RpcResult::Err {
                    ok: vocoder_typert::OkTag(false),
                    error: serde_json::from_value(error).unwrap(),
                }
            }
        };
        let body = encode_rpc_server_response(&ServerResponse {
            rpc_id: rpc_id.clone(),
            result,
        });
        return (StatusCode::OK, String::from_utf8(body).unwrap());
    }

    respond_err(
        rpc_id,
        "gateway/internal",
        "no result from business machine".into(),
    )
}

fn registry_owner_register(
    registry: &mut vocoder_typert::dispatch::NamespaceRegistry,
    namespace: &str,
    owner: &str,
) {
    use vocoder_typert::dispatch::REGISTER_NAMESPACE;
    registry.handle(MachineIn::Event {
        name: EventName::new(REGISTER_NAMESPACE),
        payload: serde_json::json!({ "namespace": namespace, "owner": owner }),
    });
}

/// Read `<home>/settings.json` for the settings machine's constructor.
/// Absent or unreadable is `None`; the machine treats that as an empty document.
fn read_settings_document(home: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(home.join("settings.json")).ok()
}

/// The host account's home directory, for the directory picker's listing root.
///
/// A machine may not consult the environment (that is a process read, like
/// any other I/O), so the driver resolves it at mount and hands it over.
/// Falls back to `/` when unset — a browse root that always exists beats
/// refusing the namespace outright.
fn home_dir() -> String {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "/".to_string())
}

/// The shared-agent config root (`$DSH_AGENTS_HOME`, else `~/.agents`).
///
/// Upstream's skill and preset discovery both read it, and both are process
/// facts rather than machine state, so the driver resolves it once.
fn agents_home() -> String {
    std::env::var("DSH_AGENTS_HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| format!("{}/.agents", home_dir()))
}

/// The credentials machine's boot reads.
///
/// All four are I/O, so they happen here: the managed document's text, the
/// launching environment, and the two `.env` fallback layers. Both `.env`
/// paths mirror upstream's layered launch environment — the invoking
/// directory's file, then the harness home's.
fn credentials_boot(
    home: &std::path::Path,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let inherited = std::env::vars().collect();
    let cwd_env = std::env::current_dir()
        .ok()
        .map(|cwd| parse_env_file(&cwd.join(".env")))
        .unwrap_or_default();
    let user_env = parse_env_file(&home.join(".env"));
    (inherited, cwd_env, user_env)
}

/// Parse a `.env` file's `NAME=value` lines.
///
/// Deliberately the simple shape: `#` comments, a leading `export`, and
/// optionally-quoted values. Nothing here is authoritative — these are the
/// lowest-precedence fallback layer, and a line this does not understand is
/// skipped rather than failing the boot.
fn parse_env_file(path: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let mut value = value.trim();
        // Strip one matching pair of surrounding quotes, keeping interior
        // spaces — which is exactly what the trimming above would lose.
        for q in ['"', '\''] {
            if value.len() >= 2 && value.starts_with(q) && value.ends_with(q) {
                value = &value[1..value.len() - 1];
                break;
            }
        }
        out.insert(name.to_string(), value.to_string());
    }
    out
}

/// Serve the web GUI's index with the client-module boot table injected.
///
/// The dist is static and carries no graph; the injection table (facade, batch
/// preloads, `__DSH_BOOT__`) is what makes it bootable. See `web_boot`.
async fn serve_index(
    dist: std::path::PathBuf,
    boot: std::sync::Arc<web_boot::Boot>,
) -> axum::response::Response {
    let path = dist.join("index.html");
    let html = match std::fs::read_to_string(&path) {
        Ok(h) => h,
        Err(e) => {
            return axum::response::Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(format!("no index.html in dist: {e}").into())
                .unwrap();
        }
    };
    axum::response::Html(web_boot::inject_into_index(&html, &boot.graph)).into_response()
}

/// Serve a `/plugins/??<id>/client.js,<id>/client.js&rev=<rev>` combo.
///
/// The specifier begins with `??`, so it is the request's **query string**, not
/// its path: axum routes `/plugins/{*resource}` on the path alone and would see
/// an empty capture. The handler therefore takes the raw request target
/// (`pathname + search`) and strips the `/plugins/` prefix, which is exactly the
/// key upstream registers its combo responses under. `rev` rides in the same
/// string after `&rev=`; a rev this host did not issue still resolves, because
/// the response is looked up by resource list — which is what a cache-busting
/// query needs.
async fn serve_combo(
    uri: axum::http::Uri,
    boot: std::sync::Arc<web_boot::Boot>,
) -> axum::response::Response {
    let target = uri
        .path_and_query()
        .map(axum::http::uri::PathAndQuery::as_str)
        .unwrap_or("");
    let Some(resource) = target.strip_prefix("/plugins/") else {
        return axum::response::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("combo route expects /plugins/??<id>/client.js".into())
            .unwrap();
    };
    let (list, rev) = match resource.split_once("&rev=") {
        Some((list, rev)) => (list, rev.to_string()),
        None => (resource, String::new()),
    };
    let Some(ids) = list.strip_prefix("??") else {
        return axum::response::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("combo route expects /plugins/??<id>/client.js".into())
            .unwrap();
    };
    // Split into resource names and normalize each to its package id. A single
    // request is all-script or all-map; the map suffix on any resource selects
    // the map form for the whole response, matching how the client builds a
    // source-map request (one map URL per combo script).
    let mut map = false;
    let mut packages = Vec::new();
    for one in ids.split(',').filter(|s| !s.is_empty()) {
        let stem = if let Some(stem) = one.strip_suffix("/client.js") {
            stem
        } else if let Some(stem) = one.strip_suffix("/client.js.map") {
            map = true;
            stem
        } else {
            return axum::response::Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(format!("bad combo resource: {one}").into())
                .unwrap();
        };
        packages.push(stem.to_string());
    }
    if packages.is_empty() {
        return axum::response::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("empty combo".into())
            .unwrap();
    }
    match boot.combo(&packages, &rev, map) {
        Ok(body) => axum::response::Response::builder()
            .status(StatusCode::OK)
            .header("content-type", web_boot::Boot::combo_content_type(map))
            // Immutable: the URL carries the revision, so a changed bundle is a
            // changed URL.
            .header("cache-control", "public, max-age=31536000, immutable")
            .body(body.into())
            .unwrap(),
        Err(e) => axum::response::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(format!("combo failed: {e}").into())
            .unwrap(),
    }
}

/// The client-module dev channel: `GET /plugins/events`.
///
/// This is the host half of `client-hmr` (`packages/client/hmr/src/index.ts:158`),
/// the SSE stream the browser opens to learn a bundle was rebuilt. Faithful to
/// upstream in framing, in the connect-time `graph` snapshot, and — importantly
/// — in what it does *not* do here: no filesystem watcher exists, so a
/// `rebuilt` frame is never emitted. The graph is composed once at boot, so the
/// connect snapshot is the boot graph and never changes.
///
/// It is served rather than omitted because its absence is a *console error* in
/// the client, and the e2e boot cell is "no console errors": an `EventSource`
/// against a 404 fires the browser's resource error, which this axis counts.
async fn serve_plugin_events(boot: std::sync::Arc<web_boot::Boot>) -> axum::response::Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    // `sseData`'s frame, verbatim: `data: <json>\n\n`. axum's `Event` renders
    // the same `data:` framing; the graph value is the same `__DSH_BOOT__`
    // object, so a client receiving it sees no difference from the control's
    // connect frame.
    let graph = serde_json::json!({ "type": "graph", "graph": boot.graph.to_json() });
    let first = futures_util::stream::once(async move {
        Ok::<_, std::convert::Infallible>(Event::default().data(graph.to_string()))
    });
    // The connection stays open after the snapshot, as upstream's does: the
    // writer holds the response rather than ending the body.
    use futures_util::StreamExt;
    let stream = first.chain(futures_util::stream::pending());
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn uuid() -> String {
    // Cheap session id; uniqueness across a single process is enough here.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{:x}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}
