use std::collections::HashMap;

use anyhow::Result;
use bytes::Bytes;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::config::types::AppConfig;
use crate::event::{Event, EventBus};
use crate::state::{
    RoomStateUpdate, SharedState, StateCommand, UpdateSource, Z2mDeviceInfo, Z2mGroupInfo,
    Z2mGroupMember, Z2mScene,
};

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

pub async fn handle_message(
    topic: &str,
    payload: &Bytes,
    base_topic: &str,
    state: &SharedState,
    state_tx: &mpsc::Sender<StateCommand>,
    event_bus: &EventBus,
    config: &AppConfig,
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
            handle_availability(device_name, payload, state, event_bus)?;
        }
        _ => {
            handle_device_state(relative, payload, state, state_tx, event_bus, config).await?;
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
            available: true,
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

fn handle_availability(
    device_name: &str,
    payload: &Bytes,
    state: &SharedState,
    event_bus: &EventBus,
) -> Result<()> {
    let text = std::str::from_utf8(payload)?;
    let available = text.contains("online");

    let current = state.load();
    let ieee = current
        .device_map
        .values()
        .find(|d| d.friendly_name == device_name)
        .map(|d| d.ieee_address.clone());

    if let Some(ieee) = ieee {
        debug!(
            "Device '{}' ({}): available={}",
            device_name, ieee, available
        );
        event_bus.publish(Event::DeviceAvailabilityChanged { ieee, available });
    }

    Ok(())
}

async fn handle_device_state(
    relative_topic: &str,
    payload: &Bytes,
    state: &SharedState,
    state_tx: &mpsc::Sender<StateCommand>,
    event_bus: &EventBus,
    config: &AppConfig,
) -> Result<()> {
    let device_name = relative_topic;

    let Ok(msg) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return Ok(());
    };

    let current = state.load();

    // Check if this is a remote action
    if let Some(action) = msg.get("action").and_then(|v| v.as_str()) {
        if !action.is_empty() {
            let ieee = current
                .device_map
                .values()
                .find(|d| d.friendly_name == device_name)
                .map(|d| d.ieee_address.clone());

            if let Some(ieee) = ieee {
                debug!("Remote action '{}' from '{}'", action, device_name);
                event_bus.publish(Event::RemoteAction {
                    remote_ieee: ieee,
                    action: action.to_string(),
                });
            }
        }
    }

    // Check if this is a motion sensor update
    if let Some(occupancy) = msg.get("occupancy").and_then(|v| v.as_bool()) {
        let ieee = current
            .device_map
            .values()
            .find(|d| d.friendly_name == device_name)
            .map(|d| d.ieee_address.clone());

        if let Some(ieee) = ieee {
            if occupancy {
                event_bus.publish(Event::MotionDetected {
                    room_id: String::new(),
                    sensor_ieee: ieee,
                });
            } else {
                event_bus.publish(Event::MotionCleared {
                    room_id: String::new(),
                    sensor_ieee: ieee,
                });
            }
        }
    }

    // Track light ON/OFF state from Z2M messages
    let state_str = msg.get("state").and_then(|v| v.as_str());
    let has_brightness = msg.get("brightness").and_then(|v| v.as_u64());
    let has_color_temp = msg.get("color_temp").and_then(|v| v.as_u64());
    let is_on = state_str.is_some_and(|s| s == "ON");
    let is_off = state_str.is_some_and(|s| s == "OFF");

    if (is_on || is_off)
        && let Some(room_id) = find_room_for_device(device_name, &current, config)
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
        && let Some(room_id) = find_room_for_device(device_name, &current, config)
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
                let external_override_secs = config.general.external_override_secs;
                info!(
                    "External light change detected in room '{}' (via '{}'): \
                     brightness {:?}->{:?} (intended {:?}), color_temp {:?}->{:?} (intended {:?}). \
                     Pausing circadian for {}m.",
                    room_id,
                    device_name,
                    room_state.current_brightness,
                    has_brightness,
                    room_state.intended_brightness,
                    room_state.current_color_temp_mired,
                    has_color_temp,
                    room_state.intended_color_temp_mired,
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

    Ok(())
}

/// Find which room a device (by friendly name) belongs to.
/// Checks Z2M group membership and direct light IEEE matches.
fn find_room_for_device(
    device_name: &str,
    system_state: &crate::state::SystemState,
    config: &AppConfig,
) -> Option<String> {
    // Find the device's IEEE address from friendly name
    let ieee = system_state
        .device_map
        .values()
        .find(|d| d.friendly_name == device_name)
        .map(|d| d.ieee_address.as_str());

    for (room_id, room_config) in &config.rooms {
        // Check direct light match
        if let Some(ieee) = ieee
            && room_config.lights.iter().any(|l| l == ieee)
        {
            return Some(room_id.clone());
        }

        // Check Z2M group membership
        if let Some(ref group_name) = room_config.z2m_group {
            // Check if this IS the group topic (e.g., "Living Room")
            if device_name == group_name {
                return Some(room_id.clone());
            }

            // Check if device is a member of this group
            if let Some(ieee) = ieee
                && let Some(group) = system_state.group_map.get(group_name)
                && group.members.iter().any(|m| m.ieee_address == ieee)
            {
                return Some(room_id.clone());
            }
        }
    }

    None
}

pub fn resolve_topic(state: &SharedState, ieee: &str, base_topic: &str) -> Option<String> {
    let current = state.load();
    current
        .device_map
        .get(ieee)
        .map(|d| format!("{}/{}", base_topic, d.friendly_name))
}
