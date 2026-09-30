//! FluidEDA collaboration server — see the library docs for the API.
//!
//! Environment:
//! * `FLUIDEDA_STORE` — store directory (default `store`, relative to the cwd).
//! * `FLUIDEDA_ADDR` — listen address (default `127.0.0.1:9000`; the dev app
//!   reaches it through Trunk's proxy, so nothing needs a wider bind).
//! * `RUST_LOG` — log filter (default `info`).

use fluideda_server::room::{Hub, SnapshotPolicy};
use fluideda_server::store::Store;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let dir = std::env::var("FLUIDEDA_STORE").unwrap_or_else(|_| "store".to_string());
    let addr = std::env::var("FLUIDEDA_ADDR").unwrap_or_else(|_| "127.0.0.1:9000".to_string());
    let store = Store::open(&dir).unwrap_or_else(|e| panic!("cannot open store {dir}: {e}"));
    let hub = Arc::new(Hub::new(store, SnapshotPolicy::default()));
    fluideda_server::spawn_snapshotter(hub.clone());

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    log::info!("FluidEDA server on {addr}, store {dir}");
    tokio::select! {
        r = fluideda_server::serve(listener, hub.clone()) => {
            if let Err(e) = r {
                log::error!("server error: {e}");
            }
        }
        _ = shutdown_signal() => log::info!("shutting down"),
    }
    // Compact every room with unsnapshotted updates. (Their WALs would
    // recover them anyway; this leaves a clean store.)
    let _ = tokio::task::spawn_blocking(move || hub.flush()).await;
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}
