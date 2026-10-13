//! maitred: an OpenAI-compatible `/v1` front for self-hosted LLM servers that share GPUs.
//!
//! It forwards requests to the server that serves the named model (bodies pass through untouched except
//! for the model name and the reasoning effort), checks virtual keys, lists every model with its context
//! window, reasoning efforts and live status, and answers a request for a model whose server is not up
//! with what to use instead.

mod auth;
mod config;
mod effort;
mod mcp;
mod proxy;
mod status;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use std::path::PathBuf;
use std::sync::Arc;

/// Requests carry base64 images and long conversations.
const BODY_LIMIT: usize = 256 << 20;

fn config_path() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--config" {
            return args.next().map(PathBuf::from);
        }
        if let Some(p) = a.strip_prefix("--config=") {
            return Some(PathBuf::from(p));
        }
    }
    std::env::var_os("MAITRED_CONFIG").map(PathBuf::from)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(false)
        .init();
    let Some(path) = config_path() else {
        eprintln!("usage: maitred --config FILE (or MAITRED_CONFIG)");
        std::process::exit(2);
    };
    let config = config::Config::load(&path).unwrap_or_else(|e| {
        eprintln!("maitred: {e}");
        std::process::exit(1);
    });
    let listen = config.listen.clone();
    let state = proxy::AppState::new(config).unwrap_or_else(|e| {
        eprintln!("maitred: {e}");
        std::process::exit(1);
    });
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/mcp", post(mcp::post).get(mcp::other).delete(mcp::other))
        .route("/v1/models", get(proxy::models))
        .route("/v1/models/{*id}", get(proxy::model))
        .route("/v1/{*rest}", post(proxy::v1))
        .fallback(proxy::fallback)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(Arc::new(state));
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| {
            eprintln!("maitred: listen {listen}: {e}");
            std::process::exit(1);
        });
    tracing::info!("listening on {listen}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        })
        .await
        .expect("server");
}
