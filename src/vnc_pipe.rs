//! Bidirectional pipe: VNC TCP ↔ WebRTC DataChannel.
//!
//! Bridge receives WebRTC offers via MQTT signaling, creates a PeerConnection,
//! then pipes bytes between the VNC server (localhost TCP) and the browser
//! (DataChannel). Only one active session at a time; new offers replace old ones.

use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::signaling::IceServerConfig;
use crate::webrtc_peer::{self, PeerSession};

#[derive(Clone)]
pub struct Bridge {
    vnc_addr: String,
    ice_servers: Vec<IceServerConfig>,
    session: Arc<Mutex<Option<ActiveSession>>>,
    pending_ice: Arc<Mutex<Vec<PendingIce>>>,
}

struct ActiveSession {
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    pipe_handles: Vec<tokio::task::JoinHandle<()>>,
    created_at: std::time::Instant,
}

struct PendingIce {
    candidate: Option<serde_json::Value>,
    sdp_mid: Option<String>,
    sdp_mline_index: Option<u16>,
}

impl Bridge {
    pub fn new(vnc_addr: String, ice_servers: Vec<IceServerConfig>) -> Self {
        Self {
            vnc_addr,
            ice_servers,
            session: Arc::new(Mutex::new(None)),
            pending_ice: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn handle_offer(&self, offer_sdp: &str) -> Result<String> {
        // Tear down previous session if stale
        {
            let mut guard = self.session.lock().await;
            if let Some(ref old) = *guard {
                let age = old.created_at.elapsed();
                let state = old.pc.connection_state();
                use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
                if age < std::time::Duration::from_secs(10)
                    && matches!(
                        state,
                        RTCPeerConnectionState::New
                            | RTCPeerConnectionState::Connecting
                            | RTCPeerConnectionState::Connected
                    )
                {
                    info!(age_ms = age.as_millis(), ?state, "Ignoring duplicate offer");
                    return Err(anyhow::anyhow!("duplicate offer ignored"));
                }
            }
            if let Some(old) = guard.take() {
                info!("Closing previous session");
                let _ = old.pc.close().await;
                for h in old.pipe_handles {
                    h.abort();
                }
            }
        }

        let (answer_sdp, session) =
            webrtc_peer::handle_offer(offer_sdp, &self.ice_servers).await?;

        let pc = session.pc.clone();
        let handles = spawn_vnc_pipe(self.vnc_addr.clone(), session);

        // Store active session
        {
            let mut guard = self.session.lock().await;
            *guard = Some(ActiveSession {
                pc: pc.clone(),
                pipe_handles: handles,
                created_at: std::time::Instant::now(),
            });
        }

        // Flush any ICE candidates that arrived before the session was ready
        let pending: Vec<PendingIce> = self.pending_ice.lock().await.drain(..).collect();
        if !pending.is_empty() {
            info!(count = pending.len(), "Flushing buffered ICE candidates");
            for p in pending {
                if let Err(e) = webrtc_peer::add_ice_candidate(
                    &pc,
                    p.candidate,
                    p.sdp_mid,
                    p.sdp_mline_index,
                )
                .await
                {
                    warn!("Failed to add buffered ICE candidate: {e:#}");
                }
            }
        }

        Ok(answer_sdp)
    }

    pub async fn handle_ice_candidate(
        &self,
        candidate: Option<serde_json::Value>,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> Result<()> {
        let guard = self.session.lock().await;
        if let Some(ref s) = *guard {
            webrtc_peer::add_ice_candidate(&s.pc, candidate, sdp_mid, sdp_mline_index).await?;
        } else {
            drop(guard);
            self.pending_ice.lock().await.push(PendingIce {
                candidate,
                sdp_mid,
                sdp_mline_index,
            });
            debug!("Buffered ICE candidate (no active session)");
        }
        Ok(())
    }
}

/// Spawn the VNC↔DC pipe. Waits for DC ready, connects TCP, then runs two
/// forwarding loops until either side closes.
fn spawn_vnc_pipe(vnc_addr: String, session: PeerSession) -> Vec<tokio::task::JoinHandle<()>> {
    let PeerSession {
        dc_rx,
        dc_tx,
        dc_ready,
        ..
    } = session;

    let handle = tokio::spawn(async move {
        dc_ready.notified().await;
        info!("DataChannel ready, connecting VNC at {vnc_addr}");

        let tcp = match TcpStream::connect(&vnc_addr).await {
            Ok(s) => s,
            Err(e) => {
                error!("VNC connect failed ({vnc_addr}): {e}");
                return;
            }
        };
        info!("VNC connected (TCP)");

        let (mut tcp_read, tcp_write) = tcp.into_split();
        let tcp_write = Arc::new(Mutex::new(tcp_write));
        let mut dc_rx = dc_rx;

        // VNC→Browser: TCP read → dc_tx channel → (webrtc_peer) → DC.send
        let vnc_to_dc = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                match tcp_read.read(&mut buf).await {
                    Ok(0) => {
                        info!("VNC closed");
                        break;
                    }
                    Ok(n) => {
                        if dc_tx.send(buf[..n].to_vec()).await.is_err() {
                            debug!("DC tx dropped");
                            break;
                        }
                    }
                    Err(e) => {
                        error!("VNC read: {e}");
                        break;
                    }
                }
            }
        });

        // Browser→VNC: dc_rx channel ← (webrtc_peer) ← DC.on_message → TCP write
        let dc_to_vnc = tokio::spawn(async move {
            while let Some(data) = dc_rx.recv().await {
                let mut w = tcp_write.lock().await;
                if let Err(e) = w.write_all(&data).await {
                    error!("VNC write: {e}");
                    break;
                }
            }
        });

        let _ = tokio::join!(vnc_to_dc, dc_to_vnc);
        info!("VNC pipe closed");
    });

    vec![handle]
}
