#!/bin/bash
# BeeOS vnc-bridge installer for self-hosted PCs.
# Downloads the vnc-bridge binary and sets up systemd service (Linux)
# or launchd plist (macOS).
#
# Usage: curl -fsSL https://beeos.ai/install-vnc-bridge | bash -s -- \
#          --mqtt wss://mqtt.beeos.ai/mqtt \
#          --token <MQTT_TOKEN> \
#          --topic devices/<INSTANCE_ID> \
#          --ice-servers '[{"urls":["stun:stun.l.google.com:19302"]}]'

set -euo pipefail

INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
VNC_ADDR="${VNC_ADDR:-127.0.0.1:5900}"
MQTT_URL=""
MQTT_TOKEN=""
DEVICE_TOPIC=""
ICE_SERVERS="[]"
VERSION="${VERSION:-latest}"

usage() {
  cat <<EOF
Usage: $0 --mqtt <url> --token <token> --topic <topic> [options]

Required:
  --mqtt <url>        MQTT broker URL (wss://mqtt.beeos.ai/mqtt)
  --token <token>     MQTT authentication token
  --topic <topic>     Device topic (devices/<instance-id>)

Optional:
  --vnc <addr>        VNC server address (default: 127.0.0.1:5900)
  --ice-servers <json> ICE server config (default: [])
  --version <ver>     Binary version (default: latest)
  --install-dir <dir> Install directory (default: /usr/local/bin)
EOF
  exit 1
}

while [[ $# -gt 0 ]]; do
  case $1 in
    --mqtt) MQTT_URL="$2"; shift 2;;
    --token) MQTT_TOKEN="$2"; shift 2;;
    --topic) DEVICE_TOPIC="$2"; shift 2;;
    --vnc) VNC_ADDR="$2"; shift 2;;
    --ice-servers) ICE_SERVERS="$2"; shift 2;;
    --version) VERSION="$2"; shift 2;;
    --install-dir) INSTALL_DIR="$2"; shift 2;;
    *) echo "Unknown option: $1"; usage;;
  esac
done

[[ -z "$MQTT_URL" ]] && { echo "Error: --mqtt is required"; usage; }
[[ -z "$MQTT_TOKEN" ]] && { echo "Error: --token is required"; usage; }
[[ -z "$DEVICE_TOPIC" ]] && { echo "Error: --topic is required"; usage; }

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) ARCH="x86_64";;
  aarch64|arm64) ARCH="aarch64";;
  *) echo "Unsupported architecture: $ARCH"; exit 1;;
esac

echo "Installing vnc-bridge for $OS/$ARCH..."

DOWNLOAD_URL="https://github.com/beeos-ai/beeos/releases/download/vnc-bridge-v${VERSION}/vnc-bridge-${OS}-${ARCH}"
if [[ "$VERSION" == "latest" ]]; then
  DOWNLOAD_URL="https://github.com/beeos-ai/beeos/releases/latest/download/vnc-bridge-${OS}-${ARCH}"
fi

curl -fsSL "$DOWNLOAD_URL" -o /tmp/vnc-bridge
chmod +x /tmp/vnc-bridge
sudo mv /tmp/vnc-bridge "${INSTALL_DIR}/vnc-bridge"

echo "Binary installed to ${INSTALL_DIR}/vnc-bridge"

# Create config file
CONFIG_DIR="${HOME}/.config/beeos"
mkdir -p "$CONFIG_DIR"
cat > "${CONFIG_DIR}/vnc-bridge.env" <<ENV
VNC_ADDR=${VNC_ADDR}
MQTT_BROKER_URL=${MQTT_URL}
MQTT_TOKEN=${MQTT_TOKEN}
DEVICE_TOPIC=${DEVICE_TOPIC}
ICE_SERVERS=${ICE_SERVERS}
ENV
chmod 600 "${CONFIG_DIR}/vnc-bridge.env"
echo "Config saved to ${CONFIG_DIR}/vnc-bridge.env"

if [[ "$OS" == "linux" ]] && command -v systemctl &>/dev/null; then
  cat > /tmp/vnc-bridge.service <<UNIT
[Unit]
Description=BeeOS VNC Bridge
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=${CONFIG_DIR}/vnc-bridge.env
ExecStart=${INSTALL_DIR}/vnc-bridge \\
  --vnc \${VNC_ADDR} \\
  --mqtt \${MQTT_BROKER_URL} \\
  --token \${MQTT_TOKEN} \\
  --topic \${DEVICE_TOPIC} \\
  --ice-servers \${ICE_SERVERS}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

  sudo mv /tmp/vnc-bridge.service /etc/systemd/system/vnc-bridge.service
  sudo systemctl daemon-reload
  sudo systemctl enable vnc-bridge
  sudo systemctl start vnc-bridge

  echo "systemd service installed and started"
  echo "  Status: sudo systemctl status vnc-bridge"
  echo "  Logs:   sudo journalctl -u vnc-bridge -f"

elif [[ "$OS" == "darwin" ]]; then
  PLIST_DIR="${HOME}/Library/LaunchAgents"
  mkdir -p "$PLIST_DIR"
  cat > "${PLIST_DIR}/ai.beeos.vnc-bridge.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>ai.beeos.vnc-bridge</string>
  <key>ProgramArguments</key>
  <array>
    <string>${INSTALL_DIR}/vnc-bridge</string>
    <string>--vnc</string>
    <string>${VNC_ADDR}</string>
    <string>--mqtt</string>
    <string>${MQTT_URL}</string>
    <string>--token</string>
    <string>${MQTT_TOKEN}</string>
    <string>--topic</string>
    <string>${DEVICE_TOPIC}</string>
    <string>--ice-servers</string>
    <string>${ICE_SERVERS}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>/tmp/vnc-bridge.log</string>
  <key>StandardErrorPath</key>
  <string>/tmp/vnc-bridge.err</string>
</dict>
</plist>
PLIST

  launchctl load "${PLIST_DIR}/ai.beeos.vnc-bridge.plist"
  echo "launchd agent installed and started"
  echo "  Logs: tail -f /tmp/vnc-bridge.log"
else
  echo ""
  echo "Auto-start not configured. Run manually:"
  echo "  vnc-bridge --vnc $VNC_ADDR --mqtt $MQTT_URL --token \$MQTT_TOKEN --topic $DEVICE_TOPIC --ice-servers '$ICE_SERVERS'"
fi

echo ""
echo "vnc-bridge installation complete!"
