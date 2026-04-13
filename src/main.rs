mod bootstrap;
mod signaling;
mod webrtc_peer;
mod vnc_pipe;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tracing::{info, warn};

use crate::bootstrap::AgentKeyPair;
use crate::signaling::{MqttCredentials, TokenRefreshFn};

#[derive(Parser, Debug)]
#[command(name = "vnc-bridge", about = "VNC TCP <-> WebRTC DataChannel bridge")]
struct Args {
    /// VNC server address (host:port)
    #[arg(long, env = "VNC_ADDR", default_value = "127.0.0.1:5900")]
    vnc: String,

    /// Agent Gateway URL for dynamic bootstrap (preferred mode)
    #[arg(long, env = "AGENT_GATEWAY_URL")]
    gateway: Option<String>,

    /// Ed25519 key file path (JSON format from beeos-openclaw entrypoint)
    #[arg(long, env = "AGENT_KEY_FILE")]
    key_file: Option<String>,

    // --- Legacy static mode (fallback when --gateway is not set) ---

    /// MQTT broker URL — static mode only (use --gateway for dynamic bootstrap)
    #[arg(long, env = "MQTT_BROKER_URL")]
    mqtt: Option<String>,

    /// MQTT authentication token — static mode only
    #[arg(long, env = "MQTT_TOKEN", default_value = "")]
    token: String,

    /// Device topic prefix — static mode only
    #[arg(long, env = "DEVICE_TOPIC")]
    topic: Option<String>,

    /// ICE servers as JSON array — static mode only
    #[arg(long, env = "ICE_SERVERS", default_value = "[]")]
    ice_servers: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls CryptoProvider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    if args.gateway.is_some() && args.key_file.is_some() {
        run_bootstrap_mode(args).await
    } else if args.mqtt.is_some() {
        run_static_mode(args).await
    } else {
        anyhow::bail!(
            "Either --gateway + --key-file (bootstrap mode) or --mqtt + --topic (static mode) is required"
        );
    }
}

/// Bootstrap mode: fetch MQTT credentials from Agent Gateway, auto-refresh on reconnect.
async fn run_bootstrap_mode(args: Args) -> Result<()> {
    let gateway_url = args.gateway.unwrap();
    let key_path = args.key_file.unwrap();

    info!(gateway = %gateway_url, key_file = %key_path, "Starting in bootstrap mode");

    let keys = bootstrap::load_agent_keys(std::path::Path::new(&key_path))?;
    info!("Ed25519 key loaded");

    let resp = bootstrap::bootstrap_with_retry(&gateway_url, &keys, 10).await?;

    let ice_servers = resp.ice_servers.clone();
    let bridge = vnc_pipe::Bridge::new(args.vnc.clone(), ice_servers);

    let initial_creds = MqttCredentials {
        mqtt_url: resp.mqtt_url,
        token: resp.mqtt_token,
        topic: resp.device_topic,
    };

    let refresh_fn = build_refresh_fn(gateway_url, keys);

    signaling::run(initial_creds, bridge, Some(refresh_fn)).await
}

/// Static mode: use CLI-provided MQTT credentials (legacy, no auto-refresh).
async fn run_static_mode(args: Args) -> Result<()> {
    let mqtt_url = args.mqtt.unwrap();
    let topic = args.topic.ok_or_else(|| anyhow::anyhow!("--topic is required in static mode"))?;

    warn!("Starting in static mode (no token refresh). Prefer --gateway + --key-file for production.");

    let ice_servers: Vec<signaling::IceServerConfig> = serde_json::from_str(&args.ice_servers)
        .unwrap_or_else(|e| {
            warn!("Failed to parse ICE_SERVERS JSON, using empty list: {e}");
            Vec::new()
        });

    info!(
        vnc = %args.vnc,
        mqtt = %mqtt_url,
        topic = %topic,
        ice_server_count = ice_servers.len(),
        "Starting vnc-bridge (static mode)"
    );

    let bridge = vnc_pipe::Bridge::new(args.vnc, ice_servers);

    let creds = MqttCredentials {
        mqtt_url,
        token: args.token,
        topic,
    };

    signaling::run(creds, bridge, None).await
}

fn build_refresh_fn(gateway_url: String, keys: AgentKeyPair) -> TokenRefreshFn {
    Arc::new(move || {
        let gw = gateway_url.clone();
        let k = keys.clone();
        Box::pin(async move {
            match bootstrap::fetch_bootstrap(&gw, &k).await {
                Ok(resp) if !resp.mqtt_token.is_empty() => {
                    info!("MQTT token refreshed via bootstrap");
                    Some(MqttCredentials {
                        mqtt_url: resp.mqtt_url,
                        token: resp.mqtt_token,
                        topic: resp.device_topic,
                    })
                }
                Ok(_) => {
                    warn!("Bootstrap returned empty token");
                    None
                }
                Err(e) => {
                    warn!(error = %e, "Bootstrap refresh failed");
                    None
                }
            }
        })
    })
}
