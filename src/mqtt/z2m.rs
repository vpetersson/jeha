use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, info, trace};

use crate::config::types::AppConfig;
use crate::event::{Event, EventBus, Illuminance};
use crate::state::{
    RoomStateUpdate, SharedState, StateCommand, UpdateSource, Z2mDeviceInfo, Z2mGroupInfo,
    Z2mGroupMember, Z2mScene,
};

/// Per-room timestamp of the most recent external-change emission. Used to
/// deduplicate rapid back-to-back MQTT messages that all read a stale
/// `circadian_paused=false` snapshot before the StateManager actor applies
/// the pause from the first message.
static EXTERNAL_CHANGE_DEDUP: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const EXTERNAL_CHANGE_DEDUP_WINDOW: Duration = Duration::from_secs(2);

/// Last-known `occupancy` value per motion-sensor IEEE, so motion events are
/// published on transitions only. Keyed by IEEE (not friendly name) because
/// friendly names can be renamed in Z2M at any time.
static OCCUPANCY_STATE: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Stores `occupancy` as the current value for `ieee` and reports whether it
/// is a transition worth publishing. The first report seen for a sensor —
/// including the retained state replayed right after a restart — always
/// counts as a transition, since jeha has no prior value to compare against.
fn record_occupancy(cache: &Mutex<HashMap<String, bool>>, ieee: &str, occupancy: bool) -> bool {
    let mut cache = cache.lock().unwrap();
    match cache.insert(ieee.to_string(), occupancy) {
        Some(previous) => previous != occupancy,
        None => true,
    }
}

