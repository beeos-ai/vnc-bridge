//! MQTT signaling for WebRTC offer/answer/ICE exchange.
//!
//! Topic layout (identical to device-agent):
//!   Subscribe: `{topic}/signaling/request`
//!   Publish:   `{topic}/signaling/response`

use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, Transport};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::vnc_pipe::Bridge;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SignalingMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    sdp: Option<String>,
    #[serde(default)]
    candidate: Option<serde_json::Value>,
    #[serde(default, rename = "sdpMid")]
    sdp_mid: Option<String>,
    #[serde(default, rename = "sdpMLineIndex")]
    sdp_mline_index: Option<u16>,
}

#[derive(Debug, Serialize)]
struct AnswerMessage {
    #[serde(rename = "type")]
    msg_type: String,
    sdp: String,
}

/// Credentials for MQTT connection, refreshable via bootstrap.
#[derive(Clone)]
pub struct MqttCredentials {
    pub mqtt_url: String,
    pub token: String,
    pub topic: String,
}

/// Token refresh callback: returns fresh credentials or None on failure.
pub type TokenRefreshFn =
    Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<MqttCredentials>> + Send>> + Send + Sync>;

/// Run the signaling loop with automatic reconnection and token refresh.
///
/// When `refresh_fn` is provided, MQTT errors trigger a token refresh before
/// reconnecting. This mirrors device-agent's MQTTManager behavior.
pub async fn run(
    initial_creds: MqttCredentials,
    bridge: Bridge,
    refresh_fn: Option<TokenRefreshFn>,
) -> Result<()> {
    let mut creds = initial_creds;
    let mut backoff = 1u64;

    loop {
        match run_session(&creds, &bridge).await {
            Ok(()) => {
                info!("Signaling session ended cleanly");
                return Ok(());
            }
            Err(e) => {
                error!("MQTT session error: {e:#}");
            }
        }

        info!("Reconnecting in {backoff}s...");
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);

        if let Some(ref refresh) = refresh_fn {
            info!("Refreshing MQTT credentials before reconnect...");
            match (refresh)().await {
                Some(new_creds) => {
                    info!(topic = %new_creds.topic, "Credentials refreshed");
                    creds = new_creds;
                    backoff = 1;
                }
                None => {
                    warn!("Token refresh failed, retrying with old credentials");
                }
            }
        }
    }
}

/// Run a single MQTT session until error or clean shutdown.
async fn run_session(creds: &MqttCredentials, bridge: &Bridge) -> Result<()> {
    let client_id = format!("vnc-bridge-{}", std::process::id());
    let mut opts = parse_mqtt_url(&creds.mqtt_url, &client_id)?;

    opts.set_keep_alive(Duration::from_secs(30));

    if !creds.token.is_empty() {
        opts.set_credentials("vnc-bridge", &creds.token);
    }

    let (client, mut eventloop) = AsyncClient::new(opts, 64);

    let request_topic = format!("{}/signaling/request", creds.topic);
    let response_topic = format!("{}/signaling/response", creds.topic);

    client
        .subscribe(&request_topic, QoS::AtLeastOnce)
        .await
        .context("Failed to subscribe to signaling topic")?;

    info!(topic = %request_topic, "Subscribed to signaling requests");

    let (offer_tx, mut offer_rx) = mpsc::channel::<SignalingMessage>(8);

    let response_topic_clone = response_topic.clone();
    let client_clone = client.clone();
    let bridge_clone = bridge.clone();

    tokio::spawn(async move {
        while let Some(msg) = offer_rx.recv().await {
            match msg.msg_type.as_str() {
                "offer" => {
                    let sdp = match msg.sdp {
                        Some(s) => s,
                        None => {
                            warn!("Offer missing SDP, ignoring");
                            continue;
                        }
                    };

                    info!("Processing WebRTC offer");
                    match bridge_clone.handle_offer(&sdp).await {
                        Ok(answer_sdp) => {
                            let answer = AnswerMessage {
                                msg_type: "answer".into(),
                                sdp: answer_sdp,
                            };
                            let payload = serde_json::to_vec(&answer)
                                .expect("AnswerMessage serialization cannot fail");
                            if let Err(e) = client_clone
                                .publish(&response_topic_clone, QoS::AtLeastOnce, false, payload)
                                .await
                            {
                                error!("Failed to publish answer: {e}");
                            } else {
                                info!("Published WebRTC answer");
                            }
                        }
                        Err(e) => warn!("Offer not processed: {e:#}"),
                    }
                }
                "ice" => {
                    debug!("Processing ICE candidate");
                    if let Err(e) = bridge_clone
                        .handle_ice_candidate(msg.candidate, msg.sdp_mid, msg.sdp_mline_index)
                        .await
                    {
                        warn!("Failed to handle ICE candidate: {e:#}");
                    }
                }
                other => {
                    warn!(msg_type = other, "Unknown signaling message type");
                }
            }
        }
    });

    info!("Waiting for signaling messages...");
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                if publish.topic == request_topic {
                    match serde_json::from_slice::<SignalingMessage>(&publish.payload) {
                        Ok(msg) => {
                            if offer_tx.send(msg).await.is_err() {
                                error!("Offer channel closed");
                                break;
                            }
                        }
                        Err(e) => warn!("Invalid signaling JSON: {e}"),
                    }
                }
            }
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                info!("MQTT connected");
            }
            Ok(_) => {}
            Err(e) => {
                error!("MQTT error: {e}");
                return Err(anyhow::anyhow!("MQTT connection error: {e}"));
            }
        }
    }

    Ok(())
}

/// Parse an MQTT broker URL into MqttOptions, handling WebSocket paths correctly.
///
/// rumqttc's `MqttOptions::parse_url` drops the URL path (e.g. `/mqtt`), which
/// breaks WebSocket connections. For Ws/Wss transport, rumqttc uses `broker_addr`
/// as the full WebSocket URL internally, so we pass the original URL as the host.
fn parse_mqtt_url(mqtt_url: &str, client_id: &str) -> Result<MqttOptions> {
    let parsed = url::Url::parse(mqtt_url)
        .with_context(|| format!("parse MQTT URL: {mqtt_url}"))?;

    let host = parsed.host_str()
        .ok_or_else(|| anyhow::anyhow!("MQTT URL missing host: {mqtt_url}"))?;
    let scheme = parsed.scheme();

    let (default_port, use_tls) = match scheme {
        "mqtt" | "tcp" => (1883, false),
        "mqtts" | "ssl" => (8883, true),
        "ws" => (8000, false),
        "wss" => (443, true),
        _ => anyhow::bail!("unsupported MQTT URL scheme: {scheme}"),
    };
    let port = parsed.port().unwrap_or(default_port);

    let is_ws = scheme == "ws" || scheme == "wss";

    if is_ws {
        // For WebSocket, broker_addr must be the full URL (rumqttc uses it for
        // both TCP connect via split_url and the WS upgrade request).
        let mut opts = MqttOptions::new(client_id, mqtt_url, port);
        if use_tls {
            opts.set_transport(Transport::Wss(rumqttc::TlsConfiguration::default()));
        } else {
            opts.set_transport(Transport::Ws);
        }
        Ok(opts)
    } else {
        let mut opts = MqttOptions::new(client_id, host, port);
        if use_tls {
            opts.set_transport(Transport::tls_with_default_config());
        }
        Ok(opts)
    }
}
