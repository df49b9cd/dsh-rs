//! vocoderd — the Rust web host. Thin async driver over the Sans-I/O core.

mod machines;
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

    let mut initial_router = Router::new();
    // Business machines mounted at boot (M3+: from profile composition).
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("goals"),
        machine: Box::new(crate::machines::goals::GoalsMachine::default()),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("session"),
        machine: Box::new(crate::machines::session::SessionMachine::new(sessions_root.clone())),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("workspace"),
        machine: Box::new(crate::machines::workspace::WorkspaceMachine::new(
            &args.home,
            sessions_root.clone(),
        )),
    });
    initial_router.handle(RouteIn::Mount {
        id: MachineId::new("settings"),
        machine: Box::new(crate::machines::settings::SettingsMachine::new(&args.home)),
    });
    let mut registry = vocoder_typert::dispatch::NamespaceRegistry::new();
    registry_owner_register(&mut registry, "goals", "goals");
    registry_owner_register(&mut registry, "session", "session");
    registry_owner_register(&mut registry, "workspace", "workspace");
    registry_owner_register(&mut registry, "settings", "settings");

    let state = Arc::new(AppState {
        router: Mutex::new(initial_router),
        namespace_registry: Mutex::new(registry),
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
        "<!doctype html><title>vocoderd</title><h1>vocoderd is up</h1>".to_string(),
    )
}

// ---------------------------------------------------------------------------
// /api/remote.mux — WS glue to a fresh mux-session machine per connection
// ---------------------------------------------------------------------------

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_conn(socket, state))
}

async fn ws_conn(mut socket: WebSocket, state: Arc<AppState>) {
    let session_id = MachineId::new(format!("mux-{}", uuid())); // simple id; no uuid dep
    // Mount this connection's mux machine.
    {
        let mut router = state.router.lock();
        router.handle(RouteIn::Mount {
            id: session_id.clone(),
            machine: Box::new(MuxSessionMachine::new()),
        });
        // Activate it.
        router.handle(RouteIn::Deliver {
            to: session_id.clone(),
            ev: MachineIn::ServicesReady { keys: vec![] },
        });
    }

    // The machine only *emits* socket writes via Realize; the router routes
    // them back through the Host — but for the WS connection the driver has
    // to do the write itself. So the "realizes" we care about here come
    // straight from the machine's outputs; we route around the broker.
    loop {
        let msg = match socket.recv().await {
            Some(Ok(m)) => m,
            _ => break,
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            _ => continue,
        };
        // Route the WS text into the machine.
        let outs = {
            let mut router = state.router.lock();
            router.handle(RouteIn::Deliver {
                to: session_id.clone(),
                ev: MachineIn::Event {
                    name: EventName::new("ws.text"),
                    payload: serde_json::json!({ "text": text }),
                },
            })
        };
        for out in outs {
            match out {
                RouteOut::Realize { request, .. } => {
                    if let vocoder_cordis::RealizeRequest::Raw(v) = request
                        && v.get("kind").and_then(|k| k.as_str()) == Some("ws.send-text")
                        && let Some(text) = v.get("text").and_then(|t| t.as_str())
                    {
                        let _ = socket.send(Message::Text(text.to_string().into())).await;
                    }
                }
                other => {
                    // Realize/ServiceRegistered/Compensate not handled yet.
                    let _ = other;
                }
            }
        }
    }

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

    let outs = state.router.lock().handle(RouteIn::Deliver {
        to: owner_id.clone(),
        ev: MachineIn::Event {
            name: EventName::new(crate::rpc::call_event(&namespace)),
            payload: serde_json::json!({
                "method": method,
                "args": req.payload.get("args").cloned().unwrap_or(serde_json::Value::Null),
            }),
        },
    });
    let _ = owner_id;

    for out in outs {
        if let RouteOut::Realize { request, .. } = out
            && let vocoder_cordis::RealizeRequest::Raw(v) = request
            && v.get("kind").and_then(|k| k.as_str()) == Some("rpc.result")
        {
            let result = v["result"].clone();
            let body = if result["ok"].as_bool() == Some(true) {
                encode_rpc_server_response(&ServerResponse {
                    rpc_id: req.rpc_id.clone(),
                    result: vocoder_typert::RpcResult::Ok {
                        ok: vocoder_typert::OkTag(true),
                        value: result["value"].clone(),
                    },
                })
            } else {
                encode_rpc_server_response(&ServerResponse {
                    rpc_id: req.rpc_id.clone(),
                    result: vocoder_typert::RpcResult::Err {
                        ok: vocoder_typert::OkTag(false),
                        error: serde_json::from_value(result["error"].clone()).unwrap(),
                    },
                })
            };
            return (StatusCode::OK, String::from_utf8(body).unwrap());
        }
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
