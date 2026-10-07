//! FluidEDA collaboration server — see the library docs for the API.
//!
//! Environment:
//! * `FLUIDEDA_STORE` — store directory (default `store`, relative to the cwd).
//! * `FLUIDEDA_ADDR` — listen address (default `127.0.0.1:9000`; the dev app
//!   reaches it through Trunk's proxy, so nothing needs a wider bind).
//! * `RUST_LOG` — log filter (default `info`).
//! * `FLUIDEDA_LATENCY_MS` — testing: every blob read waits this long first,
//!   standing in for network latency (Phase 7.1c; default 0).
//! * `FLUIDEDA_AUTH` — `on` or `off`: whether requests need a link token
//!   (Phase 7.2). Default: on, unless the server listens on a loopback
//!   address.
//! * `FLUIDEDA_UPLOAD_QUOTA_GB` — what one link may upload (default 32).
//!
//! `fluideda-server token <room> <owner|editor|viewer> [ttl-seconds]` prints
//! a link token for a room and exits: how the first owner link is made.
//! Owners then share editor and viewer links from the app.
//!
//! TLS belongs to a reverse proxy (Caddy, nginx, …) in front of the server,
//! terminating HTTPS and forwarding `/ws` (as a WebSocket), `/blobs`,
//! `/rooms`, `/packs` and `/export` to it. Links carry their token in the
//! address, so serve them over HTTPS only.

use fluideda_server::auth::{Auth, Role};
use fluideda_server::room::{Hub, SnapshotPolicy};
use fluideda_server::store::Store;
use std::sync::Arc;

/// Links are required unless the server only listens on loopback.
fn auth_required(addr: &str) -> bool {
    match std::env::var("FLUIDEDA_AUTH").as_deref() {
        Ok("on") => true,
        Ok("off") => false,
        _ => {
            let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h).trim_matches(['[', ']']);
            !matches!(host, "127.0.0.1" | "::1" | "localhost")
        }
    }
}

/// `token <room> <role> [ttl]`: print a link token and exit.
fn print_token(dir: &str, args: &[String]) {
    let usage = "usage: fluideda-server token <room> <owner|editor|viewer> [ttl-seconds]";
    let (Some(room), Some(role)) = (args.first(), args.get(1).and_then(|r| Role::parse(r))) else {
        eprintln!("{usage}");
        std::process::exit(2);
    };
    if !fluideda_server::store::is_room_name(room) {
        eprintln!("room names are 1-64 characters of [A-Za-z0-9_-]");
        std::process::exit(2);
    }
    let store = Store::open(dir).unwrap_or_else(|e| panic!("cannot open store {dir}: {e}"));
    let auth = Auth::open(store.root(), true).unwrap_or_else(|e| panic!("store secret: {e}"));
    let expires = args.get(2).and_then(|s| s.parse::<u64>().ok()).map_or(0, |ttl| fluideda_server::auth::now() + ttl);
    println!("{}", auth.mint(room, role, expires, store.room_epoch(room)));
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let dir = std::env::var("FLUIDEDA_STORE").unwrap_or_else(|_| "store".to_string());
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("token") {
        print_token(&dir, &args[1..]);
        return;
    }
    let addr = std::env::var("FLUIDEDA_ADDR").unwrap_or_else(|_| "127.0.0.1:9000".to_string());
    if let Some(ms) = std::env::var("FLUIDEDA_LATENCY_MS").ok().and_then(|v| v.parse().ok()) {
        fluideda_server::set_latency_ms(ms);
        log::warn!("testing: every blob read waits {ms} ms");
    }
    let store = Store::open(&dir).unwrap_or_else(|e| panic!("cannot open store {dir}: {e}"));
    let required = auth_required(&addr);
    let hub = Arc::new(Hub::with_auth(store, SnapshotPolicy::default(), required).unwrap_or_else(|e| panic!("store secret: {e}")));
    if required {
        log::info!("links required: run `fluideda-server token <room> owner` for a room's first link");
    }
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
