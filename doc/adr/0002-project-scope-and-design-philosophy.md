# 2. Project scope and design philosophy

Date: 2026-03-21

## Status

Accepted

## Context

JEHA is a light automation daemon for Zigbee2MQTT. Home automation software tends to grow in scope until it becomes a general-purpose platform (e.g. Home Assistant), accumulating complexity, resource usage, and configuration burden along the way. We need clear guardrails to prevent that trajectory and keep JEHA focused, fast, and simple.

## Decision

JEHA follows three core design principles:

### 1. Narrow scope — light automation only

JEHA automates lights and nothing else. It does not manage thermostats, locks, media players, or other device types. Features are only added if they directly serve light control (e.g. motion sensors as triggers, remotes as inputs). This keeps the codebase small and auditable.

### 2. Sensible defaults — batteries included

Inspired by Django's "batteries included" philosophy, JEHA ships with opinionated defaults that work well out of the box. Circadian curves, transition times, brightness ramps, external change detection tolerances, and override TTLs all have reasonable values. A minimal config (just rooms and lights) should produce good behavior without tuning. Advanced users can override defaults, but they shouldn't have to.

### 3. Performance above all else

JEHA must be fast and have a minimal resource footprint. It runs as a single static binary with no database — state is derived from config, MQTT retained messages, and current time. Lock-free reads via `ArcSwap`, lightweight async tasks via Tokio, and a small memory footprint are non-negotiable. The daemon should run comfortably on the most constrained hardware alongside Zigbee2MQTT.

## Consequences

- New feature requests that fall outside light automation will be declined, keeping maintenance burden low but limiting JEHA's appeal as a general-purpose tool.
- Strong defaults reduce initial setup time, but opinionated choices may not suit every deployment. Override knobs must remain available.
- The performance-first stance favors Rust-native solutions over external dependencies, which increases development effort but eliminates runtime overhead and keeps the deployment footprint minimal (single static binary, no interpreter, no database).
