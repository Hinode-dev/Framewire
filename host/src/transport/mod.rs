//! WebRTC sending, P2P mesh style.
//!
//! The NVENC-encoded bitstream is produced once and shared across all
//! viewers. Each viewer gets its own `RTCPeerConnection`, and the same
//! encoded frames are duplicated to every connection.
//!
//! Two signaling paths are supported:
//! - **Direct mode** (`start_server`): serves `/offer` over plain HTTP.
//!   Single-viewer, LAN-only, used for local testing.
//! - **Mesh mode** (`start_mesh_publisher`): connects outbound to
//!   `backend/` over WebSocket, which relays per-viewer offers/answers.
//!   This is the multi-viewer path used for real deployments.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::receiver_report::ReceiverReport;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc_ice::udp_network::{EphemeralUDP, UDPNetwork};

use crate::upnp::PortForward;

/// Builds a `MediaEngine` registering only H.264 High Profile, matching
/// what NVENC actually outputs.
fn build_h264_high_media_engine() -> anyhow::Result<MediaEngine> {
    let mut media_engine = MediaEngine::default();

    let video_rtcp_feedback = vec![
        RTCPFeedback {
            typ: "goog-remb".to_owned(),
            parameter: "".to_owned(),
        },
        RTCPFeedback {
            typ: "transport-cc".to_owned(),
            parameter: "".to_owned(),
        },
        RTCPFeedback {
            typ: "ccm".to_owned(),
            parameter: "fir".to_owned(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "".to_owned(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "pli".to_owned(),
        },
    ];

    media_engine.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                // High Profile (0x64), Level 3.1. level-asymmetry-allowed=1
                // lets the negotiated level differ from the actual stream.
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=64001f"
                        .to_owned(),
                rtcp_feedback: video_rtcp_feedback,
            },
            payload_type: 125,
            ..Default::default()
        },
        RTPCodecType::Video,
    )?;

    Ok(media_engine)
}

/// Reads RTCP for one viewer's connection and:
/// (1) sets a flag requesting the next encoded frame be a keyframe when a
///     PLI/FIR request arrives;
/// (2) tracks that viewer's desired bitrate via a simple AIMD scheme based
///     on reported packet loss. Since encoding is shared across all
///     viewers, the bitrate actually applied to NVENC is the minimum across
///     all viewers' desired values.
fn spawn_rtcp_handler(
    viewer_id: String,
    rtp_sender: Arc<RTCRtpSender>,
    force_idr: Arc<AtomicBool>,
    viewer_bitrates: Arc<Mutex<HashMap<String, u32>>>,
    target_bitrate_bps: Arc<AtomicU32>,
    max_bitrate_bps: u32,
) {
    tokio::spawn(async move {
        let floor_bps = (max_bitrate_bps / 4).max(1_000_000);
        let mut good_streak: u32 = 0;
        let mut current = max_bitrate_bps;
        let mut buf = vec![0u8; 1500];
        while let Ok((packets, _attrs)) = rtp_sender.read(&mut buf).await {
            for pkt in packets {
                let any = pkt.as_any();
                if any.downcast_ref::<PictureLossIndication>().is_some()
                    || any.downcast_ref::<FullIntraRequest>().is_some()
                {
                    force_idr.store(true, Ordering::SeqCst);
                }
                if let Some(rr) = any.downcast_ref::<ReceiverReport>() {
                    for report in &rr.reports {
                        let loss_frac = report.fraction_lost as f64 / 256.0;
                        let desired = if loss_frac > 0.10 {
                            good_streak = 0;
                            (current as f64 * 0.70) as u32
                        } else if loss_frac > 0.02 {
                            good_streak = 0;
                            (current as f64 * 0.85) as u32
                        } else {
                            good_streak += 1;
                            if good_streak >= 3 {
                                good_streak = 0;
                                (current as f64 * 1.05) as u32
                            } else {
                                current
                            }
                        };
                        current = desired.clamp(floor_bps, max_bitrate_bps);

                        let mut bitrates = viewer_bitrates.lock().await;
                        bitrates.insert(viewer_id.clone(), current);
                        let global_min = bitrates.values().copied().min().unwrap_or(max_bitrate_bps);
                        drop(bitrates);

                        let prev = target_bitrate_bps.swap(global_min, Ordering::Relaxed);
                        if prev != global_min {
                            println!(
                                "[transport] bitrate adapt (viewer={viewer_id}): target {}kbps -> {}kbps (fraction_lost={:.1}%)",
                                prev / 1000,
                                global_min / 1000,
                                loss_frac * 100.0
                            );
                        }
                    }
                }
            }
        }
        println!("[transport] RTCP read loop ended (viewer={viewer_id} disconnected)");
    });
}

