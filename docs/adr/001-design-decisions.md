# ADR-001: Design Decisions

**Status:** Accepted
**Date:** 2026-03-07

## Context

Home Assistant's light automation is fragile. Devices get "forgotten" when Zigbee2MQTT renames them. Motion sensors silently stop triggering. The Flux integration updates lights that are off, wastes Zigbee bandwidth, and produces jerky transitions. Config reloads are all-or-nothing. The YAML-based automation system is error-prone and hard to reason about.

jeha exists because we got tired of fixing the same problems. It is a focused, opinionated Rust daemon that handles **only** light automation — circadian lighting, motion triggers, night mode — by talking directly to Zigbee2MQTT over MQTT. Everything else stays in Z2M where it belongs.

This document records the design decisions we've made and, more importantly, *why* we made them. jeha is not a framework. It does not try to be flexible enough for every use case. It makes strong choices and sticks to them.

---

## Decision 1: Rust, not Python or Go

**We chose Rust.**

A lighting daemon runs 24/7 on modest hardware (often a Raspberry Pi). It processes MQTT messages at high frequency, holds shared state across concurrent tasks, and must never crash. Rust gives us:

- Zero-cost async with tokio — thousands of concurrent MQTT messages without threads
- Ownership model that prevents the leaked-subscription and dangling-callback bugs that plague Home Assistant's Python automation
- Compilation catches entire classes of bugs (wrong types, missing error handling, data races) before the code ever runs
- Single static binary — no runtime, no virtualenv, no dependency hell on the target machine

We are not optimizing for contributor onboarding speed. We are optimizing for correctness in a system that controls physical devices in people's homes.

## Decision 2: IEEE addresses as identity, always

**Device config uses IEEE addresses. Never friendly names.**

Z2M friendly names change. Users rename devices on a whim. HA integrations break when this happens because they bind to names. IEEE addresses (e.g., `0x00158d0004abcdef`) are burned into the hardware and never change.

jeha resolves IEEE to friendly name at runtime by subscribing to Z2M's `bridge/devices` retained topic. When a device is renamed in Z2M, jeha picks it up automatically on the next message. Zero config changes. Zero downtime.

Group names are the one exception — Z2M groups don't have IEEE addresses. We accept this trade-off because group names change far less frequently than device names, and jeha tracks group membership dynamically via `bridge/groups`.

## Decision 3: Z2M groups for room addressing

**Prefer Z2M groups over individual light commands.**

Sending `{"brightness": 200, "color_temp": 370}` to a Z2M group is one MQTT message. Z2M handles fan-out to group members at the Zigbee level, which is faster and produces less radio traffic than addressing each bulb individually.

jeha publishes circadian updates to the group topic. Individual light addressing is available via MCP for per-light control, but the default path is always the group.

This is an opinionated choice. It means rooms should have Z2M groups configured. We think this is the right trade-off: a few minutes of Z2M group setup saves constant Zigbee congestion.

## Decision 4: No database, no state file

**jeha is stateless. All runtime state is derived from config + Z2M + current time.**

On startup, jeha subscribes to retained MQTT topics, rebuilds its view of the world, computes circadian targets for the current time, and pushes correct state to all online lights. This takes milliseconds. There is no migration, no schema upgrade, no corruption risk, no "state got out of sync" debugging.

State that *does* exist in memory (room brightness, occupancy, manual override timers) uses `Arc<ArcSwap<SystemState>>` for lock-free reads and a `StateManager` actor with an mpsc channel for serialized writes. No `RwLock`, no deadlocks, no writer starvation.

The consequence: manual override TTLs and circadian snooze timers reset on restart. We consider this acceptable. A restart takes seconds, and the system immediately converges to the correct state. Persisting timers would add complexity for marginal benefit.

## Decision 5: Cosine interpolation, not linear

**Circadian curves use cosine easing.**

Home Assistant's Flux uses linear interpolation between color temperature waypoints. This produces visible jumps at transition boundaries — the light noticeably shifts when it enters or exits a ramp.

Cosine interpolation (`(1 - cos(t * pi)) / 2`) provides smooth acceleration and deceleration. The transition is imperceptible. Combined with 30-second MQTT transition hints (Z2M smooths between updates), the result is lighting that changes continuously without any visible steps.

The curve is defined by three points — start (wake), peak (midday), end (sleep) — with configurable ramp durations. No sunrise/sunset dependency. Works for any schedule, anywhere.

## Decision 6: Never update lights that are off

**Circadian updates only push to rooms where lights are ON.**

Flux's biggest sin is publishing color temperature updates to lights that are off. This wastes Zigbee bandwidth, wakes sleeping devices, and in some cases causes lights to briefly flash. It is the single most common complaint about Flux.

jeha tracks `lights_on` state per room. The circadian engine skips any room where lights are off. When lights turn on (via automation or MCP), the correct circadian values are applied at that moment.

## Decision 7: Manual overrides are temporary by default

**Manual light changes pause circadian with an optional TTL.**

When someone sets a light to a specific brightness or color temperature via MCP (e.g., "set the living room to movie mode"), circadian updates for that room are paused. Without a TTL, this pause is indefinite — the override persists until explicitly cleared.

