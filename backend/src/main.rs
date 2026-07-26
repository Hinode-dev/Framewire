//! Framewire signaling backend.
//!
//! Relays WebRTC offer/answer messages between a streaming host and its
//! viewers, and serves the viewer page and STUN configuration. No media
//! (RTP) passes through this server, and there is no TURN relay.
//!
//! - `GET /v1/host-signal` (WebSocket): the host app connects here. On
//!   connect it receives a room code, then exchanges per-viewer offer/answer
//!   messages. Passing `?room_code=XXXXXX` reuses that code if it's free.
//!   `?client_version=X.Y.Z` reports the host build's version; if it's below
//!   `FW_MIN_HOST_VERSION` (or missing), the connection is rejected with a
//!   `version_rejected` message before a room is created. If `FW_HOST_TOKEN`
//!   is set, the `x-framewire-host-token` header must match it or the
//!   connection is rejected with `unauthorized`.
//! - `GET /v1/rooms/{room_code}/viewer-signal` (WebSocket): a viewer's
//!   browser connects here, sends an offer, and receives the matching
//!   answer. Repeated wrong room codes are throttled globally (see
//!   `ratelimit.rs`).
//! - `GET /watch/{room_code}`: serves the viewer page.
//! - `GET /ice-config`: STUN configuration for viewers. No TURN: a viewer
//!   that can't reach the host via P2P simply fails to connect.
//! - `GET /`: the product/download page (see `site.rs`).

mod config;
mod ratelimit;
mod roomcode;
mod rooms;
mod site;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use config::Config;
use ratelimit::FailureRateLimiter;
use rooms::RoomStore;

const VIEWER_HTML: &str = include_str!("../assets/viewer.html");

struct AppState {
    config: Config,
    rooms: RoomStore,
    /// Throttles repeated wrong room codes on `/v1/rooms/{code}/viewer-signal`.
    viewer_lookup_limiter: FailureRateLimiter,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env();
    let bind_addr = config.bind_addr.clone();
    let state = Arc::new(AppState {
        config,
        rooms: RoomStore::default(),
        viewer_lookup_limiter: FailureRateLimiter::new(20, Duration::from_secs(60)),
    });

    let app = Router::new()
        .route("/v1/host-signal", get(host_signal))
        .route("/v1/rooms/{room_code}/viewer-signal", get(viewer_signal))
        .route("/watch/{room_code}", get(watch_page))
        .route("/ice-config", get(ice_config))
        .merge(site::router())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    println!("[backend] listening on http://{bind_addr}/");
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Deserialize)]
struct HostSignalQuery {
    /// Room code to reuse on reconnect, if available.
    room_code: Option<String>,
    /// The connecting host build's version (`CARGO_PKG_VERSION`). Absent on
    /// builds old enough to predate this check.
    client_version: Option<String>,
}

async fn host_signal(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(query): Query<HostSignalQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let token = headers
        .get("x-framewire-host-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    ws.on_upgrade(move |socket| {
        handle_host_socket(socket, state, query.room_code, query.client_version, token)
    })
}

/// Parses a `major.minor.patch` version string loosely: missing or
/// non-numeric components default to 0, so "0.2" and "0.2.0" compare equal
/// and a garbled string just sorts as very old rather than erroring.
fn parse_version(v: &str) -> (u32, u32, u32) {
    let mut parts = v.split('.');
    let mut next = || parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (next(), next(), next())
}

