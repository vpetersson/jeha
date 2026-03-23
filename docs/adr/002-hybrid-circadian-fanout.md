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

## Consequences

- Neutral-calibration devices in mixed groups now transition in perfect sync (no flicker)
- MQTT traffic reduced from N individual publishes to 1 group + M calibrated-device publishes (where M << N)
- Calibrated devices experience a brief moment (~100ms) with base values before correction arrives — imperceptible during slow circadian transitions
- The group publish path (`push_circadian_group`) remains unchanged for uniform groups
- The per-device fallback path is preserved for rooms with no Z2M group (explicit `lights` list only)