With `override_ttl_mins`, the override auto-expires and circadian resumes. This prevents the common scenario where someone sets a light for a movie and it stays at 20% brightness for the next three days because nobody remembered to turn circadian back on.

Snooze works the same way for circadian pause — `snooze_circadian(room, hours)` pauses circadian for the specified duration and auto-resumes.

Both use `std::time::Instant`-based TTLs checked on each circadian tick. No background timers, no timer wheel complexity.

## Decision 8: TOML config, not YAML

**Configuration is TOML.**

YAML is a minefield:
- `on` and `off` silently become booleans (the "Norway problem")
- Duplicate keys silently overwrite each other
- Indentation errors produce valid but wrong config
- The spec is 86 pages long

TOML has none of these problems. Duplicate keys are parse errors. There's no boolean coercion. The format is simple enough that the entire spec fits in a few pages. It's the native config format in the Rust ecosystem.

Config is fully validated at load time through a three-stage pipeline: structural parsing (serde), semantic validation (cross-referencing room IDs, IEEE format, color temp ranges), and optional runtime validation against live Z2M data.

## Decision 9: Claude (Clawbot) is the primary interface

**jeha is designed to be controlled by an AI agent, not a human clicking buttons.**

There is no web UI. There is no mobile app. The primary interface is an MCP server that exposes tools for querying state, controlling lights, managing automations, and diagnosing problems.

Every MCP tool response includes a `description` field with a natural-language summary. Error messages name the available options ("Unknown room 'kichen'. Available rooms: kitchen, office, bathroom, ..."). Tool descriptions are written for an AI consumer — they explain not just what parameters exist, but when and why to use them.

Scenes (`bright`, `relax`, `movie`, `energize`, `nightlight`) are predefined with sensible values because an AI agent should be able to say "set the living room to movie mode" without needing to know that means brightness 80 and color temp 2200K.

This is a deliberate choice. A good AI interface *is* a good human interface — the human just talks to the AI instead of fiddling with sliders.

## Decision 10: Fail gracefully, never block

**Every failure mode has a defined behavior that isn't "crash".**

| Scenario | Behavior |
|---|---|
| Device comes online | Push correct circadian state immediately (3s transition) |
| Motion sensor offline | Skip motion triggers, log warning, continue |
| MQTT disconnects | Auto-reconnect with backoff, re-push all rooms on reconnect |
| Z2M restarts | Retained messages repopulate state, circadian re-pushed |
| Light unresponsive | Log warning, skip, move on |
| Bad config on reload | Reject entirely, keep running with old config |
| Automation panics | Caught at task boundary, automation disabled, system continues |

Each automation runs in its own tokio task. A panic in one cannot crash the daemon or affect other automations. The circadian engine runs independently of the automation engine. MQTT reconnection is handled by rumqttc with configurable backoff.

jeha is designed to run unattended for months. The failure mode for any component is degradation, not termination.

## Decision 11: Auto-discover capabilities, never declare them

**jeha never asks users to specify what a light supports.**

Z2M's `bridge/devices` topic includes an `exposes` array for every device that declares its capabilities: brightness, color_temp (with min/max mired range), color_xy, color_hs. jeha parses this on startup and on every `bridge/devices` update.

Mixed-capability rooms just work. When jeha sends `{"color_temp": 370, "brightness": 200}` to a Z2M group containing both color-temp and brightness-only bulbs, Z2M handles it correctly — each bulb applies only the fields it supports.

For individual light commands via MCP, jeha validates against known capabilities and returns a clear error if a light doesn't support the requested feature.

This means zero light-type configuration. Add a bulb to a Z2M group, and jeha automatically knows what it can do.

## Decision 12: One concern, done well

**jeha only does light automation. Everything else stays in Z2M.**

| Concern | Owner |
|---|---|
| Device pairing, OTA updates | Z2M |
| Groups, network topology | Z2M |
| Circadian lighting | **jeha** |
| Motion-triggered automations | **jeha** |
| Night mode | **jeha** |
| Scenes and manual control | **jeha** (via MCP) |

We will not add thermostat control, door lock management, media player integration, or any other non-lighting concern. jeha is not Home Assistant. It is a lighting daemon that does one thing and does it well.

This constraint is what makes the system reliable. Every line of code serves light automation. There are no generic entity abstractions, no plugin systems, no "but what if someone wants to..." escape hatches.

---

## Consequences

These decisions produce a system that is:

- **Fast to start** — config parse + MQTT connect + state rebuild in under a second
- **Impossible to misconfigure silently** — TOML parsing + semantic validation catches errors at load time
- **Resilient to Z2M changes** — IEEE identity + dynamic capability discovery + group membership tracking
- **Invisible in operation** — cosine curves + 30s transitions + skip-when-off = lighting that just works
- **AI-native** — MCP-first interface with rich descriptions and sensible defaults
- **Simple to deploy** — single binary, no database, no state file, no web server dependencies

The trade-offs are real: no web UI, no plugin system, no support for non-lighting devices, timer state lost on restart, Z2M groups required for optimal operation. We accept these trade-offs because they are the cost of a system that doesn't break.
