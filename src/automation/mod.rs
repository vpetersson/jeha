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
use crate::event::{Event, EventBus, Illuminance};
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
    /// Per-room handles for continuous dimming (hold-to-dim). Aborted on brightness stop/release.
    dimming_handles: HashMap<String, CancellationToken>,
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
            dimming_handles: HashMap::new(),
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
                if !config
                    .conditions
                    .iter()
                    .all(|c| condition::evaluate_condition(c, &self.state))
                {
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
            Event::MotionDetected {
                sensor_ieee,
                illuminance,
                ..
            } => {
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

                    // Illuminance gate
                    if room_config.illuminance_gate {
                        let threshold = room_config.effective_illuminance_threshold(
                            self.config.general.illuminance_threshold,
                        );
                        let would_skip = match illuminance {
                            Some(Illuminance::AboveThreshold(true)) => true,
                            Some(Illuminance::Lux(lux)) => *lux >= threshold,
                            _ => false,
                        };
                        if would_skip {
                            if room_config.illuminance_log_only {
                                info!(
                                    "Built-in motion: '{}' — would skip (sufficient ambient light: {:?}, threshold: {}) [log_only]",
                                    room_id, illuminance, threshold
                                );
                            } else {
                                info!(
                                    "Built-in motion: skipping '{}' — sufficient ambient light ({:?}, threshold: {})",
                                    room_id, illuminance, threshold
                                );
                                continue;
                            }
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
                    let is_night_mode = room_state.map(|rs| rs.night_mode_active).unwrap_or(false);

                    if !lights_on {
                        let action = if is_night_mode {
                            let enm =
                                room_config.effective_night_mode(&self.config.night_mode.defaults);
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
                        let publisher = self.publisher.clone();
                        let state_tx = self.state_tx.clone();
                        let circadian = self.circadian_engine.clone();
                        let room_id_clone = room_id.clone();
                        let room_config_clone = room_config.clone();
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
                                error!(
                                    "Built-in motion: failed to turn on lights in '{}': {}",
                                    room_id_clone, e
                                );
                            }
                        });
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
                        let enm =
                            room_config.effective_night_mode(&self.config.night_mode.defaults);
                        timeout_secs = enm.motion_timeout_secs;
                    }

                    // Cancel existing off-timer
                    let had_existing =
                        if let Some(handle) = self.motion_off_handles.remove(&room_id) {
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
                            &off_action,
                            &rid,
                            &rc,
                            &publisher,
                            &state_tx,
                            &circadian,
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
            debug!("Unhandled remote action '{}' from {}", action, remote_ieee);
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

                    let action_cfg = if lights_on {
                        info!("Remote: toggling lights off in '{}'", room_id);
                        ActionConfig::LightsOff {
                            delay_secs: 0,
                            transition: Some(1),
                        }
                    } else {
                        info!("Remote: toggling lights on in '{}' (circadian)", room_id);
                        ActionConfig::LightsOn {
                            use_circadian: true,
                            brightness: None,
                            color_temp_k: None,
                            transition: Some(1),
                        }
                    };
                    let publisher = self.publisher.clone();
                    let state_tx = self.state_tx.clone();
                    let circadian = self.circadian_engine.clone();
                    let rid = room_id.clone();
                    let rc = room_config.clone();
                    tokio::spawn(async move {
                        if let Err(e) = action::execute_action(
                            &action_cfg,
                            &rid,
                            &rc,
                            &publisher,
                            &state_tx,
                            &circadian,
                        )
                        .await
                        {
                            error!("Remote: failed to execute toggle in '{}': {}", rid, e);
                        }
                    });
                }

                RemoteActionType::On => {
                    let current = self.state.load();
                    let lights_on = current
                        .rooms
                        .get(room_id)
                        .map(|rs| rs.lights_on)
                        .unwrap_or(false);

                    if lights_on {
                        debug!(
                            "Remote: lights already on in '{}', ignoring ON action",
                            room_id
                        );
                    } else {
                        info!("Remote: turning on lights in '{}' (circadian)", room_id);
                        let on = ActionConfig::LightsOn {
                            use_circadian: true,
                            brightness: None,
                            color_temp_k: None,
                            transition: Some(1),
                        };
                        let publisher = self.publisher.clone();
                        let state_tx = self.state_tx.clone();
                        let circadian = self.circadian_engine.clone();
                        let rid = room_id.clone();
                        let rc = room_config.clone();
                        tokio::spawn(async move {
                            if let Err(e) = action::execute_action(
                                &on, &rid, &rc, &publisher, &state_tx, &circadian,
                            )
                            .await
                            {
                                error!("Remote: failed to turn on '{}': {}", rid, e);
                            }
                        });
                    }
                }

                RemoteActionType::Off => {
                    info!("Remote: turning off lights in '{}'", room_id);
                    let off = ActionConfig::LightsOff {
                        delay_secs: 0,
                        transition: Some(1),
                    };
                    let publisher = self.publisher.clone();
                    let state_tx = self.state_tx.clone();
                    let circadian = self.circadian_engine.clone();
                    let rid = room_id.clone();
                    let rc = room_config.clone();
                    tokio::spawn(async move {
                        if let Err(e) = action::execute_action(
                            &off, &rid, &rc, &publisher, &state_tx, &circadian,
                        )
                        .await
                        {
                            error!("Remote: failed to turn off '{}': {}", rid, e);
                        }
                    });
                }

                RemoteActionType::BrightnessUpStep => {
                    self.step_brightness(room_id, room_config, true).await;
                }

                RemoteActionType::BrightnessDownStep => {
                    self.step_brightness(room_id, room_config, false).await;
                }

                RemoteActionType::BrightnessUpHold => {
                    self.start_continuous_dimming(room_id.clone(), room_config.clone(), true);
                }

                RemoteActionType::BrightnessDownHold => {
                    self.start_continuous_dimming(room_id.clone(), room_config.clone(), false);
                }

                RemoteActionType::NightMode => {
                    info!("Remote: enabling night mode in '{}'", room_id);
                    let rid = room_id.clone();
                    let rc = room_config.clone();
                    let config = self.config.clone();
                    let publisher = self.publisher.clone();
                    let state = self.state.clone();
                    let state_tx = self.state_tx.clone();
                    let event_bus = self.event_bus.clone();
                    tokio::spawn(async move {
                        if let Err(e) = crate::night_mode::activate_night_mode(
                            &rid, &rc, &config, &publisher, &state, &state_tx, &event_bus,
                        )
                        .await
                        {
                            error!("Remote: failed to activate night mode in '{}': {}", rid, e);
                        }
                    });
                }

                RemoteActionType::DayMode => {
                    info!("Remote: resuming day mode in '{}'", room_id);
                    let rid = room_id.clone();
                    let rc = room_config.clone();
                    let publisher = self.publisher.clone();
                    let state = self.state.clone();
                    let state_tx = self.state_tx.clone();
                    let event_bus = self.event_bus.clone();
                    let circadian = self.circadian_engine.clone();
                    tokio::spawn(async move {
                        if let Err(e) = crate::night_mode::deactivate_night_mode(
                            &rid, &rc, &publisher, &state, &state_tx, &event_bus, &circadian,
                        )
                        .await
                        {
                            error!(
                                "Remote: failed to deactivate night mode in '{}': {}",
                                rid, e
                            );
                        }
                    });
                }

                RemoteActionType::BrightnessStop => {
                    if let Some(token) = self.dimming_handles.remove(room_id) {
                        token.cancel();
                        debug!("Remote: stopped continuous dimming in '{}'", room_id);
                    }
                }
            }
        }
    }

    /// Single-step brightness change (click). No-op when lights are off.
    async fn step_brightness(
        &self,
        room_id: &str,
        room_config: &crate::config::types::RoomConfig,
        up: bool,
    ) {
        let current = self.state.load();
        let lights_on = current
            .rooms
            .get(room_id)
            .map(|rs| rs.lights_on)
            .unwrap_or(false);
        if !lights_on {
            return;
        }
        let step = self.config.general.remote_brightness_step;
        let current_brightness = current
            .rooms
            .get(room_id)
            .and_then(|rs| rs.current_brightness)
            .unwrap_or(128);
        let new_brightness = if up {
            current_brightness.saturating_add(step).min(254)
        } else {
            current_brightness.saturating_sub(step).max(1)
        };

        debug!(
            "Remote: brightness {} in '{}': {} -> {}",
            if up { "up" } else { "down" },
            room_id,
            current_brightness,
            new_brightness
        );
        self.set_manual_brightness(room_id, room_config, new_brightness)
            .await;
    }

    /// Start continuous dimming (hold). Repeats brightness steps every 300ms until cancelled.
    /// No-op when lights are off.
    fn start_continuous_dimming(
        &mut self,
        room_id: String,
        room_config: crate::config::types::RoomConfig,
        up: bool,
    ) {
        let lights_on = self
            .state
            .load()
            .rooms
            .get(&room_id)
            .map(|rs| rs.lights_on)
            .unwrap_or(false);
        if !lights_on {
            return;
        }

        // Cancel any existing dimming for this room
        if let Some(token) = self.dimming_handles.remove(&room_id) {
            token.cancel();
        }

        let token = CancellationToken::new();
        self.dimming_handles.insert(room_id.clone(), token.clone());

        let state = self.state.clone();
        let state_tx = self.state_tx.clone();
        let publisher = self.publisher.clone();
        let circadian_engine = self.circadian_engine.clone();
        let step = self.config.general.remote_brightness_step;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(300));
            // First tick is immediate — pause circadian once up front
            let _ = state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.clone(),
                    update: RoomStateUpdate::CircadianPause {
                        paused: true,
                        until: None,
                    },
                })
                .await;

            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {
                        let current_brightness = state
                            .load()
                            .rooms
                            .get(&room_id)
                            .and_then(|rs| rs.current_brightness)
                            .unwrap_or(128);
                        let new_brightness = if up {
                            current_brightness.saturating_add(step).min(254)
                        } else {
                            current_brightness.saturating_sub(step).max(1)
                        };

                        if new_brightness == current_brightness {
                            break; // Hit min/max
                        }

                        let action = ActionConfig::LightsOn {
                            use_circadian: false,
                            brightness: Some(new_brightness),
                            color_temp_k: None,
                            transition: Some(0),
                        };
                        if let Err(e) = action::execute_action(
                            &action,
                            &room_id,
                            &room_config,
                            &publisher,
                            &state_tx,
                            &circadian_engine,
                        )
                        .await
                        {
                            error!("Remote: continuous dim failed in '{}': {}", room_id, e);
                            break;
                        }
                    }
                }
            }
        });
    }

    /// Set brightness manually, pausing circadian. No-op when lights are off.
    async fn set_manual_brightness(
        &self,
        room_id: &str,
        room_config: &crate::config::types::RoomConfig,
        brightness: u8,
    ) {
        let lights_on = self
            .state
            .load()
            .rooms
            .get(room_id)
            .map(|rs| rs.lights_on)
            .unwrap_or(false);
        if !lights_on {
            return;
        }

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

#[derive(Debug, Clone, Copy, PartialEq)]
enum RemoteActionType {
    Toggle,
    On,
    Off,
    /// Single-step brightness increase (click).
    BrightnessUpStep,
    /// Start continuous brightness increase (hold/move).
    BrightnessUpHold,
    /// Single-step brightness decrease (click).
    BrightnessDownStep,
    /// Start continuous brightness decrease (hold/move).
    BrightnessDownHold,
    BrightnessStop,
    NightMode,
    DayMode,
}

/// Classify a Z2M action string into a built-in remote action.
///
/// Tested against:
/// - IKEA: STYRBAR (E2001/E2002/E2313), TRADFRI remote (E1524/E1810),
///   RODRET (E2201), ON/OFF switch (E1743)
/// - Philips Hue: Dimmer switch v1/v2, Smart button, Wall switch module
/// - Aqara: Mini switch (WXKG11LM)
/// - Sonoff: SNZB-01/SNZB-01P
/// - Tuya: TS004F, scene switches
///
/// Multi-button devices (SOMRIG, Aqara Opple, Tuya TS0042+) use numbered
/// action prefixes (e.g. "1_single", "button_2_hold") which are not mapped
/// here — they require user-defined automations.
fn classify_remote_action(action: &str) -> Option<RemoteActionType> {
    // --- Release/stop events (check first to avoid false matches) ---

    // Brightness stop: brightness_stop, brightness_up_release, brightness_down_release
    if action.contains("brightness") && (action.contains("stop") || action.contains("release")) {
        return Some(RemoteActionType::BrightnessStop);
    }

    // Hue dimmer: up_press_release, up_hold_release, down_press_release, down_hold_release
    if (action.starts_with("up_") || action.starts_with("down_")) && action.ends_with("_release") {
        return Some(RemoteActionType::BrightnessStop);
    }

    // All other *_release / *_hold_release events — no-op
    if action.ends_with("_release") {
        return None;
    }

    // --- Toggle ---
    // IKEA TRADFRI remote: "toggle", Tuya knobs: "toggle"
    // Aqara mini / Sonoff / Tuya 1-button: "single"
    // "toggle_hold" falls through to None
    if action == "toggle" || action == "single" {
        return Some(RemoteActionType::Toggle);
    }

    // --- On/Off ---
    // IKEA STYRBAR/RODRET/E1743: "on", "off"
    // Hue smart button: "on", "off"
    if action == "on" || action == "power_on" || action == "on_press" {
        return Some(RemoteActionType::On);
    }
    if action == "off" || action == "power_off" || action == "off_press" {
        return Some(RemoteActionType::Off);
    }

    // --- Brightness step (single press) ---
    // IKEA TRADFRI: brightness_up_click, brightness_down_click
    // Tuya knobs: brightness_step_up, brightness_step_down
    // Hue dimmer: up_press, down_press
    if action.contains("brightness") && (action.contains("up") || action.contains("increase")) {
        if action.contains("click") || action.contains("step") {
            return Some(RemoteActionType::BrightnessUpStep);
        }
        // brightness_move_up (IKEA), brightness_up_hold (TRADFRI) = continuous
        return Some(RemoteActionType::BrightnessUpHold);
    }
    if action.contains("brightness") && (action.contains("down") || action.contains("decrease")) {
        if action.contains("click") || action.contains("step") {
            return Some(RemoteActionType::BrightnessDownStep);
        }
        return Some(RemoteActionType::BrightnessDownHold);
    }

    // Hue dimmer: up_press = brightness step, up_hold = continuous
    if action == "up_press" {
        return Some(RemoteActionType::BrightnessUpStep);
    }
    if action == "down_press" {
        return Some(RemoteActionType::BrightnessDownStep);
    }
    if action == "up_hold" {
        return Some(RemoteActionType::BrightnessUpHold);
    }
    if action == "down_hold" {
        return Some(RemoteActionType::BrightnessDownHold);
    }

    // --- Night mode / Day mode ---
    // IKEA STYRBAR/TRADFRI: arrow_right_click = night, arrow_left_click = day
    if action == "arrow_right_click" {
        return Some(RemoteActionType::NightMode);
    }
    if action == "arrow_left_click" {
        return Some(RemoteActionType::DayMode);
    }

    // Hue dimmer on_hold = night mode, off_hold = day mode
    // (long-press on/off as secondary function)
    if action == "on_hold" {
        return Some(RemoteActionType::NightMode);
    }
    if action == "off_hold" {
        return Some(RemoteActionType::DayMode);
    }

    // Aqara mini: hold = night mode toggle (single button, use hold for night mode)
    if action == "hold" || action == "long" {
        return Some(RemoteActionType::NightMode);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- IKEA STYRBAR (E2001/E2002/E2313) ---
    #[test]
    fn test_styrbar() {
        assert_eq!(classify_remote_action("on"), Some(RemoteActionType::On));
        assert_eq!(classify_remote_action("off"), Some(RemoteActionType::Off));
        assert_eq!(
            classify_remote_action("brightness_move_up"),
            Some(RemoteActionType::BrightnessUpHold)
        );
        assert_eq!(
            classify_remote_action("brightness_move_down"),
            Some(RemoteActionType::BrightnessDownHold)
        );
        assert_eq!(
            classify_remote_action("brightness_stop"),
            Some(RemoteActionType::BrightnessStop)
        );
        assert_eq!(
            classify_remote_action("arrow_right_click"),
            Some(RemoteActionType::NightMode)
        );
        assert_eq!(
            classify_remote_action("arrow_left_click"),
            Some(RemoteActionType::DayMode)
        );
        assert_eq!(classify_remote_action("arrow_right_hold"), None);
        assert_eq!(classify_remote_action("arrow_left_hold"), None);
        assert_eq!(classify_remote_action("arrow_right_release"), None);
        assert_eq!(classify_remote_action("arrow_left_release"), None);
    }

    // --- IKEA TRADFRI remote (E1524/E1810) ---
    #[test]
    fn test_tradfri_remote() {
        assert_eq!(
            classify_remote_action("toggle"),
            Some(RemoteActionType::Toggle)
        );
        assert_eq!(classify_remote_action("toggle_hold"), None);
        assert_eq!(
            classify_remote_action("brightness_up_click"),
            Some(RemoteActionType::BrightnessUpStep)
        );
        assert_eq!(
            classify_remote_action("brightness_down_click"),
            Some(RemoteActionType::BrightnessDownStep)
        );
        assert_eq!(
            classify_remote_action("brightness_up_hold"),
            Some(RemoteActionType::BrightnessUpHold)
        );
        assert_eq!(
            classify_remote_action("brightness_down_hold"),
            Some(RemoteActionType::BrightnessDownHold)
        );
        assert_eq!(
            classify_remote_action("brightness_up_release"),
            Some(RemoteActionType::BrightnessStop)
        );
        assert_eq!(
            classify_remote_action("brightness_down_release"),
            Some(RemoteActionType::BrightnessStop)
        );
        assert_eq!(
            classify_remote_action("arrow_right_click"),
            Some(RemoteActionType::NightMode)
        );
        assert_eq!(
            classify_remote_action("arrow_left_click"),
            Some(RemoteActionType::DayMode)
        );
    }

    // --- IKEA RODRET dimmer (E2201) ---
    #[test]
    fn test_rodret() {
        assert_eq!(classify_remote_action("on"), Some(RemoteActionType::On));
        assert_eq!(classify_remote_action("off"), Some(RemoteActionType::Off));
        assert_eq!(
            classify_remote_action("brightness_move_up"),
            Some(RemoteActionType::BrightnessUpHold)
        );
        assert_eq!(
            classify_remote_action("brightness_move_down"),
            Some(RemoteActionType::BrightnessDownHold)
        );
        assert_eq!(
            classify_remote_action("brightness_stop"),
            Some(RemoteActionType::BrightnessStop)
        );
    }

    // --- IKEA ON/OFF switch (E1743) ---
    #[test]
    fn test_ikea_onoff_switch() {
        assert_eq!(classify_remote_action("on"), Some(RemoteActionType::On));
        assert_eq!(classify_remote_action("off"), Some(RemoteActionType::Off));
        assert_eq!(
            classify_remote_action("brightness_move_up"),
            Some(RemoteActionType::BrightnessUpHold)
        );
        assert_eq!(
            classify_remote_action("brightness_move_down"),
            Some(RemoteActionType::BrightnessDownHold)
        );
        assert_eq!(
            classify_remote_action("brightness_stop"),
            Some(RemoteActionType::BrightnessStop)
        );
    }

    // --- Philips Hue dimmer switch v1/v2 ---
    #[test]
    fn test_hue_dimmer() {
        assert_eq!(
            classify_remote_action("on_press"),
            Some(RemoteActionType::On)
        );
        assert_eq!(
            classify_remote_action("off_press"),
            Some(RemoteActionType::Off)
        );
        assert_eq!(
            classify_remote_action("up_press"),
            Some(RemoteActionType::BrightnessUpStep)
        );
        assert_eq!(
            classify_remote_action("down_press"),
            Some(RemoteActionType::BrightnessDownStep)
        );
        assert_eq!(
            classify_remote_action("up_hold"),
            Some(RemoteActionType::BrightnessUpHold)
        );
        assert_eq!(
            classify_remote_action("down_hold"),
            Some(RemoteActionType::BrightnessDownHold)
        );
        assert_eq!(
            classify_remote_action("up_press_release"),
            Some(RemoteActionType::BrightnessStop)
        );
        assert_eq!(
            classify_remote_action("up_hold_release"),
            Some(RemoteActionType::BrightnessStop)
        );
        assert_eq!(
            classify_remote_action("down_press_release"),
            Some(RemoteActionType::BrightnessStop)
        );
        assert_eq!(
            classify_remote_action("down_hold_release"),
            Some(RemoteActionType::BrightnessStop)
        );
        // Long-press on/off = night/day mode
        assert_eq!(
            classify_remote_action("on_hold"),
            Some(RemoteActionType::NightMode)
        );
        assert_eq!(
            classify_remote_action("off_hold"),
            Some(RemoteActionType::DayMode)
        );
        // Release events for on/off are no-ops
        assert_eq!(classify_remote_action("on_press_release"), None);
        assert_eq!(classify_remote_action("on_hold_release"), None);
        assert_eq!(classify_remote_action("off_press_release"), None);
        assert_eq!(classify_remote_action("off_hold_release"), None);
    }

    // --- Aqara mini switch (WXKG11LM) ---
    #[test]
    fn test_aqara_mini() {
        assert_eq!(
            classify_remote_action("single"),
            Some(RemoteActionType::Toggle)
        );
        assert_eq!(
            classify_remote_action("hold"),
            Some(RemoteActionType::NightMode)
        );
        assert_eq!(classify_remote_action("release"), None);
        assert_eq!(classify_remote_action("double"), None);
    }

    // --- Sonoff SNZB-01 ---
    #[test]
    fn test_sonoff_button() {
        assert_eq!(
            classify_remote_action("single"),
            Some(RemoteActionType::Toggle)
        );
        assert_eq!(
            classify_remote_action("long"),
            Some(RemoteActionType::NightMode)
        );
        assert_eq!(classify_remote_action("double"), None);
    }

    // --- Tuya knobs (TS004F) ---
    #[test]
    fn test_tuya_knob() {
        assert_eq!(
            classify_remote_action("toggle"),
            Some(RemoteActionType::Toggle)
        );
        assert_eq!(
            classify_remote_action("brightness_step_up"),
            Some(RemoteActionType::BrightnessUpStep)
        );
        assert_eq!(
            classify_remote_action("brightness_step_down"),
            Some(RemoteActionType::BrightnessDownStep)
        );
        assert_eq!(
            classify_remote_action("brightness_move_up"),
            Some(RemoteActionType::BrightnessUpHold)
        );
        assert_eq!(
            classify_remote_action("brightness_move_down"),
            Some(RemoteActionType::BrightnessDownHold)
        );
        assert_eq!(
            classify_remote_action("brightness_stop"),
            Some(RemoteActionType::BrightnessStop)
        );
    }

    // --- Multi-button devices should not match (user automations) ---
    #[test]
    fn test_multi_button_ignored() {
        // SOMRIG
        assert_eq!(classify_remote_action("1_short_release"), None);
        assert_eq!(classify_remote_action("2_long_press"), None);
        // Aqara Opple
        assert_eq!(classify_remote_action("button_1_single"), None);
        assert_eq!(classify_remote_action("button_3_hold"), None);
        // Tuya multi-button
        assert_eq!(classify_remote_action("1_single"), None);
        assert_eq!(classify_remote_action("2_double"), None);
    }

    #[test]
    fn test_classify_unknown() {
        assert_eq!(classify_remote_action("color_temperature_move"), None);
        assert_eq!(classify_remote_action("recall_scene_1"), None);
        assert_eq!(classify_remote_action("recall_0"), None);
    }
}
