use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde::Deserialize;
use tracing::info;

#[derive(Debug, Deserialize)]
struct Z2mDevice {
    ieee_address: String,
    friendly_name: String,
    supported: Option<bool>,
    #[serde(default)]
    definition: Option<Z2mDefinition>,
    #[serde(rename = "type")]
    device_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Z2mDefinition {
    #[serde(default)]
    exposes: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Z2mGroup {
    friendly_name: String,
    #[serde(default)]
    members: Vec<Z2mGroupMember>,
}

#[derive(Debug, Deserialize)]
struct Z2mGroupMember {
    ieee_address: String,
}

struct DeviceInfo {
    ieee_address: String,
    friendly_name: String,
    is_light: bool,
    is_motion_sensor: bool,
}

pub async fn run_init(host: &str, port: u16, output: Option<&Path>, base_topic: &str) -> Result<()> {
    info!("Connecting to MQTT at {}:{}", host, port);

    let mut opts = MqttOptions::new("jeha-init", host, port);
    opts.set_keep_alive(Duration::from_secs(10));
    opts.set_clean_session(true);

    let (client, mut event_loop) = AsyncClient::new(opts, 64);

    let mut devices_payload: Option<Vec<u8>> = None;
    let mut groups_payload: Option<Vec<u8>> = None;

    // Subscribe after connect, then wait for both retained messages
    let mut subscribed = false;
    let timeout = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            _ = &mut timeout => {
                if devices_payload.is_none() {
                    bail!("Timed out waiting for bridge/devices from Z2M. Is Zigbee2MQTT running?");
                }
                if groups_payload.is_none() {
                    bail!("Timed out waiting for bridge/groups from Z2M.");
                }
                break;
            }
            event = event_loop.poll() => {
                match event {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        if !subscribed {
                            client.subscribe(format!("{}/bridge/devices", base_topic), QoS::AtLeastOnce).await?;
                            client.subscribe(format!("{}/bridge/groups", base_topic), QoS::AtLeastOnce).await?;
                            subscribed = true;
                            info!("Connected, waiting for Z2M data...");
                        }
                    }
                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        let topic = &publish.topic;
                        if topic.ends_with("bridge/devices") {
                            devices_payload = Some(publish.payload.to_vec());
                            info!("Received bridge/devices");
                        } else if topic.ends_with("bridge/groups") {
                            groups_payload = Some(publish.payload.to_vec());
                            info!("Received bridge/groups");
                        }
                        if devices_payload.is_some() && groups_payload.is_some() {
                            break;
                        }
                    }
                    Err(e) => {
                        bail!("MQTT connection failed: {}. Check that the broker is reachable at {}:{}", e, host, port);
                    }
                    _ => {}
                }
            }
        }
    }

    let _ = client.disconnect().await;

    let devices: Vec<Z2mDevice> = serde_json::from_slice(&devices_payload.unwrap())?;
    let groups: Vec<Z2mGroup> = serde_json::from_slice(&groups_payload.unwrap())?;

    // Build device info map
    let device_map: HashMap<String, DeviceInfo> = devices
        .into_iter()
        .filter(|d| d.supported.unwrap_or(true))
        .map(|d| {
            let is_light = is_light_device(&d);
            let is_motion_sensor = is_motion_device(&d);
            (
                d.ieee_address.clone(),
                DeviceInfo {
                    ieee_address: d.ieee_address,
                    friendly_name: d.friendly_name,
                    is_light,
                    is_motion_sensor,
                },
            )
        })
        .collect();

    let config = generate_config(&groups, &device_map, base_topic);

    match output {
        Some(path) => {
            std::fs::write(path, &config)?;
            info!("Config written to {}", path.display());
        }
        None => {
            print!("{}", config);
        }
    }

    Ok(())
}

fn is_light_device(device: &Z2mDevice) -> bool {
    if device.device_type.as_deref() == Some("Coordinator") {
        return false;
    }
    if let Some(ref def) = device.definition {
        has_feature(&def.exposes, "brightness")
    } else {
        false
    }
}

fn is_motion_device(device: &Z2mDevice) -> bool {
    if let Some(ref def) = device.definition {
        has_feature(&def.exposes, "occupancy")
    } else {
        false
    }
}

fn has_feature(exposes: &[serde_json::Value], name: &str) -> bool {
    for expose in exposes {
        if let Some(features) = expose.get("features").and_then(|f| f.as_array())
            && has_feature(features, name)
        {
            return true;
        }
        let prop = expose
            .get("name")
            .or_else(|| expose.get("property"))
            .and_then(|n| n.as_str());
        if prop == Some(name) {
            return true;
        }
    }
    false
}

fn generate_config(
    groups: &[Z2mGroup],
    device_map: &HashMap<String, DeviceInfo>,
    base_topic: &str,
) -> String {
    let mut out = String::new();

    out.push_str("schema_version = 1\n");

    // Only include [mqtt] if non-default topic
    if base_topic != "zigbee2mqtt" {
        out.push_str(&format!("\n[mqtt]\nbase_topic = \"{}\"\n", base_topic));
    }

    // Generate a room for each group that contains lights
    for group in groups {
        let light_members: Vec<&DeviceInfo> = group
            .members
            .iter()
            .filter_map(|m| device_map.get(&m.ieee_address))
            .filter(|d| d.is_light)
            .collect();

        if light_members.is_empty() {
            continue;
        }

        let room_id = group
            .friendly_name
            .to_lowercase()
            .replace([' ', '-'], "_")
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>();

        out.push_str(&format!("\n[rooms.{}]\n", room_id));
        out.push_str(&format!("display_name = \"{}\"\n", group.friendly_name));
        out.push_str(&format!("z2m_group = \"{}\"\n", group.friendly_name));

        // Check if any motion sensor shares a member IEEE with this group,
        // or if there's a motion sensor with the same friendly name pattern
        let motion_sensor = find_motion_sensor_for_group(group, device_map);
        if let Some(ieee) = motion_sensor {
            out.push_str(&format!("motion_sensor = \"{}\"\n", ieee));
        }
    }

    out
}

fn find_motion_sensor_for_group(
    group: &Z2mGroup,
    device_map: &HashMap<String, DeviceInfo>,
) -> Option<String> {
    // Look for motion sensors that aren't in any group but share a name pattern
    // with this group (e.g., "Kitchen Motion" for group "Kitchen")
    let group_lower = group.friendly_name.to_lowercase();

    for device in device_map.values() {
        if !device.is_motion_sensor {
            continue;
        }
        let name_lower = device.friendly_name.to_lowercase();
        if name_lower.contains(&group_lower) || group_lower.contains(&name_lower) {
            return Some(device.ieee_address.clone());
        }
    }

    None
}
