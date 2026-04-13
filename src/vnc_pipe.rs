//! Bidirectional pipe between VNC TCP connection and WebRTC DataChannel.

use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::signaling::IceServerConfig;
use crate::webrtc_peer;

/// Buffered ICE candidate received before session is ready.
struct PendingIce {
    candidate: Option<serde_json::Value>,
    sdp_mid: Option<String>,
    sdp_mline_index: Option<u16>,
}

/// Bridge orchestrates the VNC <-> WebRTC DataChannel pipe.
#[derive(Clone)]
pub struct Bridge {
    vnc_addr: String,
    ice_servers: Vec<IceServerConfig>,
    session: Arc<Mutex<Option<ActiveSession>>>,
    pending_ice: Arc<Mutex<Vec<PendingIce>>>,
}

struct ActiveSession {
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    _pipe_handles: Vec<tokio::task::JoinHandle<()>>,
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
        // Close previous session
        {
            let mut guard = self.session.lock().await;
            if let Some(old) = guard.take() {
                info!("Closing previous WebRTC session");
                let _ = old.pc.close().await;
                for h in old._pipe_handles {
                    h.abort();
                }
            }
        }

        let (answer_sdp, peer_session) =
            webrtc_peer::handle_offer(offer_sdp, &self.ice_servers).await?;

        let vnc_addr = self.vnc_addr.clone();
        let pc = peer_session.pc.clone();
        let dc_tx = peer_session.dc_tx;
        let mut dc_rx = peer_session.dc_rx;
        let dc_ready = peer_session.dc_ready;

        // Spawn the pipe tasks — VNC TCP connects only after DataChannel is open
        let handles = spawn_pipe(vnc_addr, dc_tx, &mut dc_rx, dc_ready).await;

        {
            let mut guard = self.session.lock().await;
            *guard = Some(ActiveSession {
                pc: pc.clone(),
                _pipe_handles: handles,
            });
        }

        // Flush any ICE candidates that arrived before session was ready
        let pending: Vec<PendingIce> = {
            let mut buf = self.pending_ice.lock().await;
            buf.drain(..).collect()
        };
        if !pending.is_empty() {
            info!(count = pending.len(), "Flushing buffered ICE candidates");
            for p in pending {
                if let Err(e) = webrtc_peer::add_ice_candidate(&pc, p.candidate, p.sdp_mid, p.sdp_mline_index).await {
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
        if let Some(ref session) = *guard {
            webrtc_peer::add_ice_candidate(&session.pc, candidate, sdp_mid, sdp_mline_index)
                .await?;
        } else {
            info!("Buffering ICE candidate (session not ready yet)");
            drop(guard);
            let mut buf = self.pending_ice.lock().await;
            buf.push(PendingIce { candidate, sdp_mid, sdp_mline_index });
        }
        Ok(())
    }
}

/// Spawn a task that waits for DataChannel to open, then connects to VNC
/// and pipes data bidirectionally.
///
/// Returns task handles so they can be aborted when the session ends.
async fn spawn_pipe(
    vnc_addr: String,
    dc_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    dc_rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    dc_ready: Arc<tokio::sync::Notify>,
) -> Vec<tokio::task::JoinHandle<()>> {
    // Take ownership of dc_rx for the spawned task
    let mut dc_rx_owned = {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(tx);
        std::mem::replace(dc_rx, rx)
    };

    let pipe_handle = tokio::spawn(async move {
        info!("Waiting for DataChannel to open before connecting VNC...");
        dc_ready.notified().await;
        info!("DataChannel open, connecting to VNC at {vnc_addr}");

        let tcp = match TcpStream::connect(&vnc_addr).await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to connect to VNC at {vnc_addr}: {e}");
                return;
            }
        };

        info!(addr = %vnc_addr, "Connected to VNC server");

        let (mut tcp_read, tcp_write) = tcp.into_split();
        let tcp_write = Arc::new(Mutex::new(tcp_write));

        // VNC -> DataChannel
        let vnc_to_dc = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                match tcp_read.read(&mut buf).await {
                    Ok(0) => {
                        info!("VNC connection closed");
                        break;
                    }
                    Ok(n) => {
                        debug!(bytes = n, "VNC -> DataChannel");
                        if dc_tx.send(buf[..n].to_vec()).await.is_err() {
                            debug!("DataChannel sender dropped");
                            break;
                        }
                    }
                    Err(e) => {
                        error!("VNC read error: {e}");
                        break;
                    }
                }
            }
        });

        // DataChannel -> VNC
        let dc_to_vnc = tokio::spawn(async move {
            while let Some(data) = dc_rx_owned.recv().await {
                debug!(bytes = data.len(), "DataChannel -> VNC");
                let mut writer = tcp_write.lock().await;
                if let Err(e) = writer.write_all(&data).await {
                    error!("VNC write error: {e}");
                    break;
                }
            }
            info!("DataChannel -> VNC pipe ended");
        });

        let _ = tokio::join!(vnc_to_dc, dc_to_vnc);
    });

    vec![pipe_handle]
}
