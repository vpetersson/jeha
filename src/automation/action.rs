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

            if let Some(ref group) = room_config.z2m_group {
                publisher
                    .turn_on_group(group, Some(bright), ct_mired, trans)
                    .await?;
            } else {
                for ieee in &room_config.lights {
                    publisher
                        .turn_on_ieee(ieee, Some(bright), ct_mired, trans)
                        .await?;
                }
            }

            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.to_string(),
                    update: RoomStateUpdate::LightsOn {
                        brightness: Some(bright),
                        color_temp_mired: ct_mired,
                        source,
                    },
                })
                .await;

            // Mark as jeha push so Z2M echoes don't trigger external change detection
            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.to_string(),
                    update: RoomStateUpdate::JehaPush {
                        brightness: Some(bright),
                        color_temp_mired: ct_mired,
                    },
                })
                .await;

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

            if let Some(ref group) = room_config.z2m_group {
                publisher.turn_off_group(group, trans).await?;
            } else {
                for ieee in &room_config.lights {
                    publisher.turn_off_ieee(ieee, trans).await?;
                }
            }

            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.to_string(),
                    update: RoomStateUpdate::LightsOff,
                })
                .await;

            debug!("Lights OFF in room '{}'", room_id);
        }

        ActionConfig::SetBrightness {
            brightness,
            transition,
        } => {
            let trans = transition.or(Some(3));
            if let Some(ref group) = room_config.z2m_group {
                publisher
                    .turn_on_group(group, Some(*brightness), None, trans)
                    .await?;
            } else {
                for ieee in &room_config.lights {
                    publisher
                        .turn_on_ieee(ieee, Some(*brightness), None, trans)
                        .await?;
                }
            }

            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.to_string(),
                    update: RoomStateUpdate::LightsOn {
                        brightness: Some(*brightness),
                        color_temp_mired: None,
                        source: UpdateSource::Manual,
                    },
                })
                .await;

            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.to_string(),
                    update: RoomStateUpdate::JehaPush {
                        brightness: Some(*brightness),
                        color_temp_mired: None,
                    },
                })
                .await;

            debug!(
                "Set brightness {} in room '{}'",
                brightness, room_id
            );
        }

        ActionConfig::SetColorTemp {
            color_temp_k,
            transition,
        } => {
            let ct_mired = (1_000_000u32 / *color_temp_k as u32) as u16;
            let trans = transition.or(Some(3));
            if let Some(ref group) = room_config.z2m_group {
                publisher
                    .turn_on_group(group, None, Some(ct_mired), trans)
                    .await?;
            } else {
                for ieee in &room_config.lights {
                    publisher
                        .turn_on_ieee(ieee, None, Some(ct_mired), trans)
                        .await?;
                }
            }

            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.to_string(),
                    update: RoomStateUpdate::LightsOn {
                        brightness: None,
                        color_temp_mired: Some(ct_mired),
                        source: UpdateSource::Manual,
                    },
                })
                .await;

            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.to_string(),
                    update: RoomStateUpdate::JehaPush {
                        brightness: None,
                        color_temp_mired: Some(ct_mired),
                    },
                })
                .await;

            debug!(
                "Set color temp {}K ({}mired) in room '{}'",
                color_temp_k, ct_mired, room_id
            );
        }
    }

    Ok(())
}
