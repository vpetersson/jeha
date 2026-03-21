use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::debug;

use crate::circadian::CircadianEngine;
use crate::config::types::{ActionConfig, RoomConfig};
use crate::mqtt::publish::Publisher;
use crate::state::{RoomStateUpdate, StateCommand, UpdateSource};

pub async fn execute_action(
    action: &ActionConfig,
    room_id: &str,
    room_config: &RoomConfig,
    publisher: &Arc<Publisher>,
    state_tx: &mpsc::Sender<StateCommand>,
    circadian_engine: &Option<Arc<CircadianEngine>>,
) -> Result<()> {
    match action {
        ActionConfig::LightsOn {
            use_circadian,
            brightness,
            color_temp_k,
            transition,
        } => {
            let (bright, ct_mired, source) = if *use_circadian {
                if let Some(engine) = circadian_engine {
                    if let Some(target) = engine.compute_room_target(room_id) {
                        (
                            brightness.unwrap_or(target.brightness),
                            Some(target.color_temp_mired),
                            UpdateSource::Automation,
                        )
                    } else {
                        (brightness.unwrap_or(254), None, UpdateSource::Automation)
                    }
                } else {
                    (brightness.unwrap_or(254), None, UpdateSource::Automation)
                }
            } else {
                let ct = color_temp_k.map(|k| (1_000_000u32 / k as u32) as u16);
                (brightness.unwrap_or(254), ct, UpdateSource::Automation)
            };

            let trans = transition.or(Some(3));

            // Update state BEFORE MQTT publish so other tasks see lights_on=true immediately
            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.to_string(),
                    update: RoomStateUpdate::LightsOnWithPush {
                        brightness: Some(bright),
                        color_temp_mired: ct_mired,
                        source,
                    },
                })
                .await;

            if let Some(ref group) = room_config.z2m_group {
                publisher
                    .turn_on_group(group, Some(bright), ct_mired, trans)
                    .await?;
            } else {
                publish_on_all(room_config, publisher, Some(bright), ct_mired, trans).await?;
            }

            debug!("Lights ON in room '{}': brightness={}", room_id, bright);
        }

        ActionConfig::LightsOff {
            delay_secs,
            transition,
        } => {
            if *delay_secs > 0 {
                debug!(
                    "Delaying lights off for room '{}' by {}s",
                    room_id, delay_secs
                );
                tokio::time::sleep(Duration::from_secs(*delay_secs)).await;
            }

            let trans = transition.or(Some(3));

            // Update state BEFORE MQTT publish
            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.to_string(),
                    update: RoomStateUpdate::LightsOff,
                })
                .await;

            if let Some(ref group) = room_config.z2m_group {
                publisher.turn_off_group(group, trans).await?;
            } else {
                publish_off_all(room_config, publisher, trans).await?;
            }

            debug!("Lights OFF in room '{}'", room_id);
        }

        ActionConfig::SetBrightness {
            brightness,
            transition,
        } => {
            let trans = transition.or(Some(3));

            // Update state BEFORE MQTT publish
            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.to_string(),
                    update: RoomStateUpdate::LightsOnWithPush {
                        brightness: Some(*brightness),
                        color_temp_mired: None,
                        source: UpdateSource::Manual,
                    },
                })
                .await;

            if let Some(ref group) = room_config.z2m_group {
                publisher
                    .turn_on_group(group, Some(*brightness), None, trans)
                    .await?;
            } else {
                publish_on_all(room_config, publisher, Some(*brightness), None, trans).await?;
            }

            debug!("Set brightness {} in room '{}'", brightness, room_id);
        }

        ActionConfig::SetColorTemp {
            color_temp_k,
            transition,
        } => {
            let ct_mired = (1_000_000u32 / *color_temp_k as u32) as u16;
            let trans = transition.or(Some(3));

            // Update state BEFORE MQTT publish
            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.to_string(),
                    update: RoomStateUpdate::LightsOnWithPush {
                        brightness: None,
                        color_temp_mired: Some(ct_mired),
                        source: UpdateSource::Manual,
                    },
                })
                .await;

            if let Some(ref group) = room_config.z2m_group {
                publisher
                    .turn_on_group(group, None, Some(ct_mired), trans)
                    .await?;
            } else {
                publish_on_all(room_config, publisher, None, Some(ct_mired), trans).await?;
            }

            debug!(
                "Set color temp {}K ({}mired) in room '{}'",
                color_temp_k, ct_mired, room_id
            );
        }
    }

    Ok(())
}

/// Publish to all individual lights in a room concurrently using JoinSet.
/// Each closure receives a cloned IEEE address and publisher Arc.
async fn publish_on_all(
    room_config: &RoomConfig,
    publisher: &Arc<Publisher>,
    brightness: Option<u8>,
    color_temp_mired: Option<u16>,
    transition: Option<u32>,
) -> Result<()> {
    let mut set = tokio::task::JoinSet::new();
    for ieee in &room_config.lights {
        let pub_clone = publisher.clone();
        let ieee_clone = ieee.clone();
        set.spawn(async move {
            pub_clone
                .turn_on_ieee(&ieee_clone, brightness, color_temp_mired, transition)
                .await
        });
    }
    while let Some(result) = set.join_next().await {
        result??;
    }
    Ok(())
}

/// Turn off all individual lights in a room concurrently.
async fn publish_off_all(
    room_config: &RoomConfig,
    publisher: &Arc<Publisher>,
    transition: Option<u32>,
) -> Result<()> {
    let mut set = tokio::task::JoinSet::new();
    for ieee in &room_config.lights {
        let pub_clone = publisher.clone();
        let ieee_clone = ieee.clone();
        set.spawn(async move { pub_clone.turn_off_ieee(&ieee_clone, transition).await });
    }
    while let Some(result) = set.join_next().await {
        result??;
    }
    Ok(())
}
