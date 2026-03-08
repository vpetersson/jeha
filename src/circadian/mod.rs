pub mod curve;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{Timelike, Utc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::types::{AppConfig, RoomConfig};
use crate::event::{Event, EventBus};
use crate::mqtt::publish::Publisher;
use crate::state::{RoomStateUpdate, SharedState, StateCommand, UpdateSource};

use curve::{CircadianParams, CircadianTarget, compute_target, parse_time_to_minutes};

pub struct CircadianEngine {
    config: Arc<AppConfig>,
    state: SharedState,
    state_tx: mpsc::Sender<StateCommand>,
    publisher: Arc<Publisher>,
    event_bus: EventBus,
    cancel: CancellationToken,
}

impl CircadianEngine {
    pub fn new(
        config: Arc<AppConfig>,
        state: SharedState,
        state_tx: mpsc::Sender<StateCommand>,
        publisher: Arc<Publisher>,
        event_bus: EventBus,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            state,
            state_tx,
            publisher,
            event_bus,
            cancel,
        }
    }

    pub async fn run(self) {
        info!("Circadian engine started");
        let interval_secs = self.config.circadian.defaults.update_interval_secs;

        if let Err(e) = self.update_all_rooms().await {
            warn!("Initial circadian push failed: {}", e);
        }

        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        let mut event_rx = self.event_bus.subscribe();

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("Circadian engine shutting down");
                    return;
                }
                _ = interval.tick() => {
                    if let Err(e) = self.update_all_rooms().await {
                        warn!("Circadian update failed: {}", e);
                    }
                }
                Ok(event) = event_rx.recv() => {
                    match event {
                        Event::DeviceAvailabilityChanged { ieee, available: true } => {
                            debug!("Device {} came online, pushing circadian state", ieee);
                            if let Err(e) = self.push_for_device(&ieee).await {
                                warn!("Failed to push circadian to {}: {}", ieee, e);
                            }
                        }
                        Event::MqttConnected => {
                            info!("MQTT reconnected, re-pushing circadian to all rooms");
                            if let Err(e) = self.update_all_rooms().await {
                                warn!("Circadian re-push failed: {}", e);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn current_minutes(&self) -> u32 {
        let now = Utc::now();
        let tz: chrono_tz::Tz = self
            .config
            .general
            .timezone
            .parse()
            .unwrap_or(chrono_tz::UTC);
        let local = now.with_timezone(&tz);
        local.hour() * 60 + local.minute()
    }

    fn params_for_room(&self, room_config: &RoomConfig) -> CircadianParams {
        let defaults = room_config.effective_circadian(&self.config.circadian.defaults);
        CircadianParams {
            wake_minutes: parse_time_to_minutes(&defaults.wake_time),
            sleep_minutes: parse_time_to_minutes(&defaults.sleep_time),
            ramp_duration_mins: defaults.ramp_duration_mins,
            start_temp_k: defaults.start_temp_k,
            peak_temp_k: defaults.peak_temp_k,
            end_temp_k: defaults.end_temp_k,
            start_brightness: defaults.start_brightness,
            peak_brightness: defaults.peak_brightness,
            end_brightness: defaults.end_brightness,
            curve: defaults.curve,
        }
    }

    pub fn compute_room_target(&self, room_id: &str) -> Option<CircadianTarget> {
        let room_config = self.config.rooms.get(room_id)?;
        if !room_config.circadian_enabled {
            return None;
        }
        let params = self.params_for_room(room_config);
        let minutes = self.current_minutes();
        Some(compute_target(&params, minutes))
    }

    async fn update_all_rooms(&self) -> Result<()> {
        let minutes = self.current_minutes();
        let current_state = self.state.load();
        let mut updated_rooms: Vec<String> = Vec::new();
        let mut target_logged = false;

        for (room_id, room_config) in &self.config.rooms {
            if !room_config.circadian_enabled {
                continue;
            }

            let room_state = current_state.rooms.get(room_id);

            let lights_on = room_state.map(|r| r.lights_on).unwrap_or(false);
            if !lights_on {
                continue;
            }

            if let Some(rs) = room_state {
                // Check circadian pause/snooze
                if rs.circadian_paused {
                    if rs.is_circadian_paused() {
                        debug!("Skipping room '{}': circadian paused", room_id);
                        continue;
                    } else {
                        // Snooze expired — auto-resume
                        info!("Circadian snooze expired for room '{}', resuming", room_id);
                        let _ = self
                            .state_tx
                            .send(StateCommand::UpdateRoomState {
                                room_id: room_id.clone(),
                                update: crate::state::RoomStateUpdate::CircadianPause {
                                    paused: false,
                                    until: None,
                                },
                            })
                            .await;
                    }
                }

                if rs.update_source == UpdateSource::Manual {
                    if rs.is_manual_override_active() {
                        debug!("Skipping room '{}': manual override active", room_id);
                        continue;
                    } else {
                        // Manual override TTL expired — clear it and let circadian take over
                        info!(
                            "Manual override TTL expired for room '{}', resuming circadian",
                            room_id
                        );
                        let _ = self
                            .state_tx
                            .send(StateCommand::UpdateRoomState {
                                room_id: room_id.clone(),
                                update: RoomStateUpdate::LightsOn {
                                    brightness: None,
                                    color_temp_mired: None,
                                    source: UpdateSource::Circadian,
                                },
                            })
                            .await;
                        let _ = self
                            .state_tx
                            .send(StateCommand::UpdateRoomState {
                                room_id: room_id.clone(),
                                update: RoomStateUpdate::ManualOverrideTtl { until: None },
                            })
                            .await;
                    }
                }
            }

            let params = self.params_for_room(room_config);
            let target = compute_target(&params, minutes);
            let transition = room_config
                .effective_circadian(&self.config.circadian.defaults)
                .transition_secs;

            if !target_logged {
                info!(
                    "Circadian target: {}K ({}mired), brightness {}",
                    target.color_temp_k, target.color_temp_mired, target.brightness
                );
                target_logged = true;
            }

            if let Some(ref group) = room_config.z2m_group {
                self.publisher
                    .push_circadian_group(
                        group,
                        target.brightness,
                        target.color_temp_mired,
                        transition,
                    )
                    .await?;
            } else {
                for ieee in &room_config.lights {
                    let supports_color_temp = current_state
                        .device_map
                        .get(ieee)
                        .is_some_and(|d| d.supports_color_temp);
                    let ct = if supports_color_temp {
                        Some(target.color_temp_mired)
                    } else {
                        None
                    };
                    self.publisher
                        .turn_on_ieee(
                            ieee,
                            Some(target.brightness),
                            ct,
                            Some(transition),
                        )
                        .await?;
                }
            }

            let _ = self
                .state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.clone(),
                    update: RoomStateUpdate::LightsOn {
                        brightness: Some(target.brightness),
                        color_temp_mired: Some(target.color_temp_mired),
                        source: UpdateSource::Circadian,
                    },
                })
                .await;

            let _ = self
                .state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.clone(),
                    update: RoomStateUpdate::JehaPush,
                })
                .await;

            updated_rooms.push(room_id.clone());
        }

        if !updated_rooms.is_empty() {
            debug!("Circadian pushed to: {}", updated_rooms.join(", "));
        }

        Ok(())
    }

    async fn push_for_device(&self, ieee: &str) -> Result<()> {
        let minutes = self.current_minutes();

        for (room_id, room_config) in &self.config.rooms {
            let is_in_room = room_config.lights.iter().any(|l| l == ieee);
            if !is_in_room {
                let current_state = self.state.load();
                if let Some(ref group_name) = room_config.z2m_group {
                    if let Some(group) = current_state.group_map.get(group_name) {
                        if !group.members.iter().any(|m| m.ieee_address == ieee) {
                            continue;
                        }
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            }

            let params = self.params_for_room(room_config);
            let target = compute_target(&params, minutes);

            let current_state = self.state.load();
            let supports_color_temp = current_state
                .device_map
                .get(ieee)
                .is_some_and(|d| d.supports_color_temp);
            let ct = if supports_color_temp {
                Some(target.color_temp_mired)
            } else {
                None
            };

            self.publisher
                .turn_on_ieee(
                    ieee,
                    Some(target.brightness),
                    ct,
                    Some(3),
                )
                .await?;

            let _ = self
                .state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.clone(),
                    update: RoomStateUpdate::JehaPush,
                })
                .await;

            info!(
                "Pushed circadian state to newly online device {} (room '{}'): brightness={}, color_temp={}K",
                ieee, room_id, target.brightness, target.color_temp_k
            );
            break;
        }

        Ok(())
    }
}