/// Builds the STUN-only ICE server list, shared by the host's own
/// `RTCPeerConnection`s and distributed to browsers via `/ice-config`.
///
/// No TURN: a viewer that can't be reached via P2P (symmetric NAT, strict
/// firewall) simply fails to connect instead of relaying media through a
pub fn build_ice_servers(stun_server: &str) -> Vec<RTCIceServer> {
    vec![RTCIceServer {
        urls: vec![format!("stun:{stun_server}")],
        username: String::new(),
        credential: String::new(),
    }]
}

const INDEX_HTML: &str = include_str!("../../assets/index.html");

/// Per-viewer state kept alive for as long as that viewer is connected.
struct ViewerHandle {
    track: Arc<TrackLocalStaticSample>,
}

struct AppState {
    viewers: Mutex<HashMap<String, ViewerHandle>>,
    /// Each viewer's current desired bitrate, used to compute the shared
    /// target below.
    viewer_bitrates: Arc<Mutex<HashMap<String, u32>>>,
    force_idr: Arc<AtomicBool>,
    ice_servers: Vec<RTCIceServer>,
    /// Current NVENC target bitrate, kept in sync with the minimum of all
    /// viewers' desired bitrates.
    target_bitrate_bps: Arc<AtomicU32>,
    max_bitrate_bps: u32,
    /// UPnP port mapping, if one could be set up — lets viewers on a
    /// different network than the host connect P2P without a TURN relay
    /// (see `upnp.rs`). `None` just means today's STUN-only behavior.
    port_forward: Option<PortForward>,
}

async fn new_app_state(ice_servers: Vec<RTCIceServer>, bitrate_bps: u32) -> Arc<AppState> {
    Arc::new(AppState {
        viewers: Mutex::new(HashMap::new()),
        viewer_bitrates: Arc::new(Mutex::new(HashMap::new())),
        force_idr: Arc::new(AtomicBool::new(false)),
        ice_servers,
        target_bitrate_bps: Arc::new(AtomicU32::new(bitrate_bps)),
        max_bitrate_bps: bitrate_bps,
        port_forward: crate::upnp::try_setup().await,
    })
}

/// Handle used by the capture/encode thread to push frames out.
#[derive(Clone)]
pub struct FrameSender {
    state: Arc<AppState>,
}

impl FrameSender {
    /// Sends the same encoded frame to every connected viewer. No-op if
    /// there are no viewers.
    pub async fn send(&self, data: Vec<u8>, duration: Duration) {
        let tracks: Vec<_> = {
            let viewers = self.state.viewers.lock().await;
            viewers.values().map(|v| v.track.clone()).collect()
        };
        if tracks.is_empty() {
            return;
        }
        let bytes = Bytes::from(data);
        for track in tracks {
            let sample = Sample {
                data: bytes.clone(),
                timestamp: SystemTime::now(),
                duration,
                ..Default::default()
            };
            if let Err(e) = track.write_sample(&sample).await {
                eprintln!("[transport] write_sample error: {e}");
            }
        }
    }

    /// Checks (and clears) whether the next frame should be a keyframe,
    /// e.g. because a new viewer just joined.
    pub fn take_force_idr(&self) -> bool {
        self.state.force_idr.swap(false, Ordering::SeqCst)
    }

    /// Current NVENC target bitrate.
    pub fn target_bitrate_bps(&self) -> u32 {
        self.state.target_bitrate_bps.load(Ordering::Relaxed)
    }

