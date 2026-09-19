//! vocoderd — the Rust web host. Thin async driver over the Sans-I/O core.

#[cfg(test)]
mod composition;
mod driver;
mod machines;
mod registry;
mod rpc;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
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
use vocoder_cordis::{EventName, MachineId, MachineIn, PluginMachine, RouteIn, RouteOut, Router};
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
    fn pump(&self, to: &MachineId, ev: MachineIn) -> Vec<RouteOut> {
        let mut terminal = Vec::new();
        let mut pending = Some(ev);
        // One whole effect loop at a time; see [`AppState::dispatch`].
        let _serialized = self.dispatch.lock();
        for _ in 0..crate::driver::MAX_EFFECTS_PER_INPUT {
            let Some(input) = pending.take() else { break };
            let outs = self.router.lock().handle(RouteIn::Deliver {
                to: to.clone(),
                ev: input,
            });

            let mut effects = Vec::new();
            for out in outs {
                match out {
                    RouteOut::Realize { id, request, .. } => effects.push((id, request)),
                    terminal_out => terminal.push(terminal_out),
                }
            }
            // Route any stream frames immediately; they are not answers.
            if terminal
                .iter()
                .any(|o| matches!(o, RouteOut::Stream { .. }))
            {
                let (streams, rest): (Vec<_>, Vec<_>) = terminal
                    .drain(..)
                    .partition(|o| matches!(o, RouteOut::Stream { .. }));
                self.route_stream_outs(streams);
                terminal = rest;
            }
            if effects.is_empty() {
                break;
            }
            // Feed the first answer back; a machine awaiting several effects
            // issues them one at a time, so at most one is awaited per turn.
            let (id, request) = effects.remove(0);
            let result =
                crate::driver::realize(request).unwrap_or(vocoder_cordis::EffectResult::Done);
            pending = Some(MachineIn::EffectResult { id, result });
        }
        terminal
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
                    message,
                    details,
                } => vocoder_typert::StreamServerMessage::Error {
                    stream_id,
                    error: vocoder_typert::StreamFailure {
                        name,
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

    let mut initial_router = Router::new();
    // Business machines mounted at boot (M3+: from profile composition).
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("goals"),
        machine: Box::new(crate::machines::goals::GoalsMachine::default()),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("session"),
        machine: Box::new(crate::machines::session::SessionMachine::new(
            sessions_root.clone(),
            workspace_registry.clone(),
        )),
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
        id: MachineId::new("$events"),
        machine: Box::new(crate::machines::events::EventsMachine::default()),
    });
    initial_router.handle(RouteIn::Deliver {
        to: MachineId::new("$events"),
        ev: MachineIn::ServicesReady { keys: vec![] },
    });
    let mut registry = vocoder_typert::dispatch::NamespaceRegistry::new();
    registry_owner_register(&mut registry, "goals", "goals");
    registry_owner_register(&mut registry, "session", "session");
    registry_owner_register(&mut registry, "workspace", "workspace");
    registry_owner_register(&mut registry, "workspaceFiles", "workspaceFiles");
    registry_owner_register(&mut registry, "settings", "settings");
    registry_owner_register(&mut registry, "$events", "$events");

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
        let boot = boot_script(&args);
        let dist2 = dist.clone();
        info!(dist = ?args.web_dist, "serving web GUI");
        app.route("/", get(move || serve_index(dist.clone(), boot.clone())))
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
                                        message: format!(
                                            "no such namespace on this host: {namespace}"
                                        ),
                                        details: None,
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
                        let text = String::from_utf8(vocoder_typert::encode_stream_server(
                            &StreamServerMessage::End { stream_id },
                        ))
                        .unwrap();
                        let _ = socket.send(Message::Text(text.into())).await;
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
    let respond_err = |rpc_id: String, code: &str, message: String| -> (StatusCode, String) {
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
    };

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

    let outs = state.pump(
        &owner_id,
        MachineIn::Event {
            name: EventName::new(crate::rpc::call_event(&namespace)),
            payload: serde_json::json!({
                "method": method,
                "args": req.payload.get("args").cloned().unwrap_or(serde_json::Value::Null),
            }),
        },
    );

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
            rpc_id: req.rpc_id.clone(),
            result,
        });
        return (StatusCode::OK, String::from_utf8(body).unwrap());
    }

    respond_err(
        req.rpc_id.clone(),
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

fn boot_script(args: &ServeArgs) -> String {
    let payload = serde_json::json!({
        "kind": "vocoder",
        "host": { "home": args.home.display().to_string(), "name": args.host_name },
    });
    format!(
        "<script>window.__DSH_BOOT__ = {};</script>",
        serde_json::to_string(&payload).unwrap()
    )
}

async fn serve_index(dist: std::path::PathBuf, boot: String) -> axum::response::Response {
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
    let html = html.replacen("<head>", &format!("<head>\n    {boot}\n  "), 1);
    axum::response::Html(html).into_response()
}

fn uuid() -> String {
    // Cheap session id; uniqueness across a single process is enough here.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{:x}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}
