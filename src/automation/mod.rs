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
use crate::state::{RoomStateUpdate, SharedState, StateCommand};

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
        // Built-in remote handling: rooms with remotes get toggle/dim/night mode.
        self.handle_builtin_remote(&event).await;

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

                // Schedule gate
                if let Some(ref schedule) = config.schedule {
                    let now = crate::schedule::LocalNow::now(&self.config.general.timezone);
                    if !schedule.matches(&now) {
                        debug!(
                            "Automation '{}' schedule not active for room '{}'",
                            auto_id, room_id
                        );
                        continue;
                    }
                }

                // Conditions gate
                if !config.conditions.iter().all(|c| {
                    condition::evaluate_condition(c, &self.state)
                }) {
                    debug!(
                        "Automation '{}' conditions not met for room '{}'",
                        auto_id, room_id
                    );
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
                    // Motion schedule gate
                    if let Some(ref sched) = room_config.motion_schedule {
                        let now = crate::schedule::LocalNow::now(&self.config.general.timezone);
                        if !sched.matches(&now) {
                            debug!(
                                "Built-in motion: skipping '{}' — motion_schedule not active",
                                room_id
                            );
                            continue;
                        }
                    }

                    // Cancel any pending off-timer
                    if let Some(handle) = self.motion_off_handles.remove(&room_id) {
                        handle.abort();
                        debug!("Cancelled pending motion-off timer for room '{}'", room_id);
                    }

                    // Only turn on if lights are currently off
                    let current = self.state.load();
                    let room_state = current.rooms.get(&room_id);
                    let lights_on = room_state.map(|rs| rs.lights_on).unwrap_or(false);
                    let is_night_mode =
                        room_state.map(|rs| rs.night_mode_active).unwrap_or(false);

                    if !lights_on {
                        let action = if is_night_mode {
                            let enm = room_config
                                .effective_night_mode(&self.config.night_mode.defaults);
                            info!(
                                "Built-in motion: turning on lights in '{}' (night mode)",
                                room_id
                            );
                            ActionConfig::LightsOn {
                                use_circadian: false,
                                brightness: Some(enm.brightness),
                                color_temp_k: Some(enm.color_temp_k),
                                transition: Some(1),
                            }
                        } else {
                            info!(
                                "Built-in motion: turning on lights in '{}' (circadian)",
                                room_id
                            );
                            ActionConfig::LightsOn {
                                use_circadian: true,
                                brightness: None,
                                color_temp_k: None,
                                transition: None,
                            }
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

                for (room_id, room_config, mut timeout_secs) in rooms {
                    // Motion schedule gate
                    if let Some(ref sched) = room_config.motion_schedule {
                        let now = crate::schedule::LocalNow::now(&self.config.general.timezone);
                        if !sched.matches(&now) {
                            debug!(
                                "Built-in motion: skipping clear for '{}' — motion_schedule not active",
                                room_id
                            );
                            continue;
                        }
                    }

                    // Use night mode timeout if active
                    let current = self.state.load();
                    let is_night_mode = current
                        .rooms
                        .get(&room_id)
                        .map(|rs| rs.night_mode_active)
                        .unwrap_or(false);
                    if is_night_mode {
                        let enm = room_config
                            .effective_night_mode(&self.config.night_mode.defaults);
                        timeout_secs = enm.motion_timeout_secs;
                    }

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

    /// Handle built-in remote control actions.
    /// Matches Z2M action strings to common remote behaviors:
    /// - toggle/on/off: light control (circadian-aware)
    /// - brightness_up/down (click or move): step brightness, pause circadian
    /// - arrow_right/right: night mode on
    /// - arrow_left/left: night mode off, resume circadian
    async fn handle_builtin_remote(&mut self, event: &Event) {
        let Event::RemoteAction {
            remote_ieee,
            action,
        } = event
        else {
            return;
        };

        // Find all rooms that have this remote configured
        let rooms: Vec<(String, crate::config::types::RoomConfig)> = self
            .config
            .rooms
            .iter()
            .filter(|(_, rc)| rc.remotes.iter().any(|r| r == remote_ieee))
            .map(|(id, rc)| (id.clone(), rc.clone()))
            .collect();

        if rooms.is_empty() {
            return;
        }

        let action_lower = action.to_lowercase();
        let remote_action = classify_remote_action(&action_lower);

        let Some(remote_action) = remote_action else {
            debug!(
                "Unhandled remote action '{}' from {}",
                action, remote_ieee
            );
            return;
        };

        for (room_id, room_config) in &rooms {
            match remote_action {
                RemoteActionType::Toggle => {
                    let current = self.state.load();
                    let lights_on = current
                        .rooms
                        .get(room_id)
                        .map(|rs| rs.lights_on)
                        .unwrap_or(false);

                    if lights_on {
                        info!("Remote: toggling lights off in '{}'", room_id);
                        let off = ActionConfig::LightsOff {
                            delay_secs: 0,
                            transition: Some(1),
                        };
                        if let Err(e) = action::execute_action(
                            &off,
                            room_id,
                            room_config,
                            &self.publisher,
                            &self.state_tx,
                            &self.circadian_engine,
                        )
                        .await
                        {
                            error!("Remote: failed to turn off '{}': {}", room_id, e);
                        }
                    } else {
                        info!("Remote: toggling lights on in '{}' (circadian)", room_id);
                        let on = ActionConfig::LightsOn {
                            use_circadian: true,
                            brightness: None,
                            color_temp_k: None,
                            transition: Some(1),
                        };
                        if let Err(e) = action::execute_action(
                            &on,
                            room_id,
                            room_config,
                            &self.publisher,
                            &self.state_tx,
                            &self.circadian_engine,
                        )
                        .await
                        {
                            error!("Remote: failed to turn on '{}': {}", room_id, e);
                        }
                    }
                }

                RemoteActionType::On => {
                    info!("Remote: turning on lights in '{}' (circadian)", room_id);
                    let on = ActionConfig::LightsOn {
                        use_circadian: true,
                        brightness: None,
                        color_temp_k: None,
                        transition: Some(1),
                    };
                    if let Err(e) = action::execute_action(
                        &on,
                        room_id,
                        room_config,
                        &self.publisher,
                        &self.state_tx,
                        &self.circadian_engine,
                    )
                    .await
                    {
                        error!("Remote: failed to turn on '{}': {}", room_id, e);
                    }
                }

                RemoteActionType::Off => {
                    info!("Remote: turning off lights in '{}'", room_id);
                    let off = ActionConfig::LightsOff {
                        delay_secs: 0,
                        transition: Some(1),
                    };
                    if let Err(e) = action::execute_action(
                        &off,
                        room_id,
                        room_config,
                        &self.publisher,
                        &self.state_tx,
                        &self.circadian_engine,
                    )
                    .await
                    {
                        error!("Remote: failed to turn off '{}': {}", room_id, e);
                    }
                }

                RemoteActionType::BrightnessUp => {
                    let current = self.state.load();
                    let current_brightness = current
                        .rooms
                        .get(room_id)
                        .and_then(|rs| rs.current_brightness)
                        .unwrap_or(128);
                    let new_brightness = current_brightness.saturating_add(25).min(254);

                    debug!(
                        "Remote: brightness up in '{}': {} -> {}",
                        room_id, current_brightness, new_brightness
                    );
                    self.set_manual_brightness(room_id, room_config, new_brightness)
                        .await;
                }

                RemoteActionType::BrightnessDown => {
                    let current = self.state.load();
                    let current_brightness = current
                        .rooms
                        .get(room_id)
                        .and_then(|rs| rs.current_brightness)
                        .unwrap_or(128);
                    let new_brightness = current_brightness.saturating_sub(25).max(1);

                    debug!(
                        "Remote: brightness down in '{}': {} -> {}",
                        room_id, current_brightness, new_brightness
                    );
                    self.set_manual_brightness(room_id, room_config, new_brightness)
                        .await;
                }

                RemoteActionType::NightMode => {
                    info!("Remote: enabling night mode in '{}'", room_id);
                    if let Err(e) = crate::night_mode::activate_night_mode(
                        room_id,
                        room_config,
                        &self.config,
                        &self.publisher,
                        &self.state,
                        &self.state_tx,
                        &self.event_bus,
                    )
                    .await
                    {
                        error!(
                            "Remote: failed to activate night mode in '{}': {}",
                            room_id, e
                        );
                    }
                }

                RemoteActionType::DayMode => {
                    info!("Remote: resuming day mode in '{}'", room_id);
                    if let Err(e) = crate::night_mode::deactivate_night_mode(
                        room_id,
                        room_config,
                        &self.publisher,
                        &self.state,
                        &self.state_tx,
                        &self.event_bus,
                        &self.circadian_engine,
                    )
                    .await
                    {
                        error!(
                            "Remote: failed to deactivate night mode in '{}': {}",
                            room_id, e
                        );
                    }
                }

                RemoteActionType::BrightnessStop => {
                    // No-op: just acknowledges end of continuous dim
                    debug!("Remote: brightness stop in '{}'", room_id);
                }
            }
        }
    }

    /// Set brightness manually, pausing circadian.
    async fn set_manual_brightness(
        &self,
        room_id: &str,
        room_config: &crate::config::types::RoomConfig,
        brightness: u8,
    ) {
        // Pause circadian since this is a manual adjustment
        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::CircadianPause {
                    paused: true,
                    until: None,
                },
            })
            .await;

        let action = ActionConfig::LightsOn {
            use_circadian: false,
            brightness: Some(brightness),
            color_temp_k: None,
            transition: Some(1),
        };
        if let Err(e) = action::execute_action(
            &action,
            room_id,
            room_config,
            &self.publisher,
            &self.state_tx,
            &self.circadian_engine,
        )
        .await
        {
            error!("Remote: failed to set brightness in '{}': {}", room_id, e);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum RemoteActionType {
    Toggle,
    On,
    Off,
    BrightnessUp,
    BrightnessDown,
    BrightnessStop,
    NightMode,
    DayMode,
}

/// Classify a Z2M action string into a built-in remote action.
/// Covers IKEA, Hue, Aqara, and other common Z2M remote action names.
fn classify_remote_action(action: &str) -> Option<RemoteActionType> {
    // Toggle
    if action == "toggle" {
        return Some(RemoteActionType::Toggle);
    }

    // Explicit on/off
    if action == "on" || action == "power_on" {
        return Some(RemoteActionType::On);
    }
    if action == "off" || action == "power_off" {
        return Some(RemoteActionType::Off);
    }

    // Brightness stop/release (check before up/down so "brightness_up_release" matches stop)
    if action.contains("brightness") && (action.contains("stop") || action.contains("release")) {
        return Some(RemoteActionType::BrightnessStop);
    }

    // Brightness up (click or continuous move)
    if action.contains("brightness") && (action.contains("up") || action.contains("increase")) {
        return Some(RemoteActionType::BrightnessUp);
    }

    // Brightness down
    if action.contains("brightness") && (action.contains("down") || action.contains("decrease")) {
        return Some(RemoteActionType::BrightnessDown);
    }

    // Arrow right / right button = night mode
    if action.contains("right") && !action.contains("release") && !action.contains("stop") {
        return Some(RemoteActionType::NightMode);
    }

    // Arrow left / left button = day mode
    if action.contains("left") && !action.contains("release") && !action.contains("stop") {
        return Some(RemoteActionType::DayMode);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_toggle() {
        assert!(matches!(
            classify_remote_action("toggle"),
            Some(RemoteActionType::Toggle)
        ));
    }

    #[test]
    fn test_classify_on_off() {
        assert!(matches!(
            classify_remote_action("on"),
            Some(RemoteActionType::On)
        ));
        assert!(matches!(
            classify_remote_action("off"),
            Some(RemoteActionType::Off)
        ));
        assert!(matches!(
            classify_remote_action("power_on"),
            Some(RemoteActionType::On)
        ));
    }

    #[test]
    fn test_classify_brightness() {
        // IKEA style
        assert!(matches!(
            classify_remote_action("brightness_up_click"),
            Some(RemoteActionType::BrightnessUp)
        ));
        assert!(matches!(
            classify_remote_action("brightness_down_click"),
            Some(RemoteActionType::BrightnessDown)
        ));
        assert!(matches!(
            classify_remote_action("brightness_move_up"),
            Some(RemoteActionType::BrightnessUp)
        ));
        assert!(matches!(
            classify_remote_action("brightness_move_down"),
            Some(RemoteActionType::BrightnessDown)
        ));
        // Stop/release
        assert!(matches!(
            classify_remote_action("brightness_stop"),
            Some(RemoteActionType::BrightnessStop)
        ));
        assert!(matches!(
            classify_remote_action("brightness_up_release"),
            Some(RemoteActionType::BrightnessStop)
        ));
    }

    #[test]
    fn test_classify_arrows() {
        assert!(matches!(
            classify_remote_action("arrow_right_click"),
            Some(RemoteActionType::NightMode)
        ));
        assert!(matches!(
            classify_remote_action("arrow_left_click"),
            Some(RemoteActionType::DayMode)
        ));
        // Release should be ignored
        assert!(classify_remote_action("arrow_right_release").is_none());
        assert!(classify_remote_action("arrow_left_release").is_none());
    }

    #[test]
    fn test_classify_unknown() {
        assert!(classify_remote_action("color_temperature_move").is_none());
        assert!(classify_remote_action("recall_scene_1").is_none());
    }
}
