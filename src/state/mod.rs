use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Z2mDeviceInfo {
    pub ieee_address: String,
    pub friendly_name: String,
    pub supported: bool,
    pub supports_brightness: bool,
    pub supports_color_temp: bool,
    pub color_temp_min: Option<u16>,
    pub color_temp_max: Option<u16>,
    pub supports_color_xy: bool,
    pub supports_color_hs: bool,
}

/// Light type derived from Z2M device capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightType {
    /// Has color_temp AND (color_xy OR color_hs) — RGBW bulbs
    Rgbw,
    /// Has color_temp only — dedicated CT lights
    CtOnly,
    /// Has brightness only
    BrightnessOnly,
    /// None of the above (on/off only or unsupported)
    OnOff,
}

impl Z2mDeviceInfo {
    pub fn light_type(&self) -> LightType {
        if self.supports_color_temp && (self.supports_color_xy || self.supports_color_hs) {
            LightType::Rgbw
        } else if self.supports_color_temp {
            LightType::CtOnly
        } else if self.supports_brightness {
            LightType::BrightnessOnly
        } else {
            LightType::OnOff
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Z2mScene {
    pub id: u16,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Z2mGroupInfo {
    pub id: u16,
    pub friendly_name: String,
    pub members: Vec<Z2mGroupMember>,
    pub scenes: Vec<Z2mScene>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Z2mGroupMember {
    pub ieee_address: String,
    pub endpoint: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum UpdateSource {
    Circadian,
    Manual,
    Automation,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoomState {
    pub lights_on: bool,
    pub current_brightness: Option<u8>,
    pub current_color_temp_mired: Option<u16>,
    pub occupancy: bool,
    #[serde(skip)]
    pub last_motion: Option<Instant>,
    pub night_mode_active: bool,
    /// When night mode was activated (for fallback wake-time deactivation).
    #[serde(skip)]
    pub night_mode_since: Option<Instant>,
    pub update_source: UpdateSource,
    pub circadian_paused: bool,
    /// When set, circadian auto-resumes after this instant.
    #[serde(skip)]
    pub circadian_paused_until: Option<Instant>,
    /// When set, manual override expires and circadian resumes after this instant.
    #[serde(skip)]
    pub manual_override_until: Option<Instant>,
    /// When jeha last pushed light state to this room (circadian or API command).
    /// Used to distinguish jeha's own echoes from external changes.
    #[serde(skip)]
    pub last_jeha_push: Option<Instant>,
    /// Last values jeha intentionally published (for external change comparison).
    /// These are NOT updated by Z2M echo-backs, only by jeha's own publish actions.
    pub intended_brightness: Option<u8>,
    pub intended_color_temp_mired: Option<u16>,
    pub last_illuminance: Option<crate::event::Illuminance>,
}

impl Default for RoomState {
    fn default() -> Self {
        Self {
            lights_on: false,
            current_brightness: None,
            current_color_temp_mired: None,
            occupancy: false,
            last_motion: None,
            night_mode_active: false,
            night_mode_since: None,
            update_source: UpdateSource::Circadian,
            circadian_paused: false,
            circadian_paused_until: None,
            manual_override_until: None,
            last_jeha_push: None,
            intended_brightness: None,
            intended_color_temp_mired: None,
            last_illuminance: None,
        }
    }
}

impl RoomState {
    /// Returns true if circadian is effectively paused right now.
    pub fn is_circadian_paused(&self) -> bool {
        if !self.circadian_paused {
            return false;
        }
        match self.circadian_paused_until {
            Some(until) => Instant::now() < until,
            None => true,
        }
    }

    /// Returns true if a manual override is still active (TTL hasn't expired).
    pub fn is_manual_override_active(&self) -> bool {
        if self.update_source != UpdateSource::Manual {
            return false;
        }
        match self.manual_override_until {
            Some(until) => Instant::now() < until,
            None => true, // no TTL = indefinite manual override
        }
    }
}

/// Device and group maps are behind `Arc` so the per-command state clone in
/// `StateManager` is a pointer bump, not a deep copy of every device string.
/// They only change on Z2M bridge updates (rare); room state changes constantly.
#[derive(Debug, Clone, Default)]
pub struct SystemState {
    pub device_map: Arc<HashMap<String, Z2mDeviceInfo>>,
    pub group_map: Arc<HashMap<String, Z2mGroupInfo>>,
    /// Reverse index: Z2M friendly name -> IEEE address. Rebuilt with device_map.
    pub friendly_to_ieee: Arc<HashMap<String, String>>,
    /// Availability overlay (IEEE -> available) from Z2M availability topics.
    /// Kept separate from device_map so updates clone a small bool map, not
    /// every device struct. Devices absent here are assumed available.
    pub availability: Arc<HashMap<String, bool>>,
    pub rooms: HashMap<String, RoomState>,
    pub mqtt_connected: bool,
    pub z2m_online: bool,
    pub started_at: Option<Instant>,
}

impl SystemState {
    /// Whether a device is available per Z2M availability topics.
    /// Unknown devices default to available (availability may be disabled in Z2M).
    pub fn is_device_available(&self, ieee: &str) -> bool {
        self.availability.get(ieee).copied().unwrap_or(true)
    }
}

pub type SharedState = Arc<ArcSwap<SystemState>>;

pub fn new_shared_state() -> SharedState {
    Arc::new(ArcSwap::from_pointee(SystemState::default()))
}

pub enum StateCommand {
    UpdateDevices(HashMap<String, Z2mDeviceInfo>),
    UpdateGroups(HashMap<String, Z2mGroupInfo>),
    UpdateRoomState {
        room_id: String,
        update: RoomStateUpdate,
    },
    /// Availability report from a `<friendly_name>/availability` topic.
    /// Resolved to an IEEE inside the actor; reports for not-yet-known
    /// friendly names are buffered and applied on the next UpdateDevices.
    SetDeviceAvailability {
        friendly_name: String,
        available: bool,
    },
    SetMqttConnected(bool),
    SetZ2mOnline(bool),
}

pub enum RoomStateUpdate {
    LightsOn {
        brightness: Option<u8>,
        color_temp_mired: Option<u16>,
        source: UpdateSource,
    },
    LightsOff,
    Occupancy(bool),
    NightMode(bool),
    CircadianPause {
        paused: bool,
        until: Option<Instant>,
    },
    ManualOverrideTtl {
        until: Option<Instant>,
    },
    JehaPush {
        brightness: Option<u8>,
        color_temp_mired: Option<u16>,
    },
    /// Combined LightsOn + JehaPush in a single state mutation.
    /// Avoids two separate SystemState clones and prevents a race where
    /// a Z2M echo arrives between the two updates.
    LightsOnWithPush {
        brightness: Option<u8>,
        color_temp_mired: Option<u16>,
        source: UpdateSource,
    },
    ExternalChange {
        ttl_secs: u64,
    },
    Illuminance(crate::event::Illuminance),
    /// Roll back the light-related fields of a room to a previously captured
    /// snapshot after a failed MQTT publish. Only the fields the optimistic
    /// LightsOn/LightsOff/LightsOnWithPush updates touch are restored, so
    /// concurrent sensor updates (occupancy, motion, illuminance, night mode)
    /// that landed during the publish attempt are preserved.
    RestoreLights(Box<RoomState>),
}

pub struct StateManager {
    state: SharedState,
    rx: mpsc::Receiver<StateCommand>,
    event_bus: crate::event::EventBus,
    /// Availability reports whose friendly name didn't resolve to a known
    /// device yet (retained availability can arrive before bridge/devices).
    /// Applied and drained on the next UpdateDevices.
    pending_availability: HashMap<String, bool>,
}

impl StateManager {
    pub fn new(
        state: SharedState,
        event_bus: crate::event::EventBus,
    ) -> (Self, mpsc::Sender<StateCommand>) {
        let (tx, rx) = mpsc::channel(256);
        (
            Self {
                state,
                rx,
                event_bus,
                pending_availability: HashMap::new(),
            },
            tx,
        )
    }

    pub async fn run(mut self) {
        while let Some(cmd) = self.rx.recv().await {
            let mut current = (**self.state.load()).clone();
            // Availability events are published AFTER the store below, so
            // subscribers that consult SharedState (e.g. circadian's
            // per-device re-push) never observe stale availability.
            let mut availability_events: Vec<crate::event::Event> = Vec::new();
            match cmd {
                StateCommand::UpdateDevices(devices) => {
                    let friendly_to_ieee: HashMap<String, String> = devices
                        .values()
                        .map(|d| (d.friendly_name.clone(), d.ieee_address.clone()))
                        .collect();

                    // Apply availability reports that arrived before this
                    // device list; keep the ones that still don't resolve.
                    let mut availability: HashMap<String, bool> = current
                        .availability
                        .iter()
                        .filter(|(ieee, _)| devices.contains_key(*ieee))
                        .map(|(ieee, avail)| (ieee.clone(), *avail))
                        .collect();
                    self.pending_availability.retain(|friendly, avail| {
                        if let Some(ieee) = friendly_to_ieee.get(friendly) {
                            availability.insert(ieee.clone(), *avail);
                            availability_events.push(
                                crate::event::Event::DeviceAvailabilityChanged {
                                    ieee: ieee.clone(),
                                    available: *avail,
                                },
                            );
                            false
                        } else {
                            true
                        }
                    });

                    current.friendly_to_ieee = Arc::new(friendly_to_ieee);
                    current.availability = Arc::new(availability);
                    current.device_map = Arc::new(devices);
                }
                StateCommand::UpdateGroups(groups) => {
                    current.group_map = Arc::new(groups);
                }
                StateCommand::SetDeviceAvailability {
                    friendly_name,
                    available,
                } => {
                    if let Some(ieee) = current.friendly_to_ieee.get(&friendly_name) {
                        Arc::make_mut(&mut current.availability).insert(ieee.clone(), available);
                        availability_events.push(crate::event::Event::DeviceAvailabilityChanged {
                            ieee: ieee.clone(),
                            available,
                        });
                    } else {
                        tracing::debug!(
                            "Availability for unknown device '{}' buffered until device list arrives",
                            friendly_name
                        );
                        self.pending_availability.insert(friendly_name, available);
                        continue; // no state change to publish
                    }
                }
                StateCommand::UpdateRoomState { room_id, update } => {
                    let room = current.rooms.entry(room_id).or_default();
                    match update {
                        RoomStateUpdate::LightsOn {
                            brightness,
                            color_temp_mired,
                            source,
                        } => {
                            room.lights_on = true;
                            if brightness.is_some() {
                                room.current_brightness = brightness;
                            }
                            if color_temp_mired.is_some() {
                                room.current_color_temp_mired = color_temp_mired;
                            }
                            room.update_source = source;
                        }
                        RoomStateUpdate::LightsOff => {
                            room.lights_on = false;
                            room.circadian_paused = false;
                            room.circadian_paused_until = None;
                            // Keep manual_override_until and update_source so the next
                            // lights-on (e.g. from motion) can restore the user's manual
                            // brightness/CT until the TTL expires.
                        }
                        RoomStateUpdate::Occupancy(occ) => {
                            room.occupancy = occ;
                            if occ {
                                room.last_motion = Some(Instant::now());
                            }
                        }
                        RoomStateUpdate::NightMode(active) => {
                            room.night_mode_active = active;
                            room.night_mode_since = if active {
                                Some(room.night_mode_since.unwrap_or_else(Instant::now))
                            } else {
                                None
                            };
                        }
                        RoomStateUpdate::CircadianPause { paused, until } => {
                            room.circadian_paused = paused;
                            room.circadian_paused_until = until;
                        }
                        RoomStateUpdate::ManualOverrideTtl { until } => {
                            room.manual_override_until = until;
                        }
                        RoomStateUpdate::JehaPush {
                            brightness,
                            color_temp_mired,
                        } => {
                            room.last_jeha_push = Some(Instant::now());
                            if brightness.is_some() {
                                room.intended_brightness = brightness;
                            }
                            if color_temp_mired.is_some() {
                                room.intended_color_temp_mired = color_temp_mired;
                            }
                        }
                        RoomStateUpdate::LightsOnWithPush {
                            brightness,
                            color_temp_mired,
                            source,
                        } => {
                            room.lights_on = true;
                            if brightness.is_some() {
                                room.current_brightness = brightness;
                                room.intended_brightness = brightness;
                            }
                            if color_temp_mired.is_some() {
                                room.current_color_temp_mired = color_temp_mired;
                                room.intended_color_temp_mired = color_temp_mired;
                            }
                            room.update_source = source;
                            room.last_jeha_push = Some(Instant::now());
                        }
                        RoomStateUpdate::ExternalChange { ttl_secs } => {
                            room.update_source = UpdateSource::Manual;
                            room.manual_override_until =
                                Some(Instant::now() + std::time::Duration::from_secs(ttl_secs));
                        }
                        RoomStateUpdate::Illuminance(val) => {
                            room.last_illuminance = Some(val);
                        }
                        RoomStateUpdate::RestoreLights(snapshot) => {
                            room.lights_on = snapshot.lights_on;
                            room.current_brightness = snapshot.current_brightness;
                            room.current_color_temp_mired = snapshot.current_color_temp_mired;
                            // A changed manual_override_until means a concurrent
                            // manual-override update (e.g. ExternalChange) landed
                            // during the failed publish; keep its update_source so
                            // is_manual_override_active() still honors the TTL.
                            if room.manual_override_until == snapshot.manual_override_until {
                                room.update_source = snapshot.update_source;
                            }
                            room.intended_brightness = snapshot.intended_brightness;
                            room.intended_color_temp_mired = snapshot.intended_color_temp_mired;
                            room.last_jeha_push = snapshot.last_jeha_push;
                            room.circadian_paused = snapshot.circadian_paused;
                            room.circadian_paused_until = snapshot.circadian_paused_until;
                        }
                    }
                }
                StateCommand::SetMqttConnected(connected) => {
                    current.mqtt_connected = connected;
                }
                StateCommand::SetZ2mOnline(online) => {
                    current.z2m_online = online;
                }
            }
            self.state.store(Arc::new(current));
            for event in availability_events {
                self.event_bus.publish(event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_device(ieee: &str, friendly_name: &str) -> Z2mDeviceInfo {
        Z2mDeviceInfo {
            ieee_address: ieee.to_string(),
            friendly_name: friendly_name.to_string(),
            supported: true,
            supports_brightness: true,
            supports_color_temp: false,
            color_temp_min: None,
            color_temp_max: None,
            supports_color_xy: false,
            supports_color_hs: false,
        }
    }

    async fn wait_until(state: &SharedState, cond: impl Fn(&SystemState) -> bool) {
        for _ in 0..200 {
            if cond(&state.load()) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("condition not met within 1s");
    }

    #[tokio::test]
    async fn test_availability_buffered_until_device_list() {
        let state = new_shared_state();
        let event_bus = crate::event::EventBus::new(16);
        let mut event_rx = event_bus.subscribe();
        let (manager, tx) = StateManager::new(state.clone(), event_bus);
        tokio::spawn(manager.run());

        // Retained availability can arrive before the first bridge/devices
        tx.send(StateCommand::SetDeviceAvailability {
            friendly_name: "lamp".to_string(),
            available: false,
        })
        .await
        .unwrap();

        let mut devices = HashMap::new();
        devices.insert("0xAA".to_string(), make_device("0xAA", "lamp"));
        tx.send(StateCommand::UpdateDevices(devices)).await.unwrap();

        wait_until(&state, |s| !s.device_map.is_empty()).await;
        let current = state.load();
        // Buffered report was applied once the device list resolved the name
        assert!(!current.is_device_available("0xAA"));
        // Unknown devices default to available
        assert!(current.is_device_available("0xBB"));

        // The buffered report's event fires once resolved — and only after
        // the state was stored, so handlers reading state see fresh data.
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), event_rx.recv())
            .await
            .expect("no availability event within 1s")
            .unwrap();
        match event {
            crate::event::Event::DeviceAvailabilityChanged { ieee, available } => {
                assert_eq!(ieee, "0xAA");
                assert!(!available);
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_restore_lights_preserves_concurrent_manual_override() {
        let state = new_shared_state();
        let (manager, tx) = StateManager::new(state.clone(), crate::event::EventBus::new(16));
        tokio::spawn(manager.run());

        let room = "kitchen".to_string();

        // Snapshot taken before an optimistic lights-on (circadian source)
        let snapshot = RoomState::default();

        // Optimistic update, then a concurrent ExternalChange lands while
        // the publish is failing (sets Manual + a fresh override TTL)
        tx.send(StateCommand::UpdateRoomState {
            room_id: room.clone(),
            update: RoomStateUpdate::LightsOnWithPush {
                brightness: Some(200),
                color_temp_mired: Some(300),
                source: UpdateSource::Circadian,
            },
        })
        .await
        .unwrap();
        tx.send(StateCommand::UpdateRoomState {
            room_id: room.clone(),
            update: RoomStateUpdate::ExternalChange { ttl_secs: 600 },
        })
        .await
        .unwrap();

        // Rollback of the failed publish
        tx.send(StateCommand::UpdateRoomState {
            room_id: room.clone(),
            update: RoomStateUpdate::RestoreLights(Box::new(snapshot)),
        })
        .await
        .unwrap();

        wait_until(&state, |s| {
            s.rooms.get("kitchen").is_some_and(|r| !r.lights_on)
        })
        .await;
        let current = state.load();
        let rs = current.rooms.get("kitchen").unwrap();
        // Light fields rolled back...
        assert!(!rs.lights_on);
        assert_eq!(rs.intended_brightness, None);
        // ...but the concurrent manual override survives intact
        assert_eq!(rs.update_source, UpdateSource::Manual);
        assert!(rs.is_manual_override_active());
    }

    #[tokio::test]
    async fn test_availability_pruned_for_removed_devices() {
        let state = new_shared_state();
        let (manager, tx) = StateManager::new(state.clone(), crate::event::EventBus::new(16));
        tokio::spawn(manager.run());

        let mut devices = HashMap::new();
        devices.insert("0xAA".to_string(), make_device("0xAA", "lamp"));
        tx.send(StateCommand::UpdateDevices(devices)).await.unwrap();
        wait_until(&state, |s| !s.device_map.is_empty()).await;

        tx.send(StateCommand::SetDeviceAvailability {
            friendly_name: "lamp".to_string(),
            available: false,
        })
        .await
        .unwrap();
        wait_until(&state, |s| !s.is_device_available("0xAA")).await;

        // Device removed from Z2M: its availability entry is pruned
        tx.send(StateCommand::UpdateDevices(HashMap::new()))
            .await
            .unwrap();
        wait_until(&state, |s| s.device_map.is_empty()).await;
        assert!(state.load().is_device_available("0xAA"));
    }
}
