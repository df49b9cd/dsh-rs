//! vocoderd — the Rust web host. Thin async driver over the Sans-I/O core.

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
}

/// Shared state across HTTP/WS handlers.
struct AppState {
    /// The machine tree; synchronized because handlers touch it from
    /// connection tasks.
    router: Mutex<Router>,
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

    let state = Arc::new(AppState {
        router: Mutex::new(Router::new()),
    });

    let app = AxumRouter::new()
        .route("/", get(index))
        .route("/api/remote.mux", any(ws_upgrade))
        .route("/api/{*endpoint}", post(api_rpc))
        .route("/healthz", get(|| async { StatusCode::OK }))
        .with_state(state);

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
    State(_state): State<Arc<AppState>>,
    axum::extract::Path(endpoint): axum::extract::Path<String>,
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

    // No business machines mounted yet: reported as a stable gateway error.
    let _ = endpoint;
    respond_err(
        req.rpc_id.clone(),
        "gateway/internal",
        "endpoint is not implemented on this host".to_string(),
    )
}

fn uuid() -> String {
    // Cheap session id; uniqueness across a single process is enough here.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{:x}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}