    /// The router's public IP if a UPnP port mapping was set up, for
    /// display in the GUI — `None` means viewers outside the host's own
    /// network only connect if plain STUN happens to succeed.
    pub fn port_forward_ip(&self) -> Option<String> {
        self.state.port_forward.as_ref().map(|pf| pf.external_ip.to_string())
    }
}

// ============================================================
// Direct mode: plain HTTP signaling, LAN-only, single viewer.
// ============================================================

pub async fn start_server(
    port: u16,
    ice_servers: Vec<RTCIceServer>,
    bitrate_bps: u32,
) -> anyhow::Result<FrameSender> {
    let state = new_app_state(ice_servers, bitrate_bps).await;

    let app = Router::new()
        .route("/", get(index))
        .route("/offer", post(handle_offer))
        .route("/ice-config", get(ice_config))
        .with_state(state.clone());

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("[transport] signaling server listening on http://{addr}/");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("[transport] server error: {e}");
        }
    });

    Ok(FrameSender { state })
}

async fn index() -> impl IntoResponse {
    Html(INDEX_HTML)
}

async fn ice_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::json!({ "iceServers": state.ice_servers }))
}

async fn handle_offer(
    State(state): State<Arc<AppState>>,
    Json(offer): Json<RTCSessionDescription>,
) -> impl IntoResponse {
    // Direct mode only ever has one viewer; a new offer replaces the
    // previous one.
    {
        state.viewers.lock().await.clear();
        state.viewer_bitrates.lock().await.clear();
    }
    match create_viewer_connection(&state, "direct".to_owned(), offer).await {
        Ok(answer) => Json(answer).into_response(),
        Err(e) => {
            eprintln!("[transport] failed to handle offer: {e:#}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("peer connection setup failed: {e}"),
            )
                .into_response()
        }
    }
}

// ============================================================
// Per-viewer PeerConnection setup, shared by both modes.
// ============================================================

/// Creates a `RTCPeerConnection` for one viewer and returns the answer for
/// their offer. On success, registers the track and requests a keyframe.
async fn create_viewer_connection(
    state: &Arc<AppState>,
    viewer_id: String,
    offer: RTCSessionDescription,
) -> anyhow::Result<RTCSessionDescription> {
    let mut media_engine = build_h264_high_media_engine()?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)?;

    let mut setting_engine = SettingEngine::default();
    if let Some(pf) = &state.port_forward {
        // The UPnP-mapped port range is 1:1 (external == internal), so a
        // straight IP swap on whichever port ICE happens to pick produces a
        // real, externally-reachable candidate — no TURN relay needed even
        // for viewers on a different network.
        if let Ok(udp) = EphemeralUDP::new(pf.port_min, pf.port_max) {
            setting_engine.set_udp_network(UDPNetwork::Ephemeral(udp));
        }
        setting_engine.set_nat_1to1_ips(vec![pf.external_ip.to_string()], RTCIceCandidateType::Srflx);
    }

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build();

    let config = RTCConfiguration {
        ice_servers: state.ice_servers.clone(),
        ..Default::default()
    };
    let pc = Arc::new(api.new_peer_connection(config).await?);

    let track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            ..Default::default()
        },
        "video".to_owned(),
        "framewire".to_owned(),
    ));

    let rtp_sender = pc
        .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
        .await?;
    spawn_rtcp_handler(
        viewer_id.clone(),
        rtp_sender,
        state.force_idr.clone(),
        state.viewer_bitrates.clone(),
        state.target_bitrate_bps.clone(),
        state.max_bitrate_bps,
    );

    {
        let state = state.clone();
        let viewer_id = viewer_id.clone();
        pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            println!("[transport] PeerConnectionState(viewer={viewer_id}): {s}");
            if matches!(
                s,
                RTCPeerConnectionState::Disconnected
                    | RTCPeerConnectionState::Failed
                    | RTCPeerConnectionState::Closed
            ) {
                let state = state.clone();
                let viewer_id = viewer_id.clone();
                tokio::spawn(async move {
                    remove_viewer(&state, &viewer_id).await;
                });
            }
            Box::pin(async {})
        }));
    }

    pc.set_remote_description(offer).await?;
    let answer = pc.create_answer(None).await?;

    let mut gather_complete = pc.gathering_complete_promise().await;
    pc.set_local_description(answer).await?;
    let _ = gather_complete.recv().await;

    let local_desc = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow::anyhow!("local_description not set"))?;

    state
        .viewers
        .lock()
        .await
        .insert(viewer_id, ViewerHandle { track });
    state.force_idr.store(true, Ordering::SeqCst);

    Ok(local_desc)
}

