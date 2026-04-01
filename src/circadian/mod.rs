pub mod curve;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::calibration;
use crate::config::types::{AppConfig, RoomConfig};
use crate::event::{Event, EventBus};
use crate::mqtt::publish::Publisher;
use crate::schedule::LocalNow;
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
                            let supports_brightness = self
                                .state
                                .load()
                                .device_map
                                .get(&ieee)
                                .is_some_and(|d| d.supports_brightness);
                            if !supports_brightness {
                                debug!(
                                    "Device {} came online but does not support brightness, skipping circadian push",
                                    ieee
                                );
                            } else {
                                debug!("Device {} came online, pushing circadian state", ieee);
                                if let Err(e) = self.push_for_device(&ieee).await {
                                    warn!("Failed to push circadian to {}: {}", ieee, e);
                                }
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
        LocalNow::now(&self.config.general.timezone).minutes as u32
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

    /// Clamp a color_temp mired value to the narrowest supported range
    /// of all devices in the given Z2M group.
    fn clamp_color_temp_for_group(&self, group_name: &str, mired: u16) -> u16 {
        let current = self.state.load();
        let Some(group) = current.group_map.get(group_name) else {
            return mired;
        };

        let mut range_min: Option<u16> = None;
        let mut range_max: Option<u16> = None;

        for member in &group.members {
            if let Some(device) = current.device_map.get(&member.ieee_address)
                && device.supports_color_temp
            {
                if let Some(dev_min) = device.color_temp_min {
                    range_min = Some(range_min.map_or(dev_min, |cur: u16| cur.max(dev_min)));
                }
                if let Some(dev_max) = device.color_temp_max {
                    range_max = Some(range_max.map_or(dev_max, |cur: u16| cur.min(dev_max)));
                }
            }
        }

        let clamped = match (range_min, range_max) {
            (Some(min), Some(max)) if min <= max => mired.clamp(min, max),
            (Some(min), None) => mired.max(min),
            (None, Some(max)) => mired.min(max),
            _ => mired,
        };

        if clamped != mired {
            debug!(
                "Clamped color_temp {} -> {} mired for group '{}'",
                mired, clamped, group_name
            );
        }

        clamped
    }

    /// Clamp a color_temp mired value to a single device's supported range.
    fn clamp_color_temp_for_device(&self, ieee: &str, mired: u16) -> u16 {
        let current = self.state.load();
        let Some(device) = current.device_map.get(ieee) else {
            return mired;
        };
        let min = device.color_temp_min.unwrap_or(0);
        let max = device.color_temp_max.unwrap_or(u16::MAX);
        let clamped = mired.clamp(min, max);
        if clamped != mired {
            debug!(
                "Clamped color_temp {} -> {} mired for device '{}'",
                mired, clamped, ieee
            );
        }
        clamped
    }

    /// Get all device IEEEs for a room — from explicit lights list, or from group members.
    fn lights_for_room(
        &self,
        room_config: &RoomConfig,
        state: &crate::state::SystemState,
    ) -> Vec<String> {
        if !room_config.lights.is_empty() {
            return room_config.lights.clone();
        }
        if let Some(ref group_name) = room_config.z2m_group
            && let Some(group) = state.group_map.get(group_name)
        {
            return group
                .members
                .iter()
                .map(|m| m.ieee_address.clone())
                .collect();
        }
        Vec::new()
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
                if rs.night_mode_active {
                    debug!("Skipping room '{}': night mode active", room_id);
                    continue;
                }

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
                debug!(
                    "Circadian target: {}K ({}mired), brightness {}",
                    target.color_temp_k, target.color_temp_mired, target.brightness
                );
                target_logged = true;
            }

            let published_ct;
            let needs_per_device = room_config.z2m_group.as_ref().and_then(|group_name| {
                let group = current_state.group_map.get(group_name)?;
                let has_cal_diffs = calibration::group_needs_fanout(
                    group,
                    &self.config.light_calibration,
                    &current_state.device_map,
                );
                let has_cap_diffs =
                    calibration::group_has_mixed_capabilities(group, &current_state.device_map);
                if has_cal_diffs || has_cap_diffs {
                    debug!(
                        "Room '{}': per-device color_temp for group '{}' \
                         (calibration_diffs={}, capability_diffs={})",
                        room_id, group_name, has_cal_diffs, has_cap_diffs
                    );
                    Some(true)
                } else {
                    Some(false)
                }
            });

            if needs_per_device == Some(false) {
                // Uniform group — single group publish
                let group_name = room_config.z2m_group.as_ref().unwrap();
                published_ct = self.clamp_color_temp_for_group(group_name, target.color_temp_mired);
                self.publisher
                    .push_circadian_group(group_name, target.brightness, published_ct, transition)
                    .await?;
            } else if let Some(ref group_name) = room_config.z2m_group {
                // Mixed group: group publish as crude estimate, then per-device
                // corrections for any member whose ideal color_temp differs.
                let clamped_ct =
                    self.clamp_color_temp_for_group(group_name, target.color_temp_mired);
                published_ct = clamped_ct;
                self.publisher
                    .push_circadian_group(group_name, target.brightness, clamped_ct, transition)
                    .await?;

                if let Some(group) = current_state.group_map.get(group_name.as_str()) {
                    for member in &group.members {
                        let supports_ct = current_state
                            .device_map
                            .get(&member.ieee_address)
                            .is_some_and(|d| d.supports_color_temp);
                        if !supports_ct {
                            continue;
                        }
                        let device_ct = self.clamp_color_temp_for_device(
                            &member.ieee_address,
                            target.color_temp_mired,
                        );
                        let cal = calibration::resolve_for_device(
                            &member.ieee_address,
                            &self.config,
                            &current_state.device_map,
                        );
                        let device_info = current_state.device_map.get(&member.ieee_address);
                        let calibrated_ct = cal.apply_color_temp(device_ct, device_info);
                        if calibrated_ct != clamped_ct {
                            self.publisher
                                .push_circadian_ieee(
                                    &member.ieee_address,
                                    target.brightness,
                                    Some(device_ct),
                                    transition,
                                )
                                .await?;
                        }
                    }
                }
            } else {
                // No group — fall back to per-device publish
                published_ct = target.color_temp_mired;
                let lights = self.lights_for_room(room_config, &current_state);
                for ieee in &lights {
                    let supports_color_temp = current_state
                        .device_map
                        .get(ieee)
                        .is_some_and(|d| d.supports_color_temp);
                    let ct = if supports_color_temp {
                        Some(self.clamp_color_temp_for_device(ieee, target.color_temp_mired))
                    } else {
                        None
                    };
                    self.publisher
                        .push_circadian_ieee(ieee, target.brightness, ct, transition)
                        .await?;
                }
            }

            let _ = self
                .state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.clone(),
                    update: RoomStateUpdate::LightsOn {
                        brightness: Some(target.brightness),
                        color_temp_mired: Some(published_ct),
                        source: UpdateSource::Circadian,
                    },
                })
                .await;

            let _ = self
                .state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.clone(),
                    update: RoomStateUpdate::JehaPush {
                        brightness: Some(target.brightness),
                        color_temp_mired: Some(published_ct),
                    },
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

            // Only push to devices in rooms where lights are on
            {
                let current_state = self.state.load();
                if let Some(rs) = current_state.rooms.get(room_id) {
                    if !rs.lights_on {
                        debug!("Skipping device push for '{}': lights are off", room_id);
                        continue;
                    }
                    if rs.night_mode_active {
                        debug!("Skipping device push for '{}': night mode active", room_id);
                        continue;
                    }
                    if rs.circadian_paused && rs.is_circadian_paused() {
                        debug!("Skipping device push for '{}': circadian paused", room_id);
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
                Some(self.clamp_color_temp_for_device(ieee, target.color_temp_mired))
            } else {
                None
            };

            self.publisher
                .push_circadian_ieee(ieee, target.brightness, ct, 3)
                .await?;

            let _ = self
                .state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.clone(),
                    update: RoomStateUpdate::JehaPush {
                        brightness: Some(target.brightness),
                        color_temp_mired: ct,
                    },
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
