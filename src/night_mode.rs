use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{Timelike, Utc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::circadian::CircadianEngine;
use crate::config::types::{AppConfig, RoomConfig};
use crate::event::{Event, EventBus};
use crate::mqtt::publish::Publisher;
use crate::state::{RoomStateUpdate, SharedState, StateCommand, UpdateSource};

/// Activate night mode for a room: push night mode light values, pause circadian, set flag.
/// Only pushes light values if lights are currently on.
pub async fn activate_night_mode(
    room_id: &str,
    room_config: &RoomConfig,
    config: &AppConfig,
    publisher: &Publisher,
    state: &SharedState,
    state_tx: &mpsc::Sender<StateCommand>,
    event_bus: &EventBus,
) -> Result<()> {
    let enm = room_config.effective_night_mode(&config.night_mode.defaults);
    let ct_mired = (1_000_000u32 / enm.color_temp_k as u32) as u16;

    // Set night mode flag
    let _ = state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::NightMode(true),
        })
        .await;

    // Pause circadian
    let _ = state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::CircadianPause {
                paused: true,
                until: None,
            },
        })
        .await;

    // Only push values if lights are on
    let lights_on = state
        .load()
        .rooms
        .get(room_id)
        .map(|rs| rs.lights_on)
        .unwrap_or(false);

    if lights_on {
        if let Some(ref group) = room_config.z2m_group {
            publisher
                .turn_on_group(group, Some(enm.brightness), Some(ct_mired), Some(3))
                .await?;
        } else {
            for ieee in &room_config.lights {
                let _ = publisher
                    .turn_on_ieee(ieee, Some(enm.brightness), Some(ct_mired), Some(3))
                    .await;
            }
        }

        let _ = state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::LightsOn {
                    brightness: Some(enm.brightness),
                    color_temp_mired: Some(ct_mired),
                    source: UpdateSource::Automation,
                },
            })
            .await;

        let _ = state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::JehaPush {
                    brightness: Some(enm.brightness),
                    color_temp_mired: Some(ct_mired),
                },
            })
            .await;
    }

    event_bus.publish(Event::NightModeChanged {
        room_id: room_id.to_string(),
        active: true,
    });

    Ok(())
}

/// Deactivate night mode for a room: clear flag, resume circadian, push circadian values.
pub async fn deactivate_night_mode(
    room_id: &str,
    room_config: &RoomConfig,
    publisher: &Publisher,
    state: &SharedState,
    state_tx: &mpsc::Sender<StateCommand>,
    event_bus: &EventBus,
    circadian_engine: &Option<Arc<CircadianEngine>>,
) -> Result<()> {
    let _ = state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::NightMode(false),
        })
        .await;

    let _ = state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::CircadianPause {
                paused: false,
                until: None,
            },
        })
        .await;

    // Reset source to circadian
    let _ = state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::LightsOn {
                brightness: None,
                color_temp_mired: None,
                source: UpdateSource::Circadian,
            },
        })
        .await;

    // Actively push circadian values so lights change immediately
    let lights_on = state
        .load()
        .rooms
        .get(room_id)
        .map(|rs| rs.lights_on)
        .unwrap_or(false);

    if lights_on {
        if let Some(engine) = circadian_engine {
            if let Some(target) = engine.compute_room_target(room_id) {
                let ct_mired = Some(target.color_temp_mired);
                if let Some(ref group) = room_config.z2m_group {
                    let _ = publisher
                        .turn_on_group(group, Some(target.brightness), ct_mired, Some(3))
                        .await;
                } else {
                    for ieee in &room_config.lights {
                        let _ = publisher
                            .turn_on_ieee(ieee, Some(target.brightness), ct_mired, Some(3))
                            .await;
                    }
                }

                let _ = state_tx
                    .send(StateCommand::UpdateRoomState {
                        room_id: room_id.to_string(),
                        update: RoomStateUpdate::LightsOn {
                            brightness: Some(target.brightness),
                            color_temp_mired: ct_mired,
                            source: UpdateSource::Circadian,
                        },
                    })
                    .await;

                let _ = state_tx
                    .send(StateCommand::UpdateRoomState {
                        room_id: room_id.to_string(),
                        update: RoomStateUpdate::JehaPush {
                            brightness: Some(target.brightness),
                            color_temp_mired: ct_mired,
                        },
                    })
                    .await;
            }
        }
    }

    event_bus.publish(Event::NightModeChanged {
        room_id: room_id.to_string(),
        active: false,
    });

    Ok(())
}