async fn remove_viewer(state: &Arc<AppState>, viewer_id: &str) {
    state.viewers.lock().await.remove(viewer_id);
    let mut bitrates = state.viewer_bitrates.lock().await;
    bitrates.remove(viewer_id);
    let new_target = bitrates.values().copied().min().unwrap_or(state.max_bitrate_bps);
    drop(bitrates);
    state.target_bitrate_bps.store(new_target, Ordering::Relaxed);
}

// ============================================================
// Mesh mode: WebSocket signaling with the backend.
// ============================================================

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, WsMessage>;
type WsSource = SplitStream<WsStream>;
type OutboundHandle = Arc<Mutex<WsSink>>;

/// Messages received from the backend (mirrors `backend/src/main.rs`).
#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HostInbound {
    RoomCreated { room_code: String },
    Offer { viewer_id: String, sdp: String },
    ViewerLeft { viewer_id: String },
    VersionRejected { min_version: String },
}

/// Returned by [`connect_host_signal`] when the backend rejects this build
/// as too old (`FW_MIN_HOST_VERSION`). Callers should stop retrying and
/// surface this to the user instead of looping forever against a backend
/// that will keep saying no.
#[derive(Debug)]
pub struct VersionRejected {
    pub min_version: String,
}

impl std::fmt::Display for VersionRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "このバージョンは古いためbackendに拒否されました（必要バージョン: {}以上）。新しいframewire.exeをダウンロードしてください。",
            self.min_version
        )
    }
}

impl std::error::Error for VersionRejected {}

/// Connects outbound to the backend over WebSocket and handles per-viewer
/// offer/answer exchange for the rest of the stream's lifetime. The host
/// never opens an inbound port in this mode.
///
/// Reconnects with backoff if the connection drops, reusing the same room
/// code. The first connection attempt is awaited here so setup errors
/// (e.g. backend unreachable) surface immediately to the caller.
pub async fn start_mesh_publisher(
    backend_ws_base: String,
    ice_servers: Vec<RTCIceServer>,
    bitrate_bps: u32,
) -> anyhow::Result<(FrameSender, String)> {
    let state = new_app_state(ice_servers, bitrate_bps).await;

    let (room_code, outbound, ws_rx) = connect_host_signal(&backend_ws_base, None).await?;
    println!("[transport] connected to backend signaling (room={room_code})");

    let task_state = state.clone();
    let task_room_code = room_code.clone();
    tokio::spawn(async move {
        run_signal_connection(&task_state, ws_rx, outbound).await;
        println!("[transport] signaling connection to backend dropped, reconnecting...");
        reconnect_loop(&backend_ws_base, task_room_code, &task_state).await;
    });

    Ok((FrameSender { state }, room_code))
}

