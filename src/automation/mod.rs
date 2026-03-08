pub mod action;
pub mod condition;
pub mod trigger;

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::circadian::CircadianEngine;
use crate::config::types::{ActionConfig, AppConfig, AutomationConfig};
use crate::event::{Event, EventBus};
use crate::mqtt::publish::Publisher;
use crate::state::{SharedState, StateCommand};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum AutomationState {
    Running,
    Disabled,
    Faulted,
}

pub struct AutomationInfo {
    pub config: AutomationConfig,
    pub state: AutomationState,
}

pub struct AutomationEngine {
    config: Arc<AppConfig>,
    state: SharedState,
    state_tx: mpsc::Sender<StateCommand>,
    publisher: Arc<Publisher>,
    event_bus: EventBus,
    cancel: CancellationToken,
    circadian_engine: Option<Arc<CircadianEngine>>,
    automations: HashMap<String, AutomationInfo>,
    /// Per-room handles for pending motion-off timers. Aborted on new motion events.
    motion_off_handles: HashMap<String, tokio::task::JoinHandle<()>>,
}

impl AutomationEngine {
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
            automations: HashMap::new(),
            motion_off_handles: HashMap::new(),
        }
    }

    pub async fn run(mut self) {
        info!(
            "Automation engine started with {} automations",
            self.config.automations.len()
        );

        for auto_config in &self.config.automations {
            self.automations.insert(
                auto_config.id.clone(),
                AutomationInfo {
                    config: auto_config.clone(),
                    state: AutomationState::Running,
                },
            );
        }

        let mut event_rx = self.event_bus.subscribe();

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("Automation engine shutting down");
                    return;
                }
                Ok(event) = event_rx.recv() => {
                    self.handle_event(event).await;
                }
            }
        }
    }

    async fn handle_event(&mut self, event: Event) {
        // Built-in motion handling: rooms with motion_sensor + motion_timeout_secs
        // get automatic lights-on/off without needing explicit automations.
        let builtin_handled = self.handle_builtin_motion(&event).await;

        for (auto_id, auto_info) in &self.automations {
            if auto_info.state != AutomationState::Running {
                continue;
            }

            let config = &auto_info.config;

            for room_id in &config.rooms {
                // Skip user automations for rooms already handled by built-in motion
                if builtin_handled.contains(room_id)
                    && matches!(
                        config.trigger,
                        crate::config::types::TriggerConfig::Motion
                            | crate::config::types::TriggerConfig::MotionCleared
                    )
                {
                    debug!(
                        "Skipping automation '{}' for room '{}' — handled by built-in motion",
                        auto_id, room_id
                    );
                    continue;
                }

                let room_config = match self.config.rooms.get(room_id) {
                    Some(r) => r,
                    None => continue,
                };

                let sensor_ieee = room_config.motion_sensor.as_deref();

                if !trigger::matches_trigger(&config.trigger, &event, sensor_ieee) {
                    continue;
                }

                info!("Automation '{}' triggered for room '{}'", auto_id, room_id);

                let publisher = self.publisher.clone();
                let state_tx = self.state_tx.clone();
                let action = config.action.clone();
                let off_action = config.off_action.clone();
                let room_config_clone = room_config.clone();
                let room_id_clone = room_id.clone();
                let auto_id_clone = auto_id.clone();
                let circadian = self.circadian_engine.clone();

                tokio::spawn(async move {
                    if let Err(e) = action::execute_action(
                        &action,
                        &room_id_clone,
                        &room_config_clone,
                        &publisher,
                        &state_tx,
                        &circadian,
                    )
                    .await
                    {
                        error!("Automation '{}' action failed: {}", auto_id_clone, e);
                    }

                    if let Some(off_action) = off_action
                        && let Err(e) = action::execute_action(
                            &off_action,
                            &room_id_clone,
                            &room_config_clone,
                            &publisher,
                            &state_tx,
                            &circadian,
                        )
                        .await
                    {
                        error!("Automation '{}' off_action failed: {}", auto_id_clone, e);
                    }
                });
            }
        }
    }

    /// Handle built-in motion for rooms with motion_sensor + motion_timeout_secs.
    /// Returns the set of room IDs that were handled (so user automations can skip them).
    async fn handle_builtin_motion(&mut self, event: &Event) -> Vec<String> {
        let mut handled = Vec::new();
        let global_timeout = self.config.general.motion_timeout_secs;

        match event {
            Event::MotionDetected { sensor_ieee, .. } => {
                // Find all rooms where this sensor matches and built-in motion is configured
                let rooms: Vec<(String, crate::config::types::RoomConfig)> = self
                    .config
                    .rooms
                    .iter()
                    .filter(|(_, rc)| {
                        rc.motion_sensor.as_deref() == Some(sensor_ieee.as_str())
                            && rc.effective_motion_timeout(global_timeout).is_some()
                    })
                    .map(|(id, rc)| (id.clone(), rc.clone()))
                    .collect();

                for (room_id, room_config) in rooms {
                    // Cancel any pending off-timer
                    if let Some(handle) = self.motion_off_handles.remove(&room_id) {
                        handle.abort();
                        debug!("Cancelled pending motion-off timer for room '{}'", room_id);
                    }

                    // Only turn on if lights are currently off
                    let current = self.state.load();
                    let lights_on = current
                        .rooms
                        .get(&room_id)
                        .map(|rs| rs.lights_on)
                        .unwrap_or(false);

                    if !lights_on {
                        info!(
                            "Built-in motion: turning on lights in '{}' (circadian)",
                            room_id
                        );
                        let action = ActionConfig::LightsOn {
                            use_circadian: true,
                            brightness: None,
                            color_temp_k: None,
                            transition: None,
                        };
                        if let Err(e) = action::execute_action(
                            &action,
                            &room_id,
                            &room_config,
                            &self.publisher,
                            &self.state_tx,
                            &self.circadian_engine,
                        )
                        .await
                        {
                            error!(
                                "Built-in motion: failed to turn on lights in '{}': {}",
                                room_id, e
                            );
                        }
                    } else {
                        debug!(
                            "Built-in motion: lights already on in '{}', timer reset only",
                            room_id
                        );
                    }

                    handled.push(room_id);
                }
            }

            Event::MotionCleared { sensor_ieee, .. } => {
                let rooms: Vec<(String, crate::config::types::RoomConfig, u64)> = self
                    .config
                    .rooms
                    .iter()
                    .filter_map(|(id, rc)| {
                        if rc.motion_sensor.as_deref() == Some(sensor_ieee.as_str()) {
                            rc.effective_motion_timeout(global_timeout)
                                .map(|t| (id.clone(), rc.clone(), t))
                        } else {
                            None
                        }
                    })
                    .collect();

                for (room_id, room_config, timeout_secs) in rooms {
                    // Cancel existing off-timer
                    let had_existing = if let Some(handle) = self.motion_off_handles.remove(&room_id) {
                        handle.abort();
                        true
                    } else {
                        false
                    };

                    if had_existing {
                        debug!(
                            "Built-in motion: resetting off-timer for '{}' ({}s)",
                            room_id, timeout_secs
                        );
                    } else {
                        info!(
                            "Built-in motion: scheduling lights off in '{}' after {}s",
                            room_id, timeout_secs
                        );
                    }

                    let publisher = self.publisher.clone();
                    let state_tx = self.state_tx.clone();
                    let circadian = self.circadian_engine.clone();
                    let rid = room_id.clone();
                    let rc = room_config.clone();

                    let handle = tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;
                        info!("Built-in motion: turning off lights in '{}'", rid);
                        let off_action = ActionConfig::LightsOff {
                            delay_secs: 0,
                            transition: Some(3),
                        };
                        if let Err(e) = action::execute_action(
                            &off_action, &rid, &rc, &publisher, &state_tx, &circadian,
                        )
                        .await
                        {
                            error!(
                                "Built-in motion: failed to turn off lights in '{}': {}",
                                rid, e
                            );
                        }
                    });

                    self.motion_off_handles.insert(room_id.clone(), handle);
                    handled.push(room_id);
                }
            }

            _ => {}
        }

        handled
    }
}
