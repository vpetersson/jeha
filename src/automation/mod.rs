pub mod action;
pub mod condition;
pub mod trigger;

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::circadian::CircadianEngine;
use crate::config::types::{AppConfig, AutomationConfig};
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
    #[allow(dead_code)]
    state: SharedState,
    state_tx: mpsc::Sender<StateCommand>,
    publisher: Arc<Publisher>,
    event_bus: EventBus,
    cancel: CancellationToken,
    circadian_engine: Option<Arc<CircadianEngine>>,
    automations: HashMap<String, AutomationInfo>,
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

    async fn handle_event(&self, event: Event) {
        for (auto_id, auto_info) in &self.automations {
            if auto_info.state != AutomationState::Running {
                continue;
            }

            let config = &auto_info.config;

            for room_id in &config.rooms {
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
}
