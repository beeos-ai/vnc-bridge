//! WebRTC PeerConnection management.
//!
//! Both browser (offerer) and vnc-bridge (answerer) create a negotiated
//! DataChannel with `id=0`. This avoids relying on the `on_data_channel`
//! callback path in webrtc-rs, which has known issues with `on_message`
//! delivery on the answerer side for in-band negotiated channels.

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::signaling::IceServerConfig;

pub struct PeerSession {
    pub pc: Arc<RTCPeerConnection>,
    /// Bytes FROM browser (DC → VNC direction). Caller reads this.
    pub dc_rx: mpsc::Receiver<Vec<u8>>,
    /// Bytes TO browser (VNC → DC direction). Caller writes here.
    pub dc_tx: mpsc::Sender<Vec<u8>>,
    /// Fires when the DataChannel is open. VNC connection should wait for this.
    pub dc_ready: Arc<Notify>,
}

pub async fn handle_offer(
    offer_sdp: &str,
    ice_servers: &[IceServerConfig],
) -> Result<(String, PeerSession)> {
    let mut me = MediaEngine::default();
    me.register_default_codecs()?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut me)?;

    // Force IPv4-only ICE gathering. Our deploy targets (ECS Fargate awsvpc ENI,
    // EKS pods) run on IPv4-only VPCs, and the configured STUN/TURN servers
    // (stun.l.google.com, turn.beeos.ai) have no AAAA records. Letting webrtc-rs
    // try IPv6 triggers webrtc-rs/webrtc#774: when IPv6 host resolution fails it
    // aborts the IPv4 path as well, leaving the agent with zero srflx/relay
    // candidates and ICE permanently failed. Pinning to Udp4 sidesteps the bug.
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_network_types(vec![NetworkType::Udp4]);
    info!("SettingEngine configured with network_types=[Udp4]");

    let api = APIBuilder::new()
        .with_media_engine(me)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build();

    // Belt-and-suspenders: even if SettingEngine.set_network_types is somehow
    // bypassed inside webrtc-rs 0.17.1, ice_transport_policy=Relay forces the
    // browser↔agent path through TURN (turn.beeos.ai), which has no IPv6
    // resolution issue. This is the supported escape hatch documented in
    // RFC 8829 §5.5 (`iceTransportPolicy: "relay"`).
    let config = RTCConfiguration {
        ice_servers: ice_servers
            .iter()
            .map(|s| RTCIceServer {
                urls: s.urls.clone(),
                username: s.username.clone().unwrap_or_default(),
                credential: s.credential.clone().unwrap_or_default(),
                ..Default::default()
            })
            .collect(),
        ice_transport_policy: RTCIceTransportPolicy::Relay,
        ..Default::default()
    };
    info!(
        ice_servers_count = ice_servers.len(),
        "RTCConfiguration: ice_transport_policy=Relay (TURN-only)"
    );

    let pc = Arc::new(api.new_peer_connection(config).await?);

    // --- ICE gathering completion signal ---
    let gathering_done = Arc::new(Notify::new());
    let gd = gathering_done.clone();
    pc.on_ice_gathering_state_change(Box::new(move |state| {
        if state == webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState::Complete {
            gd.notify_one();
        }
        Box::pin(async {})
    }));

    pc.on_peer_connection_state_change(Box::new(|state| {
        info!("PeerConnection state: {state}");
        Box::pin(async {})
    }));

    pc.on_ice_candidate(Box::new(|c| {
        if let Some(cand) = c {
            info!(candidate = %cand.to_string(), "Local ICE candidate gathered");
        } else {
            info!("Local ICE gathering done (null candidate)");
        }
        Box::pin(async {})
    }));

    // --- Create negotiated DataChannel (id=0) ---
    // Both browser and vnc-bridge create a DC with negotiated=true, id=0.
    // This ensures both sides share the same SCTP stream and avoids
    // webrtc-rs on_data_channel callback issues with on_message delivery.
    let dc = pc
        .create_data_channel(
            "vnc",
            Some(RTCDataChannelInit {
                ordered: Some(true),
                protocol: Some("binary".to_string()),
                negotiated: Some(0),
                ..Default::default()
            }),
        )
        .await
        .context("create negotiated data channel")?;

    info!(label = %dc.label(), id = dc.id(), "Negotiated DataChannel created");

    // --- Bidirectional byte channels ---
    let (browser_tx, browser_rx) = mpsc::channel::<Vec<u8>>(256);
    let (vnc_tx, mut vnc_rx) = mpsc::channel::<Vec<u8>>(256);

    let dc_ready = Arc::new(Notify::new());
    let dc_ready_clone = dc_ready.clone();

    // VNC→Browser forwarder (spawned on DC open)
    let dc_for_fwd = dc.clone();
    dc.on_open(Box::new(move || {
        info!("DataChannel open");
        dc_ready_clone.notify_waiters();

        let dc = dc_for_fwd.clone();
        tokio::spawn(async move {
            let mut count: u64 = 0;
            let mut total: u64 = 0;
            while let Some(data) = vnc_rx.recv().await {
                count += 1;
                total += data.len() as u64;
                if count <= 10 || count % 500 == 0 {
                    debug!(len = data.len(), total, count, "DC.send (VNC→Browser)");
                }
                let buf = bytes::Bytes::copy_from_slice(&data);
                if let Err(e) = dc.send(&buf).await {
                    warn!("DC send error: {e}");
                    break;
                }
            }
            debug!("VNC→DC forwarder ended");
        });

        Box::pin(async {})
    }));

    // Browser→VNC: DC.on_message → browser_tx channel
    dc.on_message(Box::new(move |msg: DataChannelMessage| {
        let tx = browser_tx.clone();
        Box::pin(async move {
            let _ = tx.send(msg.data.to_vec()).await;
        })
    }));

    info!("on_message handler registered on negotiated DC");

    // --- SDP exchange ---
    let offer = RTCSessionDescription::offer(offer_sdp.to_string())?;
    pc.set_remote_description(offer)
        .await
        .context("set remote description")?;

    let answer = pc.create_answer(None).await.context("create answer")?;
    pc.set_local_description(answer)
        .await
        .context("set local description")?;

    // Wait for ICE gathering (with timeout)
    tokio::select! {
        _ = gathering_done.notified() => {}
        _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
            warn!("ICE gathering timed out, returning partial candidates");
        }
    }

    let local_desc = pc
        .local_description()
        .await
        .context("no local description after answer")?;

    info!(
        candidates = local_desc.sdp.matches("a=candidate:").count(),
        "Answer SDP ready"
    );

    Ok((
        local_desc.sdp,
        PeerSession {
            pc: pc.clone(),
            dc_rx: browser_rx,
            dc_tx: vnc_tx,
            dc_ready,
        },
    ))
}

pub async fn add_ice_candidate(
    pc: &RTCPeerConnection,
    candidate: Option<serde_json::Value>,
    sdp_mid: Option<String>,
    sdp_mline_index: Option<u16>,
) -> Result<()> {
    let candidate_str = match candidate {
        Some(serde_json::Value::String(s)) => s,
        Some(serde_json::Value::Object(obj)) => obj
            .get("candidate")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => return Ok(()),
    };

    if candidate_str.is_empty() {
        return Ok(());
    }

    pc.add_ice_candidate(RTCIceCandidateInit {
        candidate: candidate_str,
        sdp_mid: sdp_mid.or(Some("0".into())),
        sdp_mline_index: sdp_mline_index.or(Some(0)),
        ..Default::default()
    })
    .await
    .context("add ICE candidate")?;

    debug!("Added remote ICE candidate");
    Ok(())
}
