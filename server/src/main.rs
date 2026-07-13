//! FluidEDA relay server.
//!
//! Deliberately dumb for Phase 0: a per-room broadcast hub. It does NOT parse
//! or understand yrs updates or presence — it just forwards every binary
//! message from one peer to all the others in the same room. Convergence is
//! guaranteed by yrs on the clients. Persistence, the y-sync handshake for late
//! joiners, and content-addressed storage come in Phase 4.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{sink::SinkExt, stream::StreamExt};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

/// room name -> broadcast channel of (sender_peer_id, message bytes).
type Rooms = Arc<Mutex<HashMap<String, broadcast::Sender<(u64, Vec<u8>)>>>>;

#[derive(Deserialize)]
struct RoomQuery {
    #[serde(default = "default_room")]
    room: String,
}

fn default_room() -> String {
    "demo".to_string()
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(|| async { "ok" }))
        .with_state(rooms);

    let addr = "0.0.0.0:9000";
    log::info!("FluidEDA relay listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<RoomQuery>,
    State(rooms): State<Rooms>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, q.room, rooms))
}

async fn handle_socket(socket: WebSocket, room: String, rooms: Rooms) {
    let peer_id = rand_id();
    let tx = {
        let mut guard = rooms.lock().unwrap();
        guard
            .entry(room.clone())
            .or_insert_with(|| broadcast::channel(1024).0)
            .clone()
    };
    let mut rx = tx.subscribe();
    log::info!("peer {peer_id} joined room {room}");

    let (mut sink, mut stream) = socket.split();

    // Fan-in: this peer's messages -> room broadcast (tagged with peer id).
    let tx_in = tx.clone();
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            if let Message::Binary(data) = msg {
                let _ = tx_in.send((peer_id, data));
            }
        }
    });

    // Fan-out: room broadcast -> this peer (skip our own messages).
    let mut send_task = tokio::spawn(async move {
        while let Ok((from, data)) = rx.recv().await {
            if from == peer_id {
                continue;
            }
            if sink.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut recv_task => send_task.abort(),
        _ = &mut send_task => recv_task.abort(),
    }
    log::info!("peer {peer_id} left room {room}");
}

fn rand_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    (n as u64).wrapping_mul(0x9E3779B97F4A7C15) ^ (n as u64 >> 17)
}
