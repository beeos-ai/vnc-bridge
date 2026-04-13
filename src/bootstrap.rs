//! Agent Gateway bootstrap — Ed25519-authenticated HTTP for MQTT credentials.
//!
//! Implements the same signing protocol as device-agent and beeos-claw:
//!   Signed message: "METHOD|PATH|timestamp|nonce"
//!   Headers: X-Agent-Public-Key, X-Agent-Signature, X-Agent-Timestamp, X-Agent-Nonce

use anyhow::{Context, Result};
use base64::Engine;
use ed25519_dalek::SigningKey;
use serde::Deserialize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

use crate::signaling::IceServerConfig;

const BOOTSTRAP_PATH: &str = "/api/v1/device/bootstrap";

#[derive(Debug, Clone)]
pub struct AgentKeyPair {
    pub signing_key: SigningKey,
    pub public_key_bytes: [u8; 32],
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct BootstrapResponse {
    #[serde(default)]
    pub mqtt_url: String,
    #[serde(default)]
    pub mqtt_token: String,
    #[serde(default)]
    pub device_topic: String,
    #[serde(default)]
    pub ice_servers: Vec<IceServerConfig>,
    #[serde(default)]
    pub expires_at: i64,
}

/// Load Ed25519 keypair from a JSON file produced by beeos-openclaw's entrypoint.
///
/// Supports two formats:
///   1. JSON with `publicKey` (base64/base64url 32-byte raw) + `privateKey` (base64 PKCS#8 DER or 32-byte seed)
///   2. Raw base64 private key seed (32 bytes)
pub fn load_agent_keys(path: &Path) -> Result<AgentKeyPair> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read key file: {}", path.display()))?;
    let raw = raw.trim();

    let b64 = base64::engine::general_purpose::STANDARD;
    let b64url = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    if raw.starts_with('{') {
        #[derive(Deserialize)]
        struct KeyFile {
            #[serde(rename = "publicKey")]
            public_key: String,
            #[serde(rename = "privateKey")]
            private_key: String,
        }
        let kf: KeyFile =
            serde_json::from_str(raw).context("parse key JSON")?;

        let priv_bytes = b64.decode(&kf.private_key)
            .or_else(|_| b64url.decode(&kf.private_key))
            .context("decode privateKey")?;

        let seed = extract_ed25519_seed(&priv_bytes)?;
        let signing_key = SigningKey::from_bytes(&seed);

        let pub_bytes = b64.decode(&kf.public_key)
            .or_else(|_| b64url.decode(&kf.public_key))
            .context("decode publicKey")?;
        let public_key_bytes: [u8; 32] = pub_bytes
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("publicKey is {} bytes, expected 32", v.len()))?;

        Ok(AgentKeyPair { signing_key, public_key_bytes })
    } else {
        let seed_bytes = b64.decode(raw)
            .or_else(|_| b64url.decode(raw))
            .context("decode raw key")?;
        let seed = extract_ed25519_seed(&seed_bytes)?;
        let signing_key = SigningKey::from_bytes(&seed);
        let public_key_bytes = signing_key.verifying_key().to_bytes();
        Ok(AgentKeyPair { signing_key, public_key_bytes })
    }
}

/// Extract 32-byte Ed25519 seed from either raw bytes or PKCS#8 DER.
fn extract_ed25519_seed(bytes: &[u8]) -> Result<[u8; 32]> {
    if bytes.len() == 32 {
        let mut seed = [0u8; 32];
        seed.copy_from_slice(bytes);
        return Ok(seed);
    }
    // PKCS#8 DER for Ed25519: 48 bytes total, seed at offset 16
    if bytes.len() == 48 {
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&bytes[16..48]);
        return Ok(seed);
    }
    // Try parsing via PKCS#8
    use ed25519_dalek::pkcs8::DecodePrivateKey;
    let sk = SigningKey::from_pkcs8_der(bytes)
        .map_err(|e| anyhow::anyhow!("parse PKCS#8 DER: {e}"))?;
    Ok(*sk.as_bytes())
}

fn sign_request(method: &str, path: &str, keys: &AgentKeyPair) -> Vec<(String, String)> {
    use ed25519_dalek::Signer;
    let b64 = base64::engine::general_purpose::STANDARD;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let nonce = uuid::Uuid::new_v4().to_string();
    let message = format!("{}|{}|{}|{}", method.to_uppercase(), path, timestamp, nonce);

    let signature = keys.signing_key.sign(message.as_bytes());

    vec![
        ("X-Agent-Public-Key".into(), b64.encode(keys.public_key_bytes)),
        ("X-Agent-Signature".into(), b64.encode(signature.to_bytes())),
        ("X-Agent-Timestamp".into(), timestamp),
        ("X-Agent-Nonce".into(), nonce),
    ]
}

/// Fetch MQTT + ICE credentials from Agent Gateway bootstrap endpoint.
pub async fn fetch_bootstrap(
    gateway_url: &str,
    keys: &AgentKeyPair,
) -> Result<BootstrapResponse> {
    let url = format!("{}{}", gateway_url.trim_end_matches('/'), BOOTSTRAP_PATH);
    let headers = sign_request("GET", BOOTSTRAP_PATH, keys);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let mut req = client.get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = req.send().await.context("bootstrap HTTP request")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("bootstrap failed (HTTP {}): {}", status, &body[..body.len().min(200)]);
    }

    let data: BootstrapResponse = resp.json().await.context("parse bootstrap JSON")?;
    info!(
        mqtt_url = %data.mqtt_url,
        device_topic = %data.device_topic,
        ice_count = data.ice_servers.len(),
        "Bootstrap succeeded"
    );
    Ok(data)
}

/// Retry bootstrap with exponential backoff until credentials are obtained.
pub async fn bootstrap_with_retry(
    gateway_url: &str,
    keys: &AgentKeyPair,
    max_attempts: usize,
) -> Result<BootstrapResponse> {
    let delays = [5, 10, 15, 30, 30, 60];
    for attempt in 0..max_attempts {
        match fetch_bootstrap(gateway_url, keys).await {
            Ok(resp) if !resp.mqtt_token.is_empty() && !resp.device_topic.is_empty() => {
                return Ok(resp);
            }
            Ok(_) => {
                warn!(attempt = attempt + 1, "Bootstrap returned empty token/topic, retrying...");
            }
            Err(e) => {
                warn!(attempt = attempt + 1, error = %e, "Bootstrap failed, retrying...");
            }
        }
        let delay = delays.get(attempt).copied().unwrap_or(60);
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
    }
    anyhow::bail!("bootstrap retries exhausted after {max_attempts} attempts")
}