/// Connects to the backend's `/v1/host-signal` and reads the initial
/// `room_created` response. Reports this build's version so the backend can
/// reject known-broken old builds before a room is even created (see
/// `VersionRejected`).
async fn connect_host_signal(
    backend_ws_base: &str,
    requested_code: Option<&str>,
) -> anyhow::Result<(String, OutboundHandle, WsSource)> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let version = env!("CARGO_PKG_VERSION");
    let url = match requested_code {
        Some(code) => {
            format!("{backend_ws_base}/v1/host-signal?room_code={code}&client_version={version}")
        }
        None => format!("{backend_ws_base}/v1/host-signal?client_version={version}"),
    };
    let request = url
        .into_client_request()
        .map_err(|e| anyhow::anyhow!("backend接続URLの構築に失敗: {e}"))?;
    let (ws_stream, _resp) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| anyhow::anyhow!("backendへの接続に失敗: {e}"))?;
    let (tx, mut rx) = ws_stream.split();

    let first = rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("backendからの初回応答がありません"))?
        .map_err(|e| anyhow::anyhow!("backendからの初回応答の受信に失敗: {e}"))?;
    let WsMessage::Text(text) = first else {
        anyhow::bail!("backendからの初回応答がテキストではありません");
    };
    let inbound: HostInbound = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("room_created応答のパースに失敗: {e} raw={text}"))?;
    let room_code = match inbound {
        HostInbound::RoomCreated { room_code } => room_code,
        HostInbound::VersionRejected { min_version } => {
            return Err(anyhow::Error::new(VersionRejected { min_version }));
        }
        _ => anyhow::bail!("backendからの初回応答がroom_createdではありません: {text}"),
    };

    Ok((room_code, Arc::new(Mutex::new(tx)), rx))
}

/// Runs one WebSocket connection's read loop until it disconnects.
async fn run_signal_connection(state: &Arc<AppState>, mut ws_rx: WsSource, outbound: OutboundHandle) {
    while let Some(msg) = ws_rx.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[transport] signaling receive error: {e:#}");
                break;
            }
        };
        let WsMessage::Text(text) = msg else {
            continue;
        };
        handle_signal_message(state, &outbound, &text).await;
    }
}

async fn handle_signal_message(state: &Arc<AppState>, outbound: &OutboundHandle, text: &str) {
    let inbound: HostInbound = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[transport] failed to parse signaling message: {e} raw={text}");
            return;
        }
    };
    match inbound {
        HostInbound::RoomCreated { .. } => {}
        HostInbound::VersionRejected { min_version } => {
            eprintln!(
                "[transport] unexpected version_rejected on an established connection (min_version={min_version})"
            );
        }
        HostInbound::Offer { viewer_id, sdp } => {
            let offer = match RTCSessionDescription::offer(sdp) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("[transport] failed to parse offer from viewer {viewer_id}: {e}");
                    return;
                }
            };
            match create_viewer_connection(state, viewer_id.clone(), offer).await {
                Ok(answer) => {
                    let msg = serde_json::json!({
                        "type": "answer",
                        "viewer_id": viewer_id,
                        "sdp": answer.sdp,
                    })
                    .to_string();
                    let mut tx = outbound.lock().await;
                    if let Err(e) = tx.send(WsMessage::Text(msg.into())).await {
                        eprintln!("[transport] failed to send answer: {e}");
                    }
                }
                Err(e) => eprintln!("[transport] failed to set up connection for viewer {viewer_id}: {e:#}"),
            }
        }
        HostInbound::ViewerLeft { viewer_id } => {
            remove_viewer(state, &viewer_id).await;
        }
    }
}

async fn reconnect_loop(backend_ws_base: &str, room_code: String, state: &Arc<AppState>) {
    let mut backoff = Duration::from_secs(2);
    loop {
        match connect_host_signal(backend_ws_base, Some(&room_code)).await {
            Ok((confirmed_code, outbound, ws_rx)) => {
                if confirmed_code != room_code {
                    eprintln!(
                        "[transport] room code changed on reconnect: {confirmed_code} != {room_code}"
                    );
                }
                println!("[transport] reconnected to backend (room={confirmed_code})");
                backoff = Duration::from_secs(2);
                run_signal_connection(state, ws_rx, outbound).await;
                println!("[transport] signaling connection to backend dropped, reconnecting...");
            }
            Err(e) if e.downcast_ref::<VersionRejected>().is_some() => {
                eprintln!("[transport] {e}; giving up (no point retrying)");
                return;
            }
            Err(e) => {
                eprintln!(
                    "[transport] reconnect to backend failed: {e:#} (retrying in {}s)",
                    backoff.as_secs()
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}
