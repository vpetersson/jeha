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
    pub available: bool,
    pub supports_brightness: bool,
    pub supports_color_temp: bool,
    pub color_temp_min: Option<u16>,
    pub color_temp_max: Option<u16>,
    pub supports_color_xy: bool,
    pub supports_color_hs: bool,
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
    pub update_source: UpdateSource,
    pub circadian_paused: bool,
    /// When set, circadian auto-resumes after this instant.
    #[serde(skip)]
    pub circadian_paused_until: Option<Instant>,
    /// When set, manual override expires and circadian resumes after this instant.
    #[serde(skip)]
    pub manual_override_until: Option<Instant>,
    /// When jeha last pushed light state to this room (circadian or MCP command).
    /// Used to distinguish jeha's own echoes from external changes.
    #[serde(skip)]
    pub last_jeha_push: Option<Instant>,
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
            update_source: UpdateSource::Circadian,
            circadian_paused: false,
            circadian_paused_until: None,
            manual_override_until: None,
            last_jeha_push: None,
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

#[derive(Debug, Clone, Default, Serialize)]
pub struct SystemState {
    pub device_map: HashMap<String, Z2mDeviceInfo>,
    pub group_map: HashMap<String, Z2mGroupInfo>,
    pub rooms: HashMap<String, RoomState>,
    pub mqtt_connected: bool,
    pub z2m_online: bool,
    #[serde(skip)]
    pub started_at: Option<Instant>,
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
    JehaPush,
    ExternalChange,
}

pub struct StateManager {
    state: SharedState,
    rx: mpsc::Receiver<StateCommand>,
}

impl StateManager {
    pub fn new(state: SharedState) -> (Self, mpsc::Sender<StateCommand>) {
        let (tx, rx) = mpsc::channel(256);
        (Self { state, rx }, tx)
    }

    pub async fn run(mut self) {
        while let Some(cmd) = self.rx.recv().await {
            let mut current = (**self.state.load()).clone();
            match cmd {
                StateCommand::UpdateDevices(devices) => {
                    current.device_map = devices;
                }
                StateCommand::UpdateGroups(groups) => {
                    current.group_map = groups;
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
                        }
                        RoomStateUpdate::Occupancy(occ) => {
                            room.occupancy = occ;
                            if occ {
                                room.last_motion = Some(Instant::now());
                            }
                        }
                        RoomStateUpdate::NightMode(active) => {
                            room.night_mode_active = active;
                        }
                        RoomStateUpdate::CircadianPause { paused, until } => {
                            room.circadian_paused = paused;
                            room.circadian_paused_until = until;
                        }
                        RoomStateUpdate::ManualOverrideTtl { until } => {
                            room.manual_override_until = until;
                        }
                        RoomStateUpdate::JehaPush => {
                            room.last_jeha_push = Some(Instant::now());
                        }
                        RoomStateUpdate::ExternalChange => {
                            room.update_source = UpdateSource::Manual;
                            room.manual_override_until = None;
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
        }
    }
}
