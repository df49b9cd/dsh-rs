use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "vocoderd", about = "Pure-Rust dsh backend (spec-driven)")]
enum Cmd {
    /// Serve the web host: static client + /api WS gateway.
    Serve(ServeArgs),
}

#[derive(clap::Args)]
struct ServeArgs {
    /// Harness home (sessions, workspaces).
    #[arg(long, default_value = ".scratch/vocoderd-home")]
    home: std::path::PathBuf,
    /// Directory holding the extracted spec/ artifacts.
    #[arg(long, default_value = "spec")]
    spec: std::path::PathBuf,
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
    #[arg(long, default_value_t = 3080)]
    port: u16,
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
    let spec_dir = args.spec.canonicalize().unwrap_or(args.spec);
    tracing::info!(?spec_dir, home = ?args.home, "vocoderd starting (M0 stub)");
    // TODO(M1): axum app — GET / serves built client, /api WS → Typert gateway.
    eprintln!("vocoderd: M0 stub — gateway not yet implemented; conformance wire suite pending");
    std::process::exit(2)
}