/// Scheduled night mode activation/deactivation based on start_time/end_time.
pub struct NightModeScheduler {
    config: Arc<AppConfig>,
    state: SharedState,
    state_tx: mpsc::Sender<StateCommand>,
    publisher: Arc<Publisher>,
    event_bus: EventBus,
    cancel: CancellationToken,
    circadian_engine: Option<Arc<CircadianEngine>>,
    /// Rooms manually deactivated during current schedule window (suppress re-activation).
    suppressed: HashSet<String>,
}

impl NightModeScheduler {
    pub fn new(
        config: Arc<AppConfig>,
        state: SharedState,
        state_tx: mpsc::Sender<StateCommand>,
        publisher: Arc<Publisher>,
        event_bus: EventBus,
        cancel: CancellationToken,
        circadian_engine: Option<Arc<CircadianEngine>>,
    ) -> Self {
        Self {
            config,
            state,
            state_tx,
            publisher,
            event_bus,
            cancel,
            circadian_engine,
            suppressed: HashSet::new(),
        }
    }

    pub async fn run(mut self) {
        let has_schedules = self.config.rooms.values().any(|rc| {
            let enm = rc.effective_night_mode(&self.config.night_mode.defaults);
            enm.start_time.is_some() && enm.end_time.is_some()
        });

        if !has_schedules {
            debug!("No rooms with night mode schedule configured");
            return;
        }

        info!("Night mode scheduler started");
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        let mut event_rx = self.event_bus.subscribe();

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("Night mode scheduler shutting down");
                    return;
                }
                _ = interval.tick() => {
                    self.check_schedules().await;
                }
                Ok(event) = event_rx.recv() => {
                    // Track manual deactivations to suppress re-activation
                    if let Event::NightModeChanged { room_id, active: false } = event {
                        self.suppressed.insert(room_id);
                    }
                }
            }
        }
    }

    async fn check_schedules(&mut self) {
        let now_minutes = self.current_minutes();
        let current = self.state.load();

        for (room_id, room_config) in &self.config.rooms {
            let enm = room_config.effective_night_mode(&self.config.night_mode.defaults);
            let (Some(start), Some(end)) = (&enm.start_time, &enm.end_time) else {
                continue;
            };

            let start_mins = parse_time_to_minutes(start);
            let end_mins = parse_time_to_minutes(end);
            let in_window = is_time_in_range(now_minutes, start_mins, end_mins);

            let is_active = current
                .rooms
                .get(room_id)
                .map(|rs| rs.night_mode_active)
                .unwrap_or(false);

            if in_window && !is_active && !self.suppressed.contains(room_id) {
                info!("Night mode schedule: activating '{}'", room_id);
                let _ = activate_night_mode(
                    room_id,
                    room_config,
                    &self.config,
                    &self.publisher,
                    &self.state,
                    &self.state_tx,
                    &self.event_bus,
                )
                .await;
            } else if !in_window && is_active {
                info!("Night mode schedule: deactivating '{}'", room_id);
                let _ = deactivate_night_mode(
                    room_id,
                    room_config,
                    &self.publisher,
                    &self.state,
                    &self.state_tx,
                    &self.event_bus,
                    &self.circadian_engine,
                )
                .await;
            }

            // Clear suppression when we exit the window
            if !in_window {
                self.suppressed.remove(room_id);
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
}

fn parse_time_to_minutes(time: &str) -> u32 {
    let parts: Vec<&str> = time.split(':').collect();
    if parts.len() == 2 {
        let h: u32 = parts[0].parse().unwrap_or(23);
        let m: u32 = parts[1].parse().unwrap_or(0);
        h * 60 + m
    } else {
        23 * 60 // fallback: 23:00
    }
}

/// Check if `now` is within the range [start, end), handling midnight crossover.
fn is_time_in_range(now: u32, start: u32, end: u32) -> bool {
    if start <= end {
        now >= start && now < end
    } else {
        // Crosses midnight (e.g., 23:00 - 06:00)
        now >= start || now < end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time_in_range_normal() {
        // 09:00 - 17:00
        assert!(is_time_in_range(600, 540, 1020)); // 10:00 in 09:00-17:00
        assert!(!is_time_in_range(480, 540, 1020)); // 08:00 not in 09:00-17:00
    }

    #[test]
    fn test_time_in_range_midnight_crossover() {
        // 23:00 - 06:00
        let start = 23 * 60; // 1380
        let end = 6 * 60; // 360
        assert!(is_time_in_range(1400, start, end)); // 23:20 in range
        assert!(is_time_in_range(120, start, end)); // 02:00 in range
        assert!(!is_time_in_range(720, start, end)); // 12:00 not in range
    }
}
