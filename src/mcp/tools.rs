use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::types::AppConfig;
use crate::mqtt::publish::Publisher;
use crate::state::{RoomStateUpdate, SharedState, StateCommand, UpdateSource};

#[derive(Clone)]
pub struct McpToolHandler {
    state: SharedState,
    state_tx: mpsc::Sender<StateCommand>,
    publisher: Arc<Publisher>,
    config: Arc<AppConfig>,
}

impl McpToolHandler {
    pub fn new(
        state: SharedState,
        state_tx: mpsc::Sender<StateCommand>,
        publisher: Arc<Publisher>,
        config: Arc<AppConfig>,
    ) -> Self {
        Self {
            state,
            state_tx,
            publisher,
            config,
        }
    }

    pub fn tool_definitions(&self) -> Vec<Value> {
        vec![
            json!({
                "name": "get_rooms",
                "description": "Get a summary of all rooms in the house with their current lighting state. Shows which lights are on/off, brightness levels, color temperature, circadian status (active/paused/snoozed), occupancy from motion sensors, and night mode. Use this to get the big picture before making changes.",
                "inputSchema": { "type": "object", "properties": {} }
            }),
            json!({
                "name": "get_room",
                "description": "Get detailed state for a specific room including light state, circadian rhythm status, and configuration. Use room_id (the short key like 'kitchen', 'living_room', 'hallway_downstairs').",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "room_id": { "type": "string", "description": "Room identifier (e.g. 'kitchen', 'living_room', 'master_bedroom')" }
                    },
                    "required": ["room_id"]
                }
            }),
            json!({
                "name": "light_on",
                "description": "Turn on lights in a room. If you specify brightness or color_temp_k, it becomes a manual override and circadian stops adjusting. Use override_ttl_mins to make the override auto-expire (e.g. 120 = circadian resumes in 2 hours). Without manual values, lights turn on at the current circadian target. Brightness: 1-254 (254=max). Color temp: 2000-6500K (lower=warmer/orange, higher=cooler/blue).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "room_id": { "type": "string", "description": "Room identifier" },
                        "brightness": { "type": "integer", "minimum": 1, "maximum": 254, "description": "Brightness level. 254=full, 127=half, 20=very dim nightlight" },
                        "color_temp_k": { "type": "integer", "minimum": 2000, "maximum": 6500, "description": "Color temperature in Kelvin. 2200=warm candlelight, 2700=soft white, 4000=neutral, 5500=daylight, 6500=cool blue" },
                        "transition": { "type": "integer", "description": "Transition time in seconds (default: 3)" },
                        "override_ttl_mins": { "type": "number", "description": "Auto-expire the manual override after this many minutes and resume circadian (e.g. 30, 60, 120). Omit for indefinite override." }
                    },
                    "required": ["room_id"]
                }
            }),
            json!({
                "name": "light_off",
                "description": "Turn off lights in a room. This does NOT affect the circadian pause state — if circadian was paused, it stays paused for when lights come back on.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "room_id": { "type": "string", "description": "Room identifier" },
                        "transition": { "type": "integer", "description": "Fade-out time in seconds (default: 3)" }
                    },
                    "required": ["room_id"]
                }
            }),
            json!({
                "name": "pause_circadian",
                "description": "Pause the circadian rhythm for a room indefinitely. Lights stay at their current brightness and color temperature — jeha stops adjusting them. Use this for parties, movie night, or when someone wants manual control. The lights stay on as-is; this just stops the automatic adjustments. Use resume_circadian to re-enable.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "room_id": { "type": "string", "description": "Room identifier" }
                    },
                    "required": ["room_id"]
                }
            }),
            json!({
                "name": "resume_circadian",
                "description": "Resume the circadian rhythm for a room. Lights will smoothly transition back to the current circadian target (appropriate brightness and color temp for the time of day). Also clears any manual override.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "room_id": { "type": "string", "description": "Room identifier" }
                    },
                    "required": ["room_id"]
                }
            }),
            json!({
                "name": "snooze_circadian",
                "description": "Pause the circadian rhythm for a room for a specific duration, then auto-resume. Perfect for 'keep the lights like this for the next 2 hours' or 'party mode for 4 hours'. When the snooze expires, circadian smoothly resumes.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "room_id": { "type": "string", "description": "Room identifier" },
                        "hours": { "type": "number", "description": "How many hours to snooze (supports decimals, e.g. 1.5 for 90 minutes)" }
                    },
                    "required": ["room_id", "hours"]
                }
            }),
            json!({
                "name": "set_night_mode",
                "description": "Toggle night mode for a room. Night mode uses very dim, warm lighting (brightness ~20, 2200K) — enough to see but not wake anyone up.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "room_id": { "type": "string", "description": "Room identifier" },
                        "active": { "type": "boolean", "description": "true to enable night mode, false to disable" }
                    },
                    "required": ["room_id", "active"]
                }
            }),
            json!({
                "name": "set_scene",
                "description": "Set a predefined lighting scene for a room. This pauses circadian and sets specific brightness and color temperature. Available scenes: 'bright' (full brightness, neutral white), 'relax' (dimmer, warm), 'movie' (very dim, warmest), 'energize' (bright, cool daylight), 'nightlight' (minimal, warm amber). Use override_ttl_mins to auto-resume circadian after a duration.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "room_id": { "type": "string", "description": "Room identifier" },
                        "scene": { "type": "string", "enum": ["bright", "relax", "movie", "energize", "nightlight"], "description": "Scene name" },
                        "override_ttl_mins": { "type": "number", "description": "Auto-resume circadian after this many minutes. Omit for indefinite." }
                    },
                    "required": ["room_id", "scene"]
                }
            }),
            json!({
                "name": "list_z2m_scenes",
                "description": "List all Z2M scenes available for a room's group. These are scenes stored on the Zigbee devices themselves (created via the Z2M frontend). Returns scene IDs and names that can be used with recall_z2m_scene.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "room_id": { "type": "string", "description": "Room identifier" }
                    },
                    "required": ["room_id"]
                }
            }),
            json!({
                "name": "recall_z2m_scene",
                "description": "Activate a Z2M scene stored on the devices in a room's group. This sends a scene_recall command to Z2M and pauses circadian so jeha doesn't overwrite the scene. Use list_z2m_scenes to see available scenes first. Use override_ttl_mins to auto-resume circadian after a duration.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "room_id": { "type": "string", "description": "Room identifier" },
                        "scene_id": { "type": "integer", "description": "Z2M scene ID (from list_z2m_scenes)" },
                        "override_ttl_mins": { "type": "number", "description": "Auto-resume circadian after this many minutes. Omit for indefinite." }
                    },
                    "required": ["room_id", "scene_id"]
                }
            }),
            json!({
                "name": "get_circadian_status",
                "description": "Show the current circadian rhythm state for all rooms: what the target brightness and color temp should be right now, whether circadian is active/paused/snoozed per room, and time until any snoozes expire.",
                "inputSchema": { "type": "object", "properties": {} }
            }),
            json!({
                "name": "get_system_status",
                "description": "Get system health: MQTT connection status, Zigbee2MQTT online status, uptime, and counts of discovered devices/groups/rooms.",
                "inputSchema": { "type": "object", "properties": {} }
            }),
            json!({
                "name": "reload_config",
                "description": "Trigger a config reload from disk. Validates the new config first — if invalid, keeps running with the old config.",
                "inputSchema": { "type": "object", "properties": {} }
            }),
        ]
    }

    pub async fn call_tool(&self, name: &str, args: &Value) -> Result<Value, String> {
        match name {
            "get_rooms" => Ok(self.get_rooms()),
            "get_room" => {
                let room_id = args["room_id"].as_str().ok_or("room_id is required")?;
                Ok(self.get_room(room_id))
            }
            "light_on" => {
                let room_id = args["room_id"].as_str().ok_or("room_id is required")?;
                let brightness = args["brightness"].as_u64().map(|b| b as u8);
                let color_temp_k = args["color_temp_k"].as_u64().map(|c| c as u16);
                let transition = args["transition"].as_u64().map(|t| t as u32);
                let override_ttl_mins = args["override_ttl_mins"].as_f64();
                self.light_on(
                    room_id,
                    brightness,
                    color_temp_k,
                    transition,
                    override_ttl_mins,
                )
                .await
            }
            "light_off" => {
                let room_id = args["room_id"].as_str().ok_or("room_id is required")?;
                let transition = args["transition"].as_u64().map(|t| t as u32);
                self.light_off(room_id, transition).await
            }
            "pause_circadian" => {
                let room_id = args["room_id"].as_str().ok_or("room_id is required")?;
                self.pause_circadian(room_id).await
            }
            "resume_circadian" => {
                let room_id = args["room_id"].as_str().ok_or("room_id is required")?;
                self.resume_circadian(room_id).await
            }
            "snooze_circadian" => {
                let room_id = args["room_id"].as_str().ok_or("room_id is required")?;
                let hours = args["hours"].as_f64().ok_or("hours is required (number)")?;
                self.snooze_circadian(room_id, hours).await
            }
            "set_night_mode" => {
                let room_id = args["room_id"].as_str().ok_or("room_id is required")?;
                let active = args["active"].as_bool().ok_or("active is required")?;
                self.set_night_mode(room_id, active).await;
                Ok(json!({"status": "ok", "room": room_id, "night_mode": active}))
            }
            "set_scene" => {
                let room_id = args["room_id"].as_str().ok_or("room_id is required")?;
                let scene = args["scene"].as_str().ok_or("scene is required")?;
                let override_ttl_mins = args["override_ttl_mins"].as_f64();
                self.set_scene(room_id, scene, override_ttl_mins).await
            }
            "list_z2m_scenes" => {
                let room_id = args["room_id"].as_str().ok_or("room_id is required")?;
                self.list_z2m_scenes(room_id)
            }
            "recall_z2m_scene" => {
                let room_id = args["room_id"].as_str().ok_or("room_id is required")?;
                let scene_id = args["scene_id"]
                    .as_u64()
                    .ok_or("scene_id is required (integer)")? as u16;
                let override_ttl_mins = args["override_ttl_mins"].as_f64();
                self.recall_z2m_scene(room_id, scene_id, override_ttl_mins)
                    .await
            }
            "get_circadian_status" => Ok(self.get_circadian_status()),
            "get_system_status" => Ok(self.get_system_status()),
            "reload_config" => Ok(json!({"status": "reload not yet implemented at runtime"})),
            _ => Err(format!("Unknown tool: {}", name)),
        }
    }

    // --- Room queries ---

    fn get_rooms(&self) -> Value {
        let current = self.state.load();
        let mut rooms: Vec<Value> = current
            .rooms
            .iter()
            .map(|(room_id, rs)| {
                let display_name = self
                    .config
                    .rooms
                    .get(room_id)
                    .and_then(|r| r.display_name.as_deref())
                    .unwrap_or(room_id);
                let circadian_status = self.describe_circadian_status(rs);
                json!({
                    "room_id": room_id,
                    "display_name": display_name,
                    "lights_on": rs.lights_on,
                    "brightness": rs.current_brightness,
                    "color_temp_mired": rs.current_color_temp_mired,
                    "occupancy": rs.occupancy,
                    "night_mode": rs.night_mode_active,
                    "circadian": circadian_status,
                })
            })
            .collect();
        rooms.sort_by(|a, b| a["display_name"].as_str().cmp(&b["display_name"].as_str()));
        json!(rooms)
    }

    fn get_room(&self, room_id: &str) -> Value {
        let current = self.state.load();
        match current.rooms.get(room_id) {
            Some(rs) => {
                let room_config = self.config.rooms.get(room_id);
                let display_name = room_config
                    .and_then(|r| r.display_name.as_deref())
                    .unwrap_or(room_id);
                let z2m_group = room_config.and_then(|r| r.z2m_group.as_deref());
                let has_motion = room_config
                    .map(|r| r.motion_sensor.is_some())
                    .unwrap_or(false);
                let circadian_status = self.describe_circadian_status(rs);

                json!({
                    "room_id": room_id,
                    "display_name": display_name,
                    "z2m_group": z2m_group,
                    "lights_on": rs.lights_on,
                    "brightness": rs.current_brightness,
                    "color_temp_mired": rs.current_color_temp_mired,
                    "occupancy": rs.occupancy,
                    "has_motion_sensor": has_motion,
                    "night_mode": rs.night_mode_active,
                    "circadian": circadian_status,
                })
            }
            None => {
                json!({"error": format!("Room '{}' not found. Use get_rooms to see available room IDs.", room_id)})
            }
        }
    }

    fn describe_circadian_status(&self, rs: &crate::state::RoomState) -> Value {
        // Check manual override with TTL first
        if rs.update_source == UpdateSource::Manual {
            if let Some(until) = rs.manual_override_until {
                let now = Instant::now();
                if now < until {
                    let remaining = until - now;
                    let mins = remaining.as_secs() / 60;
                    return json!({
                        "status": "manual_override",
                        "remaining_mins": mins,
                        "description": format!("Manual override active — circadian auto-resumes in {}m", mins)
                    });
                }
            } else if !rs.circadian_paused {
                return json!({
                    "status": "manual_override",
                    "description": "Manual override active indefinitely — use resume_circadian to re-enable"
                });
            }
        }

        if !rs.circadian_paused {
            return json!({
                "status": "active",
                "description": "Circadian rhythm is active — lights adjust automatically"
            });
        }

        match rs.circadian_paused_until {
            Some(until) => {
                let now = Instant::now();
                if now >= until {
                    json!({
                        "status": "active",
                        "description": "Circadian snooze has expired — will resume on next cycle"
                    })
                } else {
                    let remaining = until - now;
                    let mins = remaining.as_secs() / 60;
                    let hours = mins / 60;
                    let remaining_mins = mins % 60;
                    let time_str = if hours > 0 {
                        format!("{}h {}m", hours, remaining_mins)
                    } else {
                        format!("{}m", remaining_mins)
                    };
                    json!({
                        "status": "snoozed",
                        "remaining": time_str,
                        "remaining_secs": remaining.as_secs(),
                        "description": format!("Circadian snoozed — auto-resumes in {}", time_str)
                    })
                }
            }
            None => {
                json!({
                    "status": "paused",
                    "description": "Circadian paused indefinitely — use resume_circadian to re-enable"
                })
            }
        }
    }

    // --- Light control ---

    async fn light_on(
        &self,
        room_id: &str,
        brightness: Option<u8>,
        color_temp_k: Option<u16>,
        transition: Option<u32>,
        override_ttl_mins: Option<f64>,
    ) -> Result<Value, String> {
        self.validate_room(room_id)?;
        let ct_mired = color_temp_k.map(|k| (1_000_000u32 / k as u32) as u16);

        let group = self
            .config
            .rooms
            .get(room_id)
            .and_then(|r| r.z2m_group.as_deref());

        if let Some(group) = group {
            self.publisher
                .turn_on_group(group, brightness, ct_mired, transition)
                .await
                .map_err(|e| e.to_string())?;
        } else {
            let lights = self
                .config
                .rooms
                .get(room_id)
                .map(|r| r.lights.clone())
                .unwrap_or_default();
            for ieee in &lights {
                let _ = self
                    .publisher
                    .turn_on_ieee(ieee, brightness, ct_mired, transition)
                    .await;
            }
        }

        let is_manual = brightness.is_some() || color_temp_k.is_some();
        let source = if is_manual {
            UpdateSource::Manual
        } else {
            UpdateSource::Circadian
        };

        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::LightsOn {
                    brightness,
                    color_temp_mired: ct_mired,
                    source,
                },
            })
            .await;

        // Mark as jeha push for external change detection
        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::JehaPush,
            })
            .await;

        // Set override TTL if manual values were given
        if is_manual {
            let ttl_until =
                override_ttl_mins.map(|m| Instant::now() + Duration::from_secs_f64(m * 60.0));
            let _ = self
                .state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.to_string(),
                    update: RoomStateUpdate::ManualOverrideTtl { until: ttl_until },
                })
                .await;
        }

        let display_name = self.display_name(room_id);
        let mut desc = format!("{} lights turned on", display_name);
        if let Some(b) = brightness {
            desc.push_str(&format!(", brightness={}", b));
        }
        if let Some(k) = color_temp_k {
            desc.push_str(&format!(", color_temp={}K", k));
        }
        if is_manual {
            if let Some(ttl) = override_ttl_mins {
                desc.push_str(&format!(
                    " (manual override — circadian auto-resumes in {}m)",
                    ttl.round() as u32
                ));
            } else {
                desc.push_str(" (manual override — use resume_circadian to re-enable auto)");
            }
        }

        Ok(json!({"status": "ok", "room": room_id, "description": desc}))
    }

    async fn light_off(&self, room_id: &str, transition: Option<u32>) -> Result<Value, String> {
        self.validate_room(room_id)?;
        let group = self
            .config
            .rooms
            .get(room_id)
            .and_then(|r| r.z2m_group.as_deref());

        if let Some(group) = group {
            self.publisher
                .turn_off_group(group, transition)
                .await
                .map_err(|e| e.to_string())?;
        } else {
            let lights = self
                .config
                .rooms
                .get(room_id)
                .map(|r| r.lights.clone())
                .unwrap_or_default();
            for ieee in &lights {
                let _ = self.publisher.turn_off_ieee(ieee, transition).await;
            }
        }

        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::LightsOff,
            })
            .await;

        let display_name = self.display_name(room_id);
        Ok(
            json!({"status": "ok", "room": room_id, "description": format!("{} lights turned off", display_name)}),
        )
    }

    // --- Circadian control ---

    async fn pause_circadian(&self, room_id: &str) -> Result<Value, String> {
        self.validate_room(room_id)?;

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

        let display_name = self.display_name(room_id);
        Ok(json!({
            "status": "ok",
            "room": room_id,
            "circadian": "paused",
            "description": format!("Circadian rhythm paused for {}. Lights will stay at their current state — jeha won't adjust them. Use resume_circadian when done.", display_name)
        }))
    }

    async fn resume_circadian(&self, room_id: &str) -> Result<Value, String> {
        self.validate_room(room_id)?;

        // Clear pause state
        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::CircadianPause {
                    paused: false,
                    until: None,
                },
            })
            .await;

        // Also clear manual override so circadian can take over
        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::LightsOn {
                    brightness: None,
                    color_temp_mired: None,
                    source: UpdateSource::Circadian,
                },
            })
            .await;

        let display_name = self.display_name(room_id);
        Ok(json!({
            "status": "ok",
            "room": room_id,
            "circadian": "active",
            "description": format!("Circadian rhythm resumed for {}. Lights will smoothly transition to the appropriate setting for the current time of day.", display_name)
        }))
    }

    async fn snooze_circadian(&self, room_id: &str, hours: f64) -> Result<Value, String> {
        self.validate_room(room_id)?;

        if hours <= 0.0 || hours > 24.0 {
            return Err("Hours must be between 0 and 24".to_string());
        }

        let duration = Duration::from_secs_f64(hours * 3600.0);
        let until = Instant::now() + duration;

        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::CircadianPause {
                    paused: true,
                    until: Some(until),
                },
            })
            .await;

        let display_name = self.display_name(room_id);
        let time_str = if hours >= 1.0 {
            let h = hours.floor() as u32;
            let m = ((hours - hours.floor()) * 60.0).round() as u32;
            if m > 0 {
                format!("{}h {}m", h, m)
            } else {
                format!("{}h", h)
            }
        } else {
            format!("{}m", (hours * 60.0).round() as u32)
        };

        Ok(json!({
            "status": "ok",
            "room": room_id,
            "circadian": "snoozed",
            "duration": time_str,
            "description": format!("Circadian rhythm snoozed for {} in {}. Lights stay as-is. Auto-resumes in {}.", display_name, time_str, time_str)
        }))
    }

    // --- Scenes ---

    async fn set_scene(
        &self,
        room_id: &str,
        scene: &str,
        override_ttl_mins: Option<f64>,
    ) -> Result<Value, String> {
        let (brightness, color_temp_k, description) = match scene {
            "bright" => (254u8, 4000u16, "Full brightness, neutral white"),
            "relax" => (150, 2700, "Relaxed — dimmer, warm white"),
            "movie" => (30, 2200, "Movie mode — very dim, warm amber"),
            "energize" => (254, 5500, "Energize — full brightness, cool daylight"),
            "nightlight" => (10, 2200, "Nightlight — barely there, warm amber"),
            _ => {
                return Err(format!(
                    "Unknown scene '{}'. Available: bright, relax, movie, energize, nightlight",
                    scene
                ));
            }
        };

        // Pause circadian since we're setting a scene
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

        self.light_on(
            room_id,
            Some(brightness),
            Some(color_temp_k),
            Some(3),
            override_ttl_mins,
        )
        .await?;

        let display_name = self.display_name(room_id);
        let ttl_desc = match override_ttl_mins {
            Some(m) => format!(" Auto-resumes in {}m.", m.round() as u32),
            None => " Use resume_circadian to go back to automatic.".to_string(),
        };
        Ok(json!({
            "status": "ok",
            "room": room_id,
            "scene": scene,
            "brightness": brightness,
            "color_temp_k": color_temp_k,
            "description": format!("{}: {} — {}. Circadian paused.{}", display_name, scene, description, ttl_desc)
        }))
    }

    // --- Z2M scenes ---

    fn list_z2m_scenes(&self, room_id: &str) -> Result<Value, String> {
        self.validate_room(room_id)?;
        let group_name = self
            .config
            .rooms
            .get(room_id)
            .and_then(|r| r.z2m_group.as_deref());

        let Some(group_name) = group_name else {
            return Ok(json!({
                "room": room_id,
                "scenes": [],
                "description": "This room has no Z2M group — Z2M scenes require a group."
            }));
        };

        let current = self.state.load();
        let scenes: Vec<Value> = current
            .group_map
            .get(group_name)
            .map(|g| {
                g.scenes
                    .iter()
                    .map(|s| json!({"id": s.id, "name": s.name}))
                    .collect()
            })
            .unwrap_or_default();

        let display_name = self.display_name(room_id);
        let desc = if scenes.is_empty() {
            format!(
                "No Z2M scenes found for {} (group '{}'). Create scenes in the Z2M frontend first.",
                display_name, group_name
            )
        } else {
            format!(
                "{} has {} Z2M scene(s) in group '{}'",
                display_name,
                scenes.len(),
                group_name
            )
        };

        Ok(json!({
            "room": room_id,
            "z2m_group": group_name,
            "scenes": scenes,
            "description": desc,
        }))
    }

    async fn recall_z2m_scene(
        &self,
        room_id: &str,
        scene_id: u16,
        override_ttl_mins: Option<f64>,
    ) -> Result<Value, String> {
        self.validate_room(room_id)?;
        let group_name = self
            .config
            .rooms
            .get(room_id)
            .and_then(|r| r.z2m_group.as_deref())
            .ok_or_else(|| format!("Room '{}' has no Z2M group — cannot recall scene", room_id))?;

        // Verify scene exists
        let current = self.state.load();
        let scene_name = current
            .group_map
            .get(group_name)
            .and_then(|g| g.scenes.iter().find(|s| s.id == scene_id))
            .map(|s| s.name.clone());

        if scene_name.is_none() {
            let available: Vec<String> = current
                .group_map
                .get(group_name)
                .map(|g| {
                    g.scenes
                        .iter()
                        .map(|s| format!("{} (id={})", s.name, s.id))
                        .collect()
                })
                .unwrap_or_default();
            return Err(format!(
                "Scene ID {} not found in group '{}'. Available: {}",
                scene_id,
                group_name,
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            ));
        }

        // Pause circadian so we don't overwrite the scene
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

        // Set manual override source with optional TTL
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

        let ttl_until =
            override_ttl_mins.map(|m| Instant::now() + Duration::from_secs_f64(m * 60.0));
        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::ManualOverrideTtl { until: ttl_until },
            })
            .await;

        // Recall the scene
        self.publisher
            .recall_scene_group(group_name, scene_id)
            .await
            .map_err(|e| e.to_string())?;

        // Mark as jeha push so our own state echoes don't trigger external change detection
        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::JehaPush,
            })
            .await;

        let display_name = self.display_name(room_id);
        let scene_label = scene_name.as_deref().unwrap_or("unknown");
        let ttl_desc = match override_ttl_mins {
            Some(m) => format!(" Auto-resumes circadian in {}m.", m.round() as u32),
            None => " Use resume_circadian to go back to automatic.".to_string(),
        };

        Ok(json!({
            "status": "ok",
            "room": room_id,
            "z2m_scene": scene_label,
            "scene_id": scene_id,
            "description": format!("{}: recalled Z2M scene '{}'. Circadian paused.{}", display_name, scene_label, ttl_desc),
        }))
    }

    // --- Night mode ---

    async fn set_night_mode(&self, room_id: &str, active: bool) {
        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::NightMode(active),
            })
            .await;
    }

    // --- Status ---

    fn get_circadian_status(&self) -> Value {
        let current = self.state.load();
        let mut rooms: Vec<Value> = current
            .rooms
            .iter()
            .map(|(room_id, rs)| {
                let display_name = self.display_name(room_id);
                let circadian_status = self.describe_circadian_status(rs);
                json!({
                    "room_id": room_id,
                    "display_name": display_name,
                    "lights_on": rs.lights_on,
                    "brightness": rs.current_brightness,
                    "color_temp_mired": rs.current_color_temp_mired,
                    "circadian": circadian_status,
                })
            })
            .collect();
        rooms.sort_by(|a, b| a["display_name"].as_str().cmp(&b["display_name"].as_str()));
        json!(rooms)
    }

    fn get_system_status(&self) -> Value {
        let current = self.state.load();
        let uptime = current
            .started_at
            .map(|s| s.elapsed().as_secs())
            .unwrap_or(0);
        let uptime_str = format_duration(uptime);
        json!({
            "mqtt_connected": current.mqtt_connected,
            "z2m_online": current.z2m_online,
            "uptime": uptime_str,
            "uptime_secs": uptime,
            "device_count": current.device_map.len(),
            "group_count": current.group_map.len(),
            "room_count": current.rooms.len(),
        })
    }

    // --- Helpers ---

    fn validate_room(&self, room_id: &str) -> Result<(), String> {
        if self.config.rooms.contains_key(room_id) {
            Ok(())
        } else {
            let available: Vec<&str> = self.config.rooms.keys().map(|s| s.as_str()).collect();
            Err(format!(
                "Room '{}' not found. Available rooms: {}",
                room_id,
                available.join(", ")
            ))
        }
    }

    fn display_name(&self, room_id: &str) -> String {
        self.config
            .rooms
            .get(room_id)
            .and_then(|r| r.display_name.as_deref())
            .unwrap_or(room_id)
            .to_string()
    }
}

fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else {
        format!("{}m", mins)
    }
}
