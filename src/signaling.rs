//! MQTT signaling for WebRTC offer/answer/ICE exchange.
//!
//! Topic layout (identical to device-agent):
//!   Subscribe: `{topic}/signaling/request`
//!   Publish:   `{topic}/signaling/response`

use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde::{Deserialize, Serialize};
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

pub async fn run(mqtt_url: String, token: String, topic: String, bridge: Bridge) -> Result<()> {
    // rumqttc parse_url supports mqtt://, mqtts://, ws://, wss://
    // Append client_id as query parameter
    let url_with_id = format!(
        "{mqtt_url}{}client_id=vnc-bridge-{}",
        if mqtt_url.contains('?') { "&" } else { "?" },
        std::process::id()
    );

    let mut opts = MqttOptions::parse_url(&url_with_id)
        .map_err(|e| anyhow::anyhow!("Invalid MQTT URL: {e}"))?;

    opts.set_keep_alive(Duration::from_secs(30));

    if !token.is_empty() {
        opts.set_credentials("vnc-bridge", &token);
    }

    let (client, mut eventloop) = AsyncClient::new(opts, 64);

    let request_topic = format!("{topic}/signaling/request");
    let response_topic = format!("{topic}/signaling/response");

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
                error!("MQTT error: {e}, reconnecting in 3s...");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }

    Ok(())
}
