# jeha

Opinionated light automation daemon for [Zigbee2MQTT](https://www.zigbee2mqtt.io/). Handles circadian lighting, motion triggers, night mode, and scene management. Everything else stays in Z2M.

## Why

Home Assistant's light automation breaks in predictable ways: devices vanish when renamed, Flux updates lights that are off, motion sensors fail silently, config reloads are all-or-nothing. jeha replaces all of that with a focused Rust daemon that talks directly to Z2M over MQTT.

## Principles

- **IEEE addresses as identity.** Device names change. IEEE addresses don't. Config references hardware addresses; friendly names resolve at runtime from Z2M.
- **Z2M groups first.** One MQTT message per group instead of per-bulb. Less Zigbee traffic.
- **No database.** All state derives from config + Z2M retained messages + current time. Restart at any point and converge in seconds.
- **Never update lights that are off.** Circadian only pushes to rooms with lights ON.
- **External change detection.** If someone activates a Z2M scene or uses a remote, jeha detects the state change and pauses circadian automatically.
- **AI-native.** MCP server as the primary interface. No web UI. Talk to your lights through Claude.

## Quick start

```sh
cargo build --release
cp config.example.toml config.toml
# Edit config.toml with your rooms, groups, and sensors
./target/release/jeha run
```

## Configuration

TOML. See `config.example.toml` for a full example.

Minimal working config:

```toml
schema_version = 1

[rooms.kitchen]
z2m_group = "Kitchen"
```

That gives you circadian lighting with sensible defaults (cosine curve, 06:00-23:00, 2700K-4000K-2200K).

Override per room:

```toml
[rooms.kitchen.circadian]
peak_temp_k = 5500
peak_brightness = 254
```

## CLI

```
jeha run [--config path]              # Start daemon
jeha run --mqtt-host 10.0.0.5        # Override MQTT host
jeha validate [--config path]         # Validate config
jeha schema                           # Export JSON Schema
jeha init [--mqtt host:port]          # Auto-generate config from Z2M
```

## Environment variables

| Variable | Description |
|---|---|
| `JEHA_MQTT_HOST` | MQTT broker host |
| `JEHA_MQTT_PORT` | MQTT broker port |
| `JEHA_MQTT_TOPIC` | Z2M base topic (default: `zigbee2mqtt`) |
| `JEHA_MCP_BIND` | MCP server bind address (default: `127.0.0.1:8420`) |

CLI arguments take precedence over env vars, which take precedence over config file values.

## MCP tools

jeha exposes tools via MCP (Streamable HTTP on port 8420):

| Tool | What it does |
|---|---|
| `get_rooms` | List all rooms with light state, circadian status, occupancy |
| `get_room` | Detailed state for one room |
| `light_on` | Turn on with optional brightness/color_temp/override TTL |
| `light_off` | Turn off |
| `set_scene` | Predefined scenes: bright, relax, movie, energize, nightlight |
| `list_z2m_scenes` | List Z2M scenes available for a room |
| `recall_z2m_scene` | Activate a Z2M scene (pauses circadian) |
| `pause_circadian` | Stop circadian adjustments indefinitely |
| `resume_circadian` | Resume circadian |
| `snooze_circadian` | Pause circadian for N hours, auto-resume |
| `set_night_mode` | Toggle night mode |
| `get_circadian_status` | Current targets for all rooms |
| `get_system_status` | MQTT/Z2M connection, uptime, device counts |

## Circadian curve

Cosine interpolation between three points: wake (warm), midday (cool), sleep (warmest). 30-second transitions between updates make changes imperceptible. Configurable per room.

## Resilience

| Scenario | Behavior |
|---|---|
| Device renamed in Z2M | Auto-updates from `bridge/devices` (IEEE is stable) |
| Device comes online | Pushes correct circadian state immediately (3s transition) |
| Z2M scene activated externally | Detects state change, pauses circadian |
| MQTT disconnects | Auto-reconnect, re-push all rooms |
| jeha restarts | Stateless rebuild from config + Z2M retained messages |
| Bad config on reload | Rejects, keeps old config running |

## Architecture

```
Claude (MCP client)
    | Streamable HTTP (:8420)
    v
+---------------------------+
|       jeha daemon         |
|                           |
|  Config -> State (ArcSwap)|
|            |              |
|  MQTT <-> Circadian       |
|  Client   Automation      |
|            |              |
|        MCP Server         |
+---------------------------+
    | MQTT
    v
Zigbee2MQTT -> Zigbee devices
```

## License

MIT
