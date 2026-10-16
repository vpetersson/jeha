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
use crate::state::{RoomStateUpdate, SharedState, StateCommand, UpdateSource};

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
    /// Per-room hold-to-dim ramps in progress. Ended on brightness stop/release.
    dimming_sessions: HashMap<String, DimSession>,
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
            dimming_sessions: HashMap::new(),
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
                result = event_rx.recv() => {
                    match result {
                        Ok(event) => self.handle_event(event).await,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            // Dropped events may include motion — log loudly so
                            // "lights stuck on/off" is diagnosable.
                            error!("Automation engine lagged, {} events dropped", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            info!("Event bus closed, automation engine shutting down");
                            return;
                        }
                    }
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
                    let manual_active = room_state.is_some_and(|rs| rs.is_manual_override_active());
                    let manual_brightness = room_state.and_then(|rs| rs.current_brightness);
                    let manual_ct_mired = room_state.and_then(|rs| rs.current_color_temp_mired);

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
                        } else if manual_active && manual_brightness.is_some() {
                            let ct_k = manual_ct_mired
                                .map(|m| 1_000_000u32 / m as u32)
                                .map(|k| k as u16);
                            info!(
                                "Built-in motion: turning on lights in '{}' (manual override: brightness={:?}, ct={:?}K)",
                                room_id, manual_brightness, ct_k
                            );
                            ActionConfig::LightsOn {
                                use_circadian: false,
                                brightness: manual_brightness,
                                color_temp_k: ct_k,
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
                                transition: Some(1),
                            }
                        };
                        let publisher = self.publisher.clone();
                        let state_tx = self.state_tx.clone();
                        let circadian = self.circadian_engine.clone();
                        let room_id_clone = room_id.clone();
                        let room_config_clone = room_config.clone();
                        let restore_manual = manual_active && manual_brightness.is_some();
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
                                return;
                            }
                            if restore_manual {
                                // execute_action set source=Automation; flip back to Manual
                                // so subsequent motion-off/on cycles still restore these values
                                // until the TTL expires.
                                let _ = state_tx
                                    .send(StateCommand::UpdateRoomState {
                                        room_id: room_id_clone.clone(),
                                        update: RoomStateUpdate::LightsOn {
                                            brightness: None,
                                            color_temp_mired: None,
                                            source: UpdateSource::Manual,
                                        },
                                    })
                                    .await;
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
                    let room_state = current.rooms.get(&room_id);
                    let is_night_mode = room_state.map(|rs| rs.night_mode_active).unwrap_or(false);
                    if is_night_mode {
                        let enm =
                            room_config.effective_night_mode(&self.config.night_mode.defaults);
                        timeout_secs = enm.motion_timeout_secs;
                    }

                    // Skip scheduling an off-timer if the lights are already off.
                    // Without this guard, every MotionCleared event after lights-out
                    // queues a redundant LightsOff publish to Z2M.
                    let lights_on = room_state.map(|rs| rs.lights_on).unwrap_or(false);
                    if !lights_on {
                        if let Some(handle) = self.motion_off_handles.remove(&room_id) {
                            handle.abort();
                        }
                        debug!(
                            "Built-in motion: ignoring clear for '{}' — lights already off",
                            room_id
                        );
                        continue;
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
            // Any non-dimming action (on/off, night mode...) supersedes a ramp.
            if !remote_action.is_dimming() {
                self.dimming_sessions.remove(room_id);
            }
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
                    self.start_continuous_dimming(room_id, room_config, true)
                        .await;
                }

                RemoteActionType::BrightnessDownHold => {
                    self.start_continuous_dimming(room_id, room_config, false)
                        .await;
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
                            &rid,
                            &rc,
                            &config,
                            &publisher,
                            &state,
                            &state_tx,
                            &event_bus,
                            crate::night_mode::NightModeTrigger::User,
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
                    self.stop_continuous_dimming(room_id, room_config).await;
                }
            }
        }
    }

    /// Single-step brightness change (click). No-op when lights are off.
    async fn step_brightness(
        &mut self,
        room_id: &str,
        room_config: &crate::config::types::RoomConfig,
        up: bool,
    ) {
        // A click during a hold ends the ramp; continue from where it got to.
        let ramp_end = self.stop_continuous_dimming(room_id, room_config).await;

        let current = self.state.load();
        let Some(room_state) = current.rooms.get(room_id).filter(|rs| rs.lights_on) else {
            return;
        };
        let step = self.config.general.remote_brightness_step;
        let current_brightness = ramp_end.unwrap_or_else(|| remote_base_brightness(room_state));
        let new_brightness = perceptual_step(current_brightness, step, up);
        if new_brightness == current_brightness {
            return; // Already at min/max
        }

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

    /// Start continuous dimming (hold) using the bulbs' native brightness ramp,
    /// so the lights fade smoothly instead of being stepped by a stream of
    /// commands. No-op when lights are off.
    async fn start_continuous_dimming(
        &mut self,
        room_id: &str,
        room_config: &crate::config::types::RoomConfig,
        up: bool,
    ) {
        // Some remotes (Hue dimmer) repeat the hold event while the button is
        // held; restarting the ramp each time would make it stutter.
        if self
            .dimming_sessions
            .get(room_id)
            .is_some_and(|s| s.up == up && s.is_running())
        {
            return;
        }
        let ramp_end = self.stop_continuous_dimming(room_id, room_config).await;

        let current = self.state.load();
        let Some(room_state) = current.rooms.get(room_id).filter(|rs| rs.lights_on) else {
            return;
        };
        let start_brightness = ramp_end.unwrap_or_else(|| remote_base_brightness(room_state));
        let rate = hold_dim_rate(self.config.general.remote_brightness_step);

        // Pause circadian and persist the manual override, as for a click.
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
        let ttl = std::time::Duration::from_secs(self.config.general.manual_override_ttl_mins * 60);
        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::ManualOverrideTtl {
                    until: Some(std::time::Instant::now() + ttl),
                },
            })
            .await;

        let signed_rate = if up { rate as i16 } else { -(rate as i16) };
        if let Err(e) = publish_brightness_move(&self.publisher, room_config, signed_rate).await {
            error!("Remote: failed to start dimming in '{}': {}", room_id, e);
            return;
        }
        debug!(
            "Remote: dimming {} in '{}' from {} at {}/s",
            if up { "up" } else { "down" },
            room_id,
            start_brightness,
            rate
        );
        self.dimming_sessions.insert(
            room_id.to_string(),
            DimSession {
                up,
                rate,
                start_brightness,
                started: std::time::Instant::now(),
            },
        );
    }

    /// Stop a hold-to-dim ramp, if one is running, and record the brightness
    /// it reached. Returns that brightness.
    async fn stop_continuous_dimming(
        &mut self,
        room_id: &str,
        room_config: &crate::config::types::RoomConfig,
    ) -> Option<u8> {
        let session = self.dimming_sessions.remove(room_id)?;
        if let Err(e) = publish_brightness_move(&self.publisher, room_config, 0).await {
            error!("Remote: failed to stop dimming in '{}': {}", room_id, e);
        }
        let reached = session.brightness_at(session.started.elapsed());
        debug!("Remote: stopped dimming in '{}' at ~{}", room_id, reached);
        // Record as a manual jeha push so the next step starts from here and
        // the Z2M echoes of the ramp aren't treated as an external change.
        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::LightsOnWithPush {
                    brightness: Some(reached),
                    color_temp_mired: None,
                    source: UpdateSource::Manual,
                },
            })
            .await;
        Some(reached)
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

        // Persist manual override across motion-off cycles via TTL.
        let ttl = std::time::Duration::from_secs(self.config.general.manual_override_ttl_mins * 60);
        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::ManualOverrideTtl {
                    until: Some(std::time::Instant::now() + ttl),
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
            return;
        }

        // execute_action set source=Automation; flip to Manual so the TTL is honored
        // by is_manual_override_active() and motion-on restores these values.
        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::LightsOn {
                    brightness: None,
                    color_temp_mired: None,
                    source: UpdateSource::Manual,
                },
            })
            .await;
    }
}