async fn handle_host_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    requested_code: Option<String>,
    client_version: Option<String>,
    host_token: Option<String>,
) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    if let Some(expected) = &state.config.host_token {
        if host_token.as_deref() != Some(expected.as_str()) {
            println!("[backend] rejected host: bad or missing host token, no room created");
            let msg = json!({"type": "unauthorized"}).to_string();
            let _ = ws_tx.send(Message::Text(msg.into())).await;
            return;
        }
    }

    if let Some(min_version) = &state.config.min_host_version {
        let accepted = client_version
            .as_deref()
            .is_some_and(|v| parse_version(v) >= parse_version(min_version));
        if !accepted {
            println!(
                "[backend] rejected host (client_version={:?} < min={min_version}): no room created",
                client_version
            );
            let msg = json!({"type": "version_rejected", "min_version": min_version}).to_string();
            let _ = ws_tx.send(Message::Text(msg.into())).await;
            return;
        }
    }

    let (relay_tx, mut relay_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let room_code = state
        .rooms
        .register_host(requested_code.as_deref(), relay_tx);

    let welcome = json!({"type": "room_created", "room_code": room_code}).to_string();
    if ws_tx.send(Message::Text(welcome.into())).await.is_err() {
        state.rooms.remove_room(&room_code);
        return;
    }
    println!("[backend] host connected: room_code={room_code}");

    let forward_task = tokio::spawn(async move {
        while let Some(msg) = relay_rx.recv().await {
            if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = ws_rx.next().await {
        if let Message::Text(text) = msg {
            handle_host_message(&state, &room_code, &text);
        }
    }

    forward_task.abort();
    state.rooms.remove_room(&room_code);
    println!("[backend] host disconnected: room_code={room_code} (room discarded)");
}

/// Relays a host's `{"type":"answer","viewer_id":"...","sdp":"..."}` to the
/// matching viewer as `{"type":"answer","sdp":"..."}`.
fn handle_host_message(state: &Arc<AppState>, room_code: &str, text: &str) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        eprintln!("[backend] failed to parse message from host: {text}");
        return;
    };
    if value.get("type").and_then(Value::as_str) != Some("answer") {
        return;
    }
    let (Some(viewer_id), Some(sdp)) = (
        value.get("viewer_id").and_then(Value::as_str),
        value.get("sdp").and_then(Value::as_str),
    ) else {
        return;
    };
    let Some(room) = state.rooms.get(room_code) else {
        return;
    };
    room.send_to_viewer(viewer_id, json!({"type": "answer", "sdp": sdp}).to_string());
}

async fn viewer_signal(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(room_code): Path<String>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_viewer_socket(socket, state, room_code))
}

async fn handle_viewer_socket(socket: WebSocket, state: Arc<AppState>, room_code: String) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    if !state.viewer_lookup_limiter.is_allowed() {
        let _ = ws_tx
            .send(Message::Text(
                json!({"type": "error", "message": "Too many attempts, try again later"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }

    let Some(room) = state.rooms.get(&room_code) else {
        state.viewer_lookup_limiter.record_failure();
        let _ = ws_tx
            .send(Message::Text(
                json!({"type": "error", "message": "Stream not found (wrong code, or the stream hasn't started yet)"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    };

    let viewer_id = roomcode::generate_viewer_id();
    let (relay_tx, mut relay_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    room.add_viewer(viewer_id.clone(), relay_tx);
    println!("[backend] viewer connected: room_code={room_code} viewer_id={viewer_id}");

    let forward_task = tokio::spawn(async move {
        while let Some(msg) = relay_rx.recv().await {
            if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = ws_rx.next().await {
        if let Message::Text(text) = msg {
            handle_viewer_message(&state, &room_code, &viewer_id, &text);
        }
    }

    forward_task.abort();
    room.remove_viewer(&viewer_id);
    room.notify_host(json!({"type": "viewer_left", "viewer_id": viewer_id}).to_string());
    println!("[backend] viewer disconnected: room_code={room_code} viewer_id={viewer_id}");
}

/// Relays a viewer's `{"type":"offer","sdp":"..."}` to the host as
/// `{"type":"offer","viewer_id":"...","sdp":"..."}`.
fn handle_viewer_message(state: &Arc<AppState>, room_code: &str, viewer_id: &str, text: &str) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        eprintln!("[backend] failed to parse message from viewer: {text}");
        return;
    };
    if value.get("type").and_then(Value::as_str) != Some("offer") {
        return;
    }
    let Some(sdp) = value.get("sdp").and_then(Value::as_str) else {
        return;
    };
    let Some(room) = state.rooms.get(room_code) else {
        return;
    };
    room.notify_host(json!({"type": "offer", "viewer_id": viewer_id, "sdp": sdp}).to_string());
}

/// Serves the viewer page. The page's JS reads the room code from the URL
/// path and talks to `/v1/rooms/{room_code}/viewer-signal` and `/ice-config`
/// directly. The room's existence isn't checked here, so a link can be
/// shared before the stream actually starts; the page shows a waiting state
/// and retries.
async fn watch_page(Path(_room_code): Path<String>) -> impl IntoResponse {
    Html(VIEWER_HTML)
}

async fn ice_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "iceServers": [{
            "urls": [format!("stun:{}", state.config.stun_server)],
        }]
    }))
}
