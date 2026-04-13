//! WebRTC PeerConnection management.
//!
//! Acts as Answerer: receives offer, creates answer with full ICE candidates,
//! provides DataChannel for VNC byte piping.

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::signaling::IceServerConfig;

/// Active peer session with channels for bidirectional byte I/O.
pub struct PeerSession {
    pub pc: Arc<RTCPeerConnection>,
    /// Receives raw bytes from the browser via DataChannel
    pub dc_rx: mpsc::Receiver<Vec<u8>>,
    /// Sender for VNC->browser direction (forward to DataChannel via spawned task)
    pub dc_tx: mpsc::Sender<Vec<u8>>,
    /// Fires once when the DataChannel is open and ready.
    /// VNC TCP connection MUST wait for this before connecting.
    pub dc_ready: Arc<Notify>,
}

pub async fn handle_offer(
    offer_sdp: &str,
    ice_servers: &[IceServerConfig],
) -> Result<(String, PeerSession)> {
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs()?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)?;

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    let rtc_ice_servers: Vec<RTCIceServer> = ice_servers
        .iter()
        .map(|s| RTCIceServer {
            urls: s.urls.clone(),
            username: s.username.clone().unwrap_or_default(),
            credential: s.credential.clone().unwrap_or_default(),
            ..Default::default()
        })
        .collect();

    let config = RTCConfiguration {
        ice_servers: rtc_ice_servers,
        ..Default::default()
    };

    let pc = Arc::new(api.new_peer_connection(config).await?);

    let gathering_done = Arc::new(Notify::new());
    let gd = gathering_done.clone();

    pc.on_ice_gathering_state_change(Box::new(move |state| {
        info!("ICE gathering state: {state}");
        if state == webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState::Complete {
            gd.notify_one();
        }
        Box::pin(async {})
    }));

    pc.on_peer_connection_state_change(Box::new(move |state| {
        info!("PeerConnection state: {state}");
        if state == webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Failed {
            warn!("PeerConnection failed");
        }
        Box::pin(async {})
    }));

    // browser_tx/browser_rx: bytes FROM browser DataChannel -> VNC TCP
    let (browser_tx, browser_rx) = mpsc::channel::<Vec<u8>>(256);
    // vnc_tx/vnc_rx: bytes FROM VNC TCP -> browser DataChannel
    let (vnc_tx, mut vnc_rx) = mpsc::channel::<Vec<u8>>(256);

    let dc_ready = Arc::new(Notify::new());
    let dc_ready_signal = dc_ready.clone();

    let dc_holder: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let dc_holder_for_fwd = dc_holder.clone();

    pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        info!("DataChannel created: {}", dc.label());

        let tx = browser_tx.clone();
        let holder = dc_holder.clone();
        let ready = dc_ready_signal.clone();

        Box::pin(async move {
            let dc_for_open = dc.clone();
            let ready_for_open = ready.clone();

            dc.on_open(Box::new(move || {
                info!("DataChannel open — signaling VNC connect");
                {
                    let holder = holder.clone();
                    let dc_ref = dc_for_open.clone();
                    tokio::spawn(async move {
                        let mut guard = holder.lock().await;
                        *guard = Some(dc_ref);
                    });
                }
                ready_for_open.notify_waiters();
                Box::pin(async {})
            }));

            dc.on_message(Box::new(move |msg: DataChannelMessage| {
                let tx = tx.clone();
                Box::pin(async move {
                    if tx.send(msg.data.to_vec()).await.is_err() {
                        debug!("DataChannel rx dropped");
                    }
                })
            }));
        })
    }));

    let offer = RTCSessionDescription::offer(offer_sdp.to_string())?;
    pc.set_remote_description(offer)
        .await
        .context("Failed to set remote description")?;

    let answer = pc.create_answer(None).await.context("Failed to create answer")?;
    pc.set_local_description(answer)
        .await
        .context("Failed to set local description")?;

    tokio::select! {
        _ = gathering_done.notified() => {
            info!("ICE gathering complete");
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
            warn!("ICE gathering timed out after 10s, returning partial candidates");
        }
    }

    let local_desc = pc
        .local_description()
        .await
        .context("No local description after answer")?;
    let answer_sdp = local_desc.sdp;

    let candidate_count = answer_sdp.matches("a=candidate:").count();
    info!("Answer SDP has {candidate_count} ICE candidates");

    // Forward VNC data to DataChannel.
    // dc_holder is set on DataChannel open, which must happen before VNC connects.
    let fwd_dc_ready = dc_ready.clone();
    tokio::spawn(async move {
        fwd_dc_ready.notified().await;
        while let Some(data) = vnc_rx.recv().await {
            let guard = dc_holder_for_fwd.lock().await;
            if let Some(ref dc) = *guard {
                let buf = bytes::Bytes::copy_from_slice(&data);
                if let Err(e) = dc.send(&buf).await {
                    debug!("DataChannel send error: {e}");
                    break;
                }
            } else {
                warn!("VNC data received but DataChannel not yet in holder");
            }
        }
    });

    let session = PeerSession {
        pc: pc.clone(),
        dc_rx: browser_rx,
        dc_tx: vnc_tx,
        dc_ready,
    };

    Ok((answer_sdp, session))
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

    let init = RTCIceCandidateInit {
        candidate: candidate_str,
        sdp_mid: sdp_mid.or(Some("0".into())),
        sdp_mline_index: sdp_mline_index.or(Some(0)),
        ..Default::default()
    };

    pc.add_ice_candidate(init)
        .await
        .context("Failed to add ICE candidate")?;
    debug!("Added remote ICE candidate");
    Ok(())
}