/// Last `action` seen per remote IEEE, with the time it was accepted. Z2M
/// keeps the most recent `action` in the device's state document, so every
/// later republish of that device (battery, linkquality, update availability)
/// carries the same action again and would be replayed as a fresh button
/// press.
static REMOTE_ACTION_STATE: LazyLock<Mutex<HashMap<String, (String, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// How long an identical action from the same remote is treated as a
/// republish rather than a new press. Deliberate repeat presses are hundreds
/// of milliseconds apart at the very fastest, while republish storms land in
/// the same millisecond, so this keeps real double-presses working.
const REMOTE_ACTION_DEDUP_WINDOW: Duration = Duration::from_millis(500);

/// Records `action` for `ieee` and reports whether it should be published as
/// a button press. Returns `false` for an identical action seen inside
/// [`REMOTE_ACTION_DEDUP_WINDOW`]. A different action always counts, so
/// genuine sequences (`on_press` then `on_hold`) are never suppressed.
fn record_remote_action(
    cache: &Mutex<HashMap<String, (String, Instant)>>,
    ieee: &str,
    action: &str,
    now: Instant,
) -> bool {
    let mut cache = cache.lock().unwrap();
    match cache.get(ieee) {
        Some((previous, seen_at))
            if previous == action && now.duration_since(*seen_at) < REMOTE_ACTION_DEDUP_WINDOW =>
        {
            false
        }
        _ => {
            cache.insert(ieee.to_string(), (action.to_string(), now));
            true
        }
    }
}

#[derive(Debug, Deserialize)]
struct Z2mDevice {
    ieee_address: String,
    friendly_name: String,
    supported: Option<bool>,
    #[serde(default)]
    definition: Option<Z2mDefinition>,
}

#[derive(Debug, Deserialize)]
struct Z2mDefinition {
    #[serde(default)]
    exposes: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Z2mGroup {
    id: u16,
    friendly_name: String,
    #[serde(default)]
    members: Vec<Z2mGroupMemberRaw>,
    #[serde(default)]
    scenes: Vec<Z2mSceneRaw>,
}

#[derive(Debug, Deserialize)]
struct Z2mGroupMemberRaw {
    ieee_address: String,
    #[serde(default)]
    endpoint: u8,
}

#[derive(Debug, Deserialize)]
struct Z2mSceneRaw {
    id: u16,
    name: String,
}

/// Precomputed room lookups derived from the (immutable) config, so the
/// per-message hot path avoids iterating all rooms for every MQTT publish.
pub struct RoomLookup {
    /// Z2M group friendly name -> room_id (rooms configured with z2m_group).
    group_to_room: HashMap<String, String>,
    /// Light IEEE -> room_id (rooms configured with explicit lights, no group).
    ieee_to_room: HashMap<String, String>,
    /// Motion sensor IEEE -> room_ids using that sensor.
    sensor_rooms: HashMap<String, Vec<String>>,
}

impl RoomLookup {
    pub fn from_config(config: &AppConfig) -> Self {
        let mut group_to_room = HashMap::new();
        let mut ieee_to_room = HashMap::new();
        let mut sensor_rooms: HashMap<String, Vec<String>> = HashMap::new();
        for (room_id, rc) in &config.rooms {
            if let Some(ref group) = rc.z2m_group {
                if let Some(prev) = group_to_room.insert(group.clone(), room_id.clone()) {
                    tracing::warn!(
                        "Z2M group '{}' is referenced by both room '{}' and room '{}'; \
                         group state will be attributed to '{}'",
                        group,
                        prev,
                        room_id,
                        room_id
                    );
                }
            } else {
                for ieee in &rc.lights {
                    if let Some(prev) = ieee_to_room.insert(ieee.clone(), room_id.clone()) {
                        tracing::warn!(
                            "Light {} is listed in both room '{}' and room '{}'; \
                             its state will be attributed to '{}'",
                            ieee,
                            prev,
                            room_id,
                            room_id
                        );
                    }
                }
            }
            if let Some(ref sensor) = rc.motion_sensor {
                sensor_rooms
                    .entry(sensor.clone())
                    .or_default()
                    .push(room_id.clone());
            }
        }
        Self {
            group_to_room,
            ieee_to_room,
            sensor_rooms,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_message(
    topic: &str,
    payload: &Bytes,
    base_topic: &str,
    state: &SharedState,
    state_tx: &mpsc::Sender<StateCommand>,
    event_bus: &EventBus,
    config: &AppConfig,
    lookup: &RoomLookup,
) -> Result<()> {
    let relative = topic
        .strip_prefix(base_topic)
        .and_then(|s| s.strip_prefix('/'));

    let Some(relative) = relative else {
        return Ok(());
    };

    match relative {
        "bridge/devices" => {
            handle_bridge_devices(payload, state, state_tx, event_bus).await?;
        }
        "bridge/groups" => {
            handle_bridge_groups(payload, state, state_tx, event_bus).await?;
        }
        "bridge/state" => {
            let text = std::str::from_utf8(payload)?;
            let online = text.contains("online");
            let _ = state_tx.send(StateCommand::SetZ2mOnline(online)).await;
            info!("Z2M bridge state: {}", text);
        }
        _ if relative.ends_with("/availability") => {
            let device_name = relative.strip_suffix("/availability").unwrap();
            handle_availability(device_name, payload, state_tx).await?;
        }
        _ => {
            handle_device_state(
                relative, payload, state, state_tx, event_bus, config, lookup,
            )
            .await?;
        }
    }

    Ok(())
}

async fn handle_bridge_devices(
    payload: &Bytes,
    state: &SharedState,
    state_tx: &mpsc::Sender<StateCommand>,
    event_bus: &EventBus,
) -> Result<()> {
    let devices: Vec<Z2mDevice> = serde_json::from_slice(payload)?;
    let mut device_map = HashMap::new();

    for device in devices {
        let mut info = Z2mDeviceInfo {
            ieee_address: device.ieee_address.clone(),
            friendly_name: device.friendly_name,
            supported: device.supported.unwrap_or(true),
            supports_brightness: false,
            supports_color_temp: false,
            color_temp_min: None,
            color_temp_max: None,
            supports_color_xy: false,
            supports_color_hs: false,
        };

        if let Some(def) = device.definition {
            parse_capabilities(&def.exposes, &mut info);
        }

        debug!(
            "Device {}: brightness={}, color_temp={}, color_xy={}, color_hs={}",
            info.friendly_name,
            info.supports_brightness,
            info.supports_color_temp,
            info.supports_color_xy,
            info.supports_color_hs
        );
        device_map.insert(device.ieee_address, info);
    }

    let prev_count = state.load().device_map.len();
    let new_count = device_map.len();
    if new_count != prev_count {
        info!("Discovered {} devices from Z2M", new_count);
    }
    let _ = state_tx.send(StateCommand::UpdateDevices(device_map)).await;
    event_bus.publish(Event::DevicesUpdated);
    Ok(())
}

fn parse_capabilities(exposes: &[serde_json::Value], info: &mut Z2mDeviceInfo) {
    for expose in exposes {
        if let Some(features) = expose.get("features").and_then(|f| f.as_array()) {
            parse_capabilities(features, info);
        }

        let name = expose
            .get("name")
            .or_else(|| expose.get("property"))
            .and_then(|n| n.as_str());

        match name {
            Some("brightness") => info.supports_brightness = true,
            Some("color_temp") => {
                info.supports_color_temp = true;
                if let Some(min) = expose.get("value_min").and_then(|v| v.as_u64()) {
                    info.color_temp_min = Some(min as u16);
                }
                if let Some(max) = expose.get("value_max").and_then(|v| v.as_u64()) {
                    info.color_temp_max = Some(max as u16);
                }
            }
            Some("color_xy") => info.supports_color_xy = true,
            Some("color_hs") => info.supports_color_hs = true,
            _ => {}
        }
    }
}

async fn handle_bridge_groups(
    payload: &Bytes,
    state: &SharedState,
    state_tx: &mpsc::Sender<StateCommand>,
    event_bus: &EventBus,
) -> Result<()> {
    let groups: Vec<Z2mGroup> = serde_json::from_slice(payload)?;
    let mut group_map = HashMap::new();

    for group in groups {
        let members = group
            .members
            .into_iter()
            .map(|m| Z2mGroupMember {
                ieee_address: m.ieee_address,
                endpoint: m.endpoint,
            })
            .collect();

        let scenes = group
            .scenes
            .into_iter()
            .map(|s| Z2mScene {
                id: s.id,
                name: s.name,
            })
            .collect::<Vec<_>>();

        if !scenes.is_empty() {
            debug!(
                "Group '{}' has {} scenes",
                group.friendly_name,
                scenes.len()
            );
        }

        group_map.insert(
            group.friendly_name.clone(),
            Z2mGroupInfo {
                id: group.id,
                friendly_name: group.friendly_name,
                members,
                scenes,
            },
        );
    }

    let prev_count = state.load().group_map.len();
    let new_count = group_map.len();
    if new_count != prev_count {
        info!("Discovered {} groups from Z2M", new_count);
    }
    let _ = state_tx.send(StateCommand::UpdateGroups(group_map)).await;
    event_bus.publish(Event::GroupsUpdated);
    Ok(())
}

async fn handle_availability(
    device_name: &str,
    payload: &Bytes,
    state_tx: &mpsc::Sender<StateCommand>,
) -> Result<()> {
    let text = std::str::from_utf8(payload)?;
    let available = text.contains("online");
    debug!("Device '{}': available={}", device_name, available);

    // The StateManager resolves the friendly name, buffers reports that
    // arrive before the device list, and publishes DeviceAvailabilityChanged
    // AFTER the state update — so event subscribers never read stale
    // availability from SharedState.
    let _ = state_tx
        .send(StateCommand::SetDeviceAvailability {
            friendly_name: device_name.to_string(),
            available,
        })
        .await;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_device_state(
    relative_topic: &str,
    payload: &Bytes,
    state: &SharedState,
    state_tx: &mpsc::Sender<StateCommand>,
    event_bus: &EventBus,
    config: &AppConfig,
    lookup: &RoomLookup,
) -> Result<()> {
    let device_name = relative_topic;

    let Ok(msg) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return Ok(());
    };

    let current = state.load();
    let device_ieee = current.friendly_to_ieee.get(device_name);

    // Check if this is a remote action
    if let Some(action) = msg.get("action").and_then(|v| v.as_str())
        && !action.is_empty()
        && let Some(ieee) = device_ieee
    {
        if record_remote_action(&REMOTE_ACTION_STATE, ieee, action, Instant::now()) {
            debug!("Remote action '{}' from '{}'", action, device_name);
            event_bus.publish(Event::RemoteAction {
                remote_ieee: ieee.clone(),
                action: action.to_string(),
            });
        } else {
            trace!(
                "Ignoring action='{}' republish from '{}' — within dedup window",
                action, device_name
            );
        }
    }

    // Check if this is a motion sensor update
    if let Some(occupancy) = msg.get("occupancy").and_then(|v| v.as_bool())
        && let Some(ieee) = device_ieee.cloned()
    {
        let illuminance = if let Some(lux) = msg.get("illuminance").and_then(|v| v.as_u64()) {
            Some(Illuminance::Lux(lux as u16))
        } else {
            msg.get("illuminance_above_threshold")
                .and_then(|v| v.as_bool())
                .map(Illuminance::AboveThreshold)
        };

        // Persist illuminance to room state for observability
        if let Some(ref illum) = illuminance
            && let Some(room_ids) = lookup.sensor_rooms.get(&ieee)
        {
            for room_id in room_ids {
                let _ = state_tx
                    .send(StateCommand::UpdateRoomState {
                        room_id: room_id.clone(),
                        update: RoomStateUpdate::Illuminance(illum.clone()),
                    })
                    .await;
            }
        }

        // Only a change in occupancy is a motion event. Z2M re-emits the full
        // device state for every attribute update (battery, illuminance,
        // linkquality...), and each of those republishes carries the unchanged
        // occupancy value — which would otherwise reset the room's motion-off
        // timer indefinitely and keep the lights on forever.
        if !record_occupancy(&OCCUPANCY_STATE, &ieee, occupancy) {
            trace!(
                "Ignoring occupancy={} republish from '{}' — no transition",
                occupancy, device_name
            );
        } else if occupancy {
            event_bus.publish(Event::MotionDetected {
                room_id: String::new(),
                sensor_ieee: ieee,
                illuminance,
            });
        } else {
            event_bus.publish(Event::MotionCleared {
                room_id: String::new(),
                sensor_ieee: ieee,
            });
        }
    }

    // Track light ON/OFF state from Z2M messages
    let state_str = msg.get("state").and_then(|v| v.as_str());
    let has_brightness = msg.get("brightness").and_then(|v| v.as_u64());
    let has_color_temp = msg.get("color_temp").and_then(|v| v.as_u64());
    let is_on = state_str.is_some_and(|s| s == "ON");
    let is_off = state_str.is_some_and(|s| s == "OFF");

    if (is_on || is_off)
        && let Some(room_id) = find_room_for_device(device_name, &current, lookup)
    {
        if is_on {
            let brightness = has_brightness.map(|b| b as u8);
            let color_temp = has_color_temp.map(|ct| ct as u16);
            // Only set lights_on state, don't change update_source
            // (that's handled by external change detection below)
            let room_state = current.rooms.get(&room_id);
            let source = room_state
                .map(|rs| rs.update_source)
                .unwrap_or(UpdateSource::Circadian);
            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.clone(),
                    update: RoomStateUpdate::LightsOn {
                        brightness,
                        color_temp_mired: color_temp,
                        source,
                    },
                })
                .await;
        } else {
            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.clone(),
                    update: RoomStateUpdate::LightsOff,
                })
                .await;
        }
    }

    if is_on
        && (has_brightness.is_some() || has_color_temp.is_some())
        && let Some(room_id) = find_room_for_device(device_name, &current, lookup)
        && let Some(room_state) = current.rooms.get(&room_id)
        // Only check if circadian is actively managing this room
        && room_state.lights_on
        && room_state.update_source == UpdateSource::Circadian
        && !room_state.circadian_paused
    {
        // Check if enough time has passed since jeha's last push
        // to distinguish our own echoes from external changes.
        // Use transition_secs + 5s buffer as the quiet window.
        let transition_secs = config
            .rooms
            .get(&room_id)
            .map(|r| {
                r.effective_circadian(&config.circadian.defaults)
                    .transition_secs
            })
            .unwrap_or(30);
        let quiet_window = std::time::Duration::from_secs(transition_secs as u64 + 5);

        let outside_quiet_window = room_state
            .last_jeha_push
            .is_none_or(|push_time| push_time.elapsed() > quiet_window);

        if outside_quiet_window {
            // Compare against intended values (not current_*, which drifts from Z2M echoes)
            let brightness_tolerance = config.general.external_brightness_tolerance;
            let color_temp_tolerance = config.general.external_color_temp_tolerance;
            let brightness_changed = match (has_brightness, room_state.intended_brightness) {
                (Some(reported), Some(expected)) => {
                    (reported as i64 - expected as i64).unsigned_abs() > brightness_tolerance
                }
                _ => false,
            };
            let color_temp_changed = match (has_color_temp, room_state.intended_color_temp_mired) {
                (Some(reported), Some(expected)) => {
                    (reported as i64 - expected as i64).unsigned_abs() > color_temp_tolerance
                }
                _ => false,
            };

            if brightness_changed || color_temp_changed {
                // Dedup rapid duplicates: the actor's pause update from a prior
                // message may not yet be visible in `current`, so multiple
                // messages can pass the `circadian_paused` guard above. Scope
                // the MutexGuard tightly so it cannot be held across `.await`.
                let recently_emitted = {
                    let mut dedup = EXTERNAL_CHANGE_DEDUP.lock().unwrap();
                    let now = Instant::now();
                    let already = dedup
                        .get(&room_id)
                        .is_some_and(|t| now.duration_since(*t) < EXTERNAL_CHANGE_DEDUP_WINDOW);
                    if !already {
                        dedup.insert(room_id.clone(), now);
                    }
                    already
                };
                if recently_emitted {
                    debug!(
                        "External light change for '{}' suppressed (dedup window)",
                        room_id
                    );
                } else {
                    let external_override_secs = config.general.external_override_secs;
                    // `changed` flags name the comparison that actually tripped
                    // the tolerance — reported vs *intended*. Without them the
                    // line can read as a no-op ("brightness 1->1") when the
                    // trigger was really intended=77 vs reported=1.
                    info!(
                        "External light change detected in room '{}' (via '{}'): \
                         brightness {:?}->{:?} (intended {:?}, changed: {}), \
                         color_temp {:?}->{:?} (intended {:?}, changed: {}). \
                         Pausing circadian for {}m.",
                        room_id,
                        device_name,
                        room_state.current_brightness,
                        has_brightness,
                        room_state.intended_brightness,
                        brightness_changed,
                        room_state.current_color_temp_mired,
                        has_color_temp,
                        room_state.intended_color_temp_mired,
                        color_temp_changed,
                        external_override_secs / 60,
                    );
                    let _ = state_tx
                        .send(StateCommand::UpdateRoomState {
                            room_id: room_id.clone(),
                            update: RoomStateUpdate::ExternalChange {
                                ttl_secs: external_override_secs,
                            },
                        })
                        .await;
                    event_bus.publish(Event::ExternalLightChange {
                        room_id,
                        device_name: device_name.to_string(),
                    });
                }
            }
        }
    }

    Ok(())
}

/// Find which room a device (by friendly name) belongs to.
/// Rooms with a Z2M group only match the group topic (avoids processing
/// duplicate state from individual device topics); rooms with explicit
/// lights match the individual device by IEEE.
fn find_room_for_device(
    device_name: &str,
    system_state: &crate::state::SystemState,
    lookup: &RoomLookup,
) -> Option<String> {
    if let Some(room_id) = lookup.group_to_room.get(device_name) {
        return Some(room_id.clone());
    }
    system_state
        .friendly_to_ieee
        .get(device_name)
        .and_then(|ieee| lookup.ieee_to_room.get(ieee))
        .cloned()
}

pub fn resolve_topic(state: &SharedState, ieee: &str, base_topic: &str) -> Option<String> {
    let current = state.load();
    current
        .device_map
        .get(ieee)
        .map(|d| format!("{}/{}", base_topic, d.friendly_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_room_lookup_from_config() {
        let toml_str = r#"
schema_version = 1

[rooms.kitchen]
z2m_group = "Kitchen"
motion_sensor = "0x00158d000aaaaaaa"

[rooms.office]
lights = ["0x001788010aaaaaa1", "0x001788010aaaaaa2"]
motion_sensor = "0x00158d000aaaaaaa"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let lookup = RoomLookup::from_config(&config);

        // Group-based room matches by group name only, not by member IEEE
        assert_eq!(lookup.group_to_room.get("Kitchen").unwrap(), "kitchen");
        assert!(!lookup.group_to_room.contains_key("Office"));

        // Light-based room matches by IEEE
        assert_eq!(
            lookup.ieee_to_room.get("0x001788010aaaaaa1").unwrap(),
            "office"
        );
        assert_eq!(
            lookup.ieee_to_room.get("0x001788010aaaaaa2").unwrap(),
            "office"
        );

        // A sensor shared by two rooms maps to both
        let mut rooms = lookup
            .sensor_rooms
            .get("0x00158d000aaaaaaa")
            .unwrap()
            .clone();
        rooms.sort();
        assert_eq!(rooms, vec!["kitchen", "office"]);
    }

    #[test]
    fn test_record_occupancy_only_reports_transitions() {
        let cache = Mutex::new(HashMap::new());
        let sensor = "0x00158d000aaaaaaa";

        // First report after startup has nothing to compare against
        assert!(record_occupancy(&cache, sensor, false));

        // Retained-state republishes (battery/illuminance/linkquality reports
        // re-emitting the same occupancy) must not look like motion events
        assert!(!record_occupancy(&cache, sensor, false));
        assert!(!record_occupancy(&cache, sensor, false));

        // Real motion
        assert!(record_occupancy(&cache, sensor, true));
        assert!(!record_occupancy(&cache, sensor, true));

        // Real clear
        assert!(record_occupancy(&cache, sensor, false));
    }

    #[test]
    fn test_record_occupancy_tracks_sensors_independently() {
        let cache = Mutex::new(HashMap::new());
        let hallway = "0x00158d000aaaaaaa";
        let office = "0x00158d000bbbbbbb";

        assert!(record_occupancy(&cache, hallway, true));
        // A different sensor's first report is its own transition, and does
        // not consume the hallway's cached value
        assert!(record_occupancy(&cache, office, true));
        assert!(!record_occupancy(&cache, hallway, true));
        assert!(record_occupancy(&cache, office, false));
        assert!(!record_occupancy(&cache, hallway, true));
    }

    #[test]
    fn test_record_remote_action_suppresses_republished_action() {
        let cache = Mutex::new(HashMap::new());
        let remote = "0x00158d000ccccccc";
        let start = Instant::now();

        // First press is always an event.
        assert!(record_remote_action(&cache, remote, "on_hold", start));

        // Z2M republishing the device state (battery, linkquality, ...) repeats
        // the same action; those must not replay the press.
        assert!(!record_remote_action(
            &cache,
            remote,
            "on_hold",
            start + Duration::from_micros(130)
        ));
        assert!(!record_remote_action(
            &cache,
            remote,
            "on_hold",
            start + Duration::from_millis(52)
        ));

        // A genuine repeat press outside the window still counts.
        assert!(record_remote_action(
            &cache,
            remote,
            "on_hold",
            start + REMOTE_ACTION_DEDUP_WINDOW
        ));
    }

    #[test]
    fn test_record_remote_action_allows_distinct_actions_back_to_back() {
        let cache = Mutex::new(HashMap::new());
        let remote = "0x00158d000ccccccc";
        let start = Instant::now();

        // Real remotes emit sequences like on_press -> on_hold within a few
        // milliseconds; only exact repeats are republishes.
        assert!(record_remote_action(&cache, remote, "on_press", start));
        assert!(record_remote_action(
            &cache,
            remote,
            "on_hold",
            start + Duration::from_millis(5)
        ));
        assert!(record_remote_action(
            &cache,
            remote,
            "on_press_release",
            start + Duration::from_millis(10)
        ));
    }

    #[test]
    fn test_record_remote_action_tracks_remotes_independently() {
        let cache = Mutex::new(HashMap::new());
        let bedroom = "0x00158d000ccccccc";
        let kitchen = "0x00158d000ddddddd";
        let start = Instant::now();

        assert!(record_remote_action(&cache, bedroom, "toggle", start));
        // Same action from a different remote is its own press.
        assert!(record_remote_action(&cache, kitchen, "toggle", start));
        assert!(!record_remote_action(
            &cache,
            bedroom,
            "toggle",
            start + Duration::from_millis(1)
        ));
    }
}
