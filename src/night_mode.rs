use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::circadian::CircadianEngine;
use crate::config::types::{AppConfig, RoomConfig};
use crate::event::{Event, EventBus};
use crate::mqtt::publish::Publisher;
use crate::schedule::LocalNow;
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

/// Scheduled night mode activation/deactivation based on schedule predicates.
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
            enm.schedule.is_some()
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
        let now = LocalNow::now(&self.config.general.timezone);
        let current = self.state.load();

        for (room_id, room_config) in &self.config.rooms {
            let enm = room_config.effective_night_mode(&self.config.night_mode.defaults);
            let Some(ref schedule) = enm.schedule else {
                continue;
            };

            let in_window = schedule.matches(&now);

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
}