/// How long after a jeha push its intended brightness is trusted over the
/// Z2M-reported one. Echoes that arrive mid-transition (or carry per-bulb
/// calibration offsets) would otherwise make consecutive steps jump around.
const REMOTE_BASE_TRUST_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// Brightness to step from for a remote press.
fn remote_base_brightness(room_state: &crate::state::RoomState) -> u8 {
    let recent_push = room_state
        .last_jeha_push
        .is_some_and(|t| t.elapsed() < REMOTE_BASE_TRUST_WINDOW);
    let (first, second) = if recent_push {
        (
            room_state.intended_brightness,
            room_state.current_brightness,
        )
    } else {
        (
            room_state.current_brightness,
            room_state.intended_brightness,
        )
    };
    first.or(second).unwrap_or(128)
}

/// Perceived-brightness exponent. Eyes are far more sensitive to changes in
/// dim light, so a fixed raw step (e.g. 5 -> 30) looks like a flash at the low
/// end but barely registers near full brightness.
const PERCEPTUAL_GAMMA: f64 = 2.2;

/// Step brightness by `step` (out of 254) in perceptual space, so each press
/// looks like the same change whether the room is dim or bright. Always moves
/// at least one unit unless already at the limit.
fn perceptual_step(current: u8, step: u8, up: bool) -> u8 {
    let current = current.clamp(1, 254);
    let perceived = (current as f64 / 254.0).powf(1.0 / PERCEPTUAL_GAMMA);
    let delta = step as f64 / 254.0;
    let target = if up {
        perceived + delta
    } else {
        perceived - delta
    }
    .clamp(0.0, 1.0);
    let next = (target.powf(PERCEPTUAL_GAMMA) * 254.0)
        .round()
        .clamp(1.0, 254.0) as u8;
    if up {
        next.max(current.saturating_add(1)).min(254)
    } else {
        next.min(current.saturating_sub(1)).max(1)
    }
}

