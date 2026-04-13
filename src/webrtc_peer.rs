//! WebRTC PeerConnection management.
//!
//! Answerer role: receives browser offer, returns answer with ICE candidates,
//! and exposes a bidirectional byte channel for VNC piping.
//!
//! Design: DataChannel I/O is fully encapsulated inside the `on_data_channel`
//! callback. No DC reference leaks out — the forwarding goroutine lives inside
//! the callback closure, eliminating race conditions between DC readiness and
//! data flow.

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
    let api = APIBuilder::new()
        .with_media_engine(me)
        .with_interceptor_registry(registry)
        .build();

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
        ..Default::default()
    };

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

    // --- Bidirectional byte channels ---
    //
    //  Browser→VNC:  DC.on_message → browser_tx ~~~> browser_rx  (caller reads)
    //  VNC→Browser:  (caller writes) vnc_tx ~~~> vnc_rx → DC.send  (spawned inside on_data_channel)
    //
    let (browser_tx, browser_rx) = mpsc::channel::<Vec<u8>>(256);
    let (vnc_tx, vnc_rx) = mpsc::channel::<Vec<u8>>(256);

    let dc_ready = Arc::new(Notify::new());
    let dc_ready_for_cb = dc_ready.clone();

    // on_data_channel is FnMut but we only expect one DC per session.
    // Wrap move-once values in Arc<std::sync::Mutex<Option<T>>> for take().
    let browser_tx_slot = Arc::new(std::sync::Mutex::new(Some(browser_tx)));
    let vnc_rx_slot = Arc::new(std::sync::Mutex::new(Some(vnc_rx)));
    let ready_slot = Arc::new(std::sync::Mutex::new(Some(dc_ready_for_cb)));

    pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        let tx = match browser_tx_slot.lock().unwrap().take() {
            Some(v) => v,
            None => {
                warn!("Duplicate DataChannel ignored");
                return Box::pin(async {});
            }
        };
        let rx = vnc_rx_slot.lock().unwrap().take().unwrap();
        let ready = ready_slot.lock().unwrap().take().unwrap();

        info!(label = %dc.label(), "DataChannel negotiated");

        Box::pin(async move {
            let dc_for_fwd = dc.clone();

            // on_open is Fn (not FnOnce), so wrap move-once values for take().
            let rx_cell = Arc::new(std::sync::Mutex::new(Some(rx)));
            let ready_cell = Arc::new(std::sync::Mutex::new(Some(ready)));

            dc.on_open(Box::new(move || {
                if let Some(ready) = ready_cell.lock().unwrap().take() {
                    info!("DataChannel open");
                    ready.notify_waiters();
                }

                if let Some(rx) = rx_cell.lock().unwrap().take() {
                    let dc = dc_for_fwd.clone();
                    let mut rx = rx;
                    tokio::spawn(async move {
                        while let Some(data) = rx.recv().await {
                            let buf = bytes::Bytes::copy_from_slice(&data);
                            if let Err(e) = dc.send(&buf).await {
                                warn!("DC send error: {e}");
                                break;
                            }
                        }
                        debug!("VNC→DC forwarder ended");
                    });
                }

                Box::pin(async {})
            }));

            dc.on_message(Box::new(move |msg: DataChannelMessage| {
                let tx = tx.clone();
                Box::pin(async move {
                    let _ = tx.send(msg.data.to_vec()).await;
                })
            }));
        })
    }));

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
