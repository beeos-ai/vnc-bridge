mod signaling;
mod webrtc_peer;
mod vnc_pipe;

use anyhow::Result;
use clap::Parser;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(name = "vnc-bridge", about = "VNC TCP <-> WebRTC DataChannel bridge")]
struct Args {
    /// VNC server address (host:port)
    #[arg(long, env = "VNC_ADDR", default_value = "127.0.0.1:5900")]
    vnc: String,

    /// MQTT broker URL (mqtt://, mqtts://, ws://, wss://)
    #[arg(long, env = "MQTT_BROKER_URL")]
    mqtt: String,

    /// MQTT authentication token (used as password)
    #[arg(long, env = "MQTT_TOKEN", default_value = "")]
    token: String,

    /// Device topic prefix (e.g. "devices/instance-123")
    #[arg(long, env = "DEVICE_TOPIC")]
    topic: String,

    /// ICE servers as JSON array (e.g. '[{"urls":["stun:stun.l.google.com:19302"]}]')
    #[arg(long, env = "ICE_SERVERS", default_value = "[]")]
    ice_servers: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let ice_servers: Vec<signaling::IceServerConfig> = serde_json::from_str(&args.ice_servers)
        .unwrap_or_else(|e| {
            warn!("Failed to parse ICE_SERVERS JSON, using empty list: {e}");
            Vec::new()
        });

    info!(
        vnc = %args.vnc,
        mqtt = %args.mqtt,
        topic = %args.topic,
        ice_server_count = ice_servers.len(),
        "Starting vnc-bridge"
    );

    let bridge = vnc_pipe::Bridge::new(args.vnc.clone(), ice_servers.clone());

    signaling::run(args.mqtt, args.token, args.topic, bridge).await
}