/// Hold-to-dim ramp speed in brightness units per second: two clicks' worth
/// per second, i.e. a full sweep in ~5s at the default step of 25.
fn hold_dim_rate(step: u8) -> u8 {
    step.saturating_mul(2).max(1)
}

/// A hold-to-dim ramp running on the bulbs.
struct DimSession {
    up: bool,
    rate: u8,
    start_brightness: u8,
    started: std::time::Instant,
}

impl DimSession {
    /// False once the ramp must have hit min/max, so a hold whose release was
    /// missed doesn't block the next hold in the same direction.
    fn is_running(&self) -> bool {
        self.started.elapsed().as_secs_f64() < 254.0 / self.rate as f64
    }

    /// Estimated brightness after the ramp has run for `elapsed`.
    fn brightness_at(&self, elapsed: std::time::Duration) -> u8 {
        let moved = self.rate as f64 * elapsed.as_secs_f64();
        let start = self.start_brightness as f64;
        let b = if self.up {
            start + moved
        } else {
            start - moved
        };
        b.round().clamp(1.0, 254.0) as u8
    }
}

/// Start (rate != 0) or stop (rate == 0) a native brightness ramp on a room.
async fn publish_brightness_move(
    publisher: &Arc<Publisher>,
    room_config: &crate::config::types::RoomConfig,
    rate: i16,
) -> anyhow::Result<()> {
    if let Some(ref group) = room_config.z2m_group {
        publisher.brightness_move_group(group, rate).await
    } else {
        for ieee in &room_config.lights {
            publisher.brightness_move_ieee(ieee, rate).await?;
        }
        Ok(())
    }
}

impl RemoteActionType {
    fn is_dimming(self) -> bool {
        matches!(
            self,
            Self::BrightnessUpStep
                | Self::BrightnessDownStep
                | Self::BrightnessUpHold
                | Self::BrightnessDownHold
                | Self::BrightnessStop
        )
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

    // --- Remote dimming math ---
    #[test]
    fn test_perceptual_step_small_at_low_end() {
        // From a dim room a press must not jump straight to ~30 (a visible flash).
        let next = perceptual_step(5, 25, true);
        assert!(next > 5 && next <= 15, "5 -> {}", next);
        // Near the top the same press is a much larger raw change.
        let bright = perceptual_step(200, 25, true);
        assert!(bright - 200 > next - 5, "200 -> {}", bright);
    }

    #[test]
    fn test_perceptual_step_monotonic_and_bounded() {
        let mut b = 1u8;
        let mut presses = 0;
        while b < 254 {
            let next = perceptual_step(b, 25, true);
            assert!(next > b);
            b = next;
            presses += 1;
        }
        assert!((9..=13).contains(&presses), "presses: {}", presses);
        assert_eq!(perceptual_step(254, 25, true), 254);
        while b > 1 {
            let next = perceptual_step(b, 25, false);
            assert!(next < b);
            b = next;
        }
        assert_eq!(perceptual_step(1, 25, false), 1);
    }

    #[test]
    fn test_perceptual_step_up_down_roughly_reversible() {
        for b in [10u8, 50, 120, 200] {
            let back = perceptual_step(perceptual_step(b, 25, true), 25, false);
            assert!((back as i16 - b as i16).abs() <= 1, "{} -> {}", b, back);
        }
    }

    #[test]
    fn test_remote_base_prefers_recent_intended() {
        let mut rs = crate::state::RoomState {
            lights_on: true,
            current_brightness: Some(60), // stale mid-transition echo
            intended_brightness: Some(100),
            last_jeha_push: Some(std::time::Instant::now()),
            ..Default::default()
        };
        assert_eq!(remote_base_brightness(&rs), 100);

        // Long after jeha's push, the reported value wins (external changes).
        rs.last_jeha_push = Some(std::time::Instant::now() - REMOTE_BASE_TRUST_WINDOW * 2);
        assert_eq!(remote_base_brightness(&rs), 60);

        rs.current_brightness = None;
        assert_eq!(remote_base_brightness(&rs), 100);
        rs.intended_brightness = None;
        assert_eq!(remote_base_brightness(&rs), 128);
    }

    #[test]
    fn test_dim_session_estimate() {
        let session = DimSession {
            up: true,
            rate: 50,
            start_brightness: 100,
            started: std::time::Instant::now(),
        };
        assert_eq!(
            session.brightness_at(std::time::Duration::from_secs(1)),
            150
        );
        assert_eq!(
            session.brightness_at(std::time::Duration::from_secs(10)),
            254
        );
        let down = DimSession {
            up: false,
            ..session
        };
        assert_eq!(
            down.brightness_at(std::time::Duration::from_millis(500)),
            75
        );
        assert_eq!(down.brightness_at(std::time::Duration::from_secs(10)), 1);
        assert!(down.is_running());
    }

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
