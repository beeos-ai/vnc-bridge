# vnc-bridge

VNC TCP <-> WebRTC DataChannel bridge with MQTT signaling.

Bridges a local VNC/RFB server to a remote browser client over WebRTC DataChannel, using MQTT (EMQX) for signaling. Designed for NAT traversal scenarios where the VNC host has no public IP.

## Usage

```bash
vnc-bridge \
  --vnc 127.0.0.1:5900 \
  --mqtt wss://mqtt.beeos.ai/mqtt \
  --token <MQTT_TOKEN> \
  --topic devices/<INSTANCE_ID> \
  --ice-servers '[{"urls":["stun:stun.l.google.com:19302"]}]'
```

### Environment variables

All CLI flags can also be set via environment variables:

| Flag | Env var | Default |
|---|---|---|
| `--vnc` | `VNC_ADDR` | `127.0.0.1:5900` |
| `--mqtt` | `MQTT_BROKER_URL` | (required) |
| `--token` | `MQTT_TOKEN` | `""` |
| `--topic` | `DEVICE_TOPIC` | (required) |
| `--ice-servers` | `ICE_SERVERS` | `[]` |

## Install (self-hosted)

```bash
curl -fsSL https://raw.githubusercontent.com/beeos-ai/vnc-bridge/main/scripts/install.sh | bash -s -- \
  --mqtt wss://mqtt.beeos.ai/mqtt \
  --token <TOKEN> \
  --topic devices/<ID>
```

The installer downloads the correct binary for your OS/arch, installs it to `/usr/local/bin`, and sets up systemd (Linux) or launchd (macOS) for auto-start.

## Build from source

```bash
cargo build --release
```

### Cross-compile for Linux (from macOS)

```bash
make build-linux
```

## Architecture

```
Browser (noVNC + WebRTC)
    │
    │ DataChannel (P2P / TURN relay)
    │
    ▼
vnc-bridge ──TCP──▶ VNC Server (localhost:5900)
    │
    │ MQTT signaling (offer/answer/ICE)
    │
    ▼
EMQX Broker
```

## License

Private — BeeOS internal component.
