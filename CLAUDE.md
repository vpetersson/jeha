# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

jeha is a light automation daemon for Zigbee2MQTT written in Rust (edition 2024). It runs as a single static binary with no database — state is derived from config + Z2M retained MQTT messages + current time.

## Build & Test Commands

```bash
cargo build                          # Debug build
cargo build --release                # Release build (stripped, LTO)
cargo test --all                     # Run all tests
cargo test <test_name>               # Run a single test (e.g. cargo test test_cosine_morning_ramp)
cargo fmt --all --check              # Check formatting
cargo clippy --all-targets -- -D warnings  # Lint (CI treats warnings as errors)
```

Rust toolchain is at `~/.cargo/bin` (may not be in default PATH).

CLI subcommands: `run`, `validate`, `migrate`, `schema`, `init`.

## Architecture

Event-driven daemon with concurrent engines coordinating through shared state and a broadcast event bus:

- **Config** (TOML) loads into `AppConfig`, validated at startup
- **State** (`Arc<ArcSwap<SystemState>>`) provides lock-free reads; writes go through `StateManager` actor via mpsc channel
- **MQTT** (`rumqttc`) connects to Z2M, subscribes to bridge/devices, bridge/groups, bridge/state, device states, and availability topics
- **EventBus** (`tokio::broadcast`) distributes events (MotionDetected, LightStateChanged, DeviceAvailabilityChanged, etc.) to all engines
- **CircadianEngine** — periodic task that computes brightness/color_temp targets using cosine interpolation and pushes to lights via MQTT
- **AutomationEngine** — reacts to motion/remote events, evaluates conditions, executes actions (lights on/off with delays)
- **REST API Server** — axum-based REST API on port 8420; endpoints for room control, circadian management, scenes, and system status
- **ConfigSync** — auto-discovers new Z2M groups and appends them to config file
- **LightsOutTask** / **NightModeScheduler** — time-based tasks running on 30s check intervals

All engines run as independent `tokio::spawn` tasks. Failures in one engine don't crash the daemon.

## Key Design Patterns

- **IEEE addresses as device identity** — never use Z2M friendly names (they can change); resolve at runtime via device_map
- **Z2M groups preferred for publishing** — one MQTT message per group instead of per-bulb
- **External change detection** — compares incoming light state to last intended values (with drift tolerances: brightness ±15, color_temp ±25 mired); pauses circadian on external changes
- **Manual override TTL** — `override_ttl_mins` on light_on/set_scene auto-expires back to circadian control
- **Circadian math** in `src/circadian/curve.rs`: cosine easing `(1 - cos(t*π))/2`, Kelvin↔Mired conversion `mired = 1_000_000 / kelvin`

## Module Layout

- `src/cli/run.rs` — daemon startup orchestration (wires everything together)
- `src/config/types.rs` — all config structs (`AppConfig`, `RoomConfig`, `AutomationConfig`, etc.)
- `src/config/validate.rs` — multi-stage config validation
- `src/state/mod.rs` — `SystemState`, `RoomState`, `StateManager` actor, `Z2mDeviceInfo`/`Z2mGroupInfo`
- `src/event.rs` — `Event` enum and `EventBus`
- `src/mqtt/z2m.rs` — Z2M message handlers (devices, groups, state, availability)
- `src/mqtt/publish.rs` — `Publisher` for sending commands to Z2M
- `src/api/handlers.rs` — REST API handler functions and shared AppState
- `src/schedule.rs` — `TimeOfDay`, `Schedule`, time matching logic

## Testing

~18 unit tests across: circadian curve math, config parsing/validation, trigger matching, IEEE validation, schedule matching. Tests are co-located in their respective modules (`#[cfg(test)]`).
