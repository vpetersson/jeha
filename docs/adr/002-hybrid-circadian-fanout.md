# ADR-002: Hybrid Group+Individual Circadian Fan-out

## Status

Accepted

## Context

When a Z2M group contains devices with mixed calibration (e.g., 4 CT-only ceiling lights + 1 RGBW table lamp with +15 mired offset), the circadian engine needs to send different color_temp values to different devices. The original implementation used an all-or-nothing approach: if ANY device in the group had non-neutral calibration, ALL devices received individual MQTT publishes instead of a single group command.

This caused visible flicker in rooms like the kitchen, where 4 identical ceiling lights (kitchen_0 through kitchen_3) received their commands staggered by 100-500ms, starting their transitions at different times.

This violated ADR-001 Decision 3 ("Prefer Z2M groups over individual light commands").

## Decision

Use a hybrid group+individual publish strategy when fan-out is needed:

1. **Send the group command** with base (uncalibrated) brightness and color_temp values. This ensures all neutral-calibration devices transition in perfect sync via a single Zigbee multicast.

2. **Send individual corrections** only to devices with non-neutral calibration. These devices briefly see the base value before the correction arrives, but with a 30s transition time on a single lamp this is imperceptible.

For the kitchen example, this reduces 5 individual publishes to 1 group + 1 individual (Kitchen Table only).

## Refinement: Capability-aware per-device corrections

The original hybrid approach only sent per-device corrections to devices with non-neutral **calibration**. This missed a second source of value mismatch: groups with mixed **capabilities** (e.g., GU10 spotlights with color_temp range 153-454 mired alongside regular bulbs with range 150-500 mired). The group-level clamped value (intersection of all ranges) could be wrong for individual devices, and the double-command (group + correction) caused oscillation at the Zigbee level.

The refined approach:

1. **Detect mixed groups** — check for both calibration differences AND capability differences (different light types or color_temp ranges) via `group_has_mixed_capabilities`.
2. **Group publish as crude estimate** — send brightness + intersection-clamped color_temp to the group for synchronized transitions.
3. **Per-device corrections when value differs** — for each CT-capable member, compute its ideal color_temp (per-device clamping + calibration). Only send a per-device correction if this differs from the group-level value. This eliminates unnecessary MQTT traffic for devices that are already correct.

## Consequences

- Neutral-calibration devices in uniform groups still use a single group publish (no change)
- Mixed-capability groups now get per-device color_temp corrections, not just mixed-calibration groups
- Per-device corrections are skipped when the device's ideal value matches the group value (minimal MQTT traffic)
- The per-device fallback path is preserved for rooms with no Z2M group (explicit `lights` list only)
