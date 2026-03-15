use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::circadian::CircadianEngine;
use crate::config::types::AppConfig;
use crate::event::EventBus;
use crate::mqtt::publish::Publisher;
use crate::state::{RoomStateUpdate, SharedState, StateCommand, UpdateSource};

// --- Shared state ---

#[derive(Clone)]
pub struct AppState {
    state: SharedState,
    state_tx: mpsc::Sender<StateCommand>,
    publisher: Arc<Publisher>,
    config: Arc<AppConfig>,
    event_bus: EventBus,
    circadian_engine: Option<Arc<CircadianEngine>>,
}

impl AppState {
    pub fn new(
        state: SharedState,
        state_tx: mpsc::Sender<StateCommand>,
        publisher: Arc<Publisher>,
        config: Arc<AppConfig>,
        event_bus: EventBus,
        circadian_engine: Option<Arc<CircadianEngine>>,
    ) -> Self {
        Self {
            state,
            state_tx,
            publisher,
            config,
            event_bus,
            circadian_engine,
        }
    }
}

// --- Error type ---

pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

impl AppState {
    fn validate_room(&self, room_id: &str) -> Result<(), ApiError> {
        if self.config.rooms.contains_key(room_id) {
            Ok(())
        } else {
            let available: Vec<&str> = self.config.rooms.keys().map(|s| s.as_str()).collect();
            Err(ApiError::NotFound(format!(
                "Room '{}' not found. Available rooms: {}",
                room_id,
                available.join(", ")
            )))
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

    fn lights_for_room(&self, room_id: &str) -> Vec<String> {
        let room_config = self.config.rooms.get(room_id);
        if let Some(rc) = room_config {
            if !rc.lights.is_empty() {
                return rc.lights.clone();
            }
            if let Some(ref group_name) = rc.z2m_group {
                let current = self.state.load();
                if let Some(group) = current.group_map.get(group_name) {
                    return group
                        .members
                        .iter()
                        .map(|m| m.ieee_address.clone())
                        .collect();
                }
            }
        }
        Vec::new()
    }

    fn describe_circadian_status(&self, rs: &crate::state::RoomState) -> Value {
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
}

// --- Request types ---

#[derive(Deserialize)]
pub struct LightOnRequest {
    pub brightness: Option<u8>,
    pub color_temp_k: Option<u16>,
    pub transition: Option<u32>,
    pub override_ttl_mins: Option<f64>,
}

#[derive(Deserialize)]
pub struct LightOffRequest {
    pub transition: Option<u32>,
}

#[derive(Deserialize)]
pub struct SnoozeRequest {
    pub hours: f64,
}

#[derive(Deserialize)]
pub struct SetSceneRequest {
    pub scene: String,
    pub override_ttl_mins: Option<f64>,
}

#[derive(Deserialize)]
pub struct RecallZ2mSceneRequest {
    pub scene_id: u16,
    pub override_ttl_mins: Option<f64>,
}

#[derive(Deserialize)]
pub struct SetNightModeRequest {
    pub active: bool,
}

// --- Handlers ---

pub async fn get_rooms(State(app): State<Arc<AppState>>) -> Json<Value> {
    let current = app.state.load();
    let mut rooms: Vec<Value> = current
        .rooms
        .iter()
        .map(|(room_id, rs)| {
            let display_name = app
                .config
                .rooms
                .get(room_id)
                .and_then(|r| r.display_name.as_deref())
                .unwrap_or(room_id);
            let circadian_status = app.describe_circadian_status(rs);
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
    Json(json!(rooms))
}

pub async fn get_room(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let current = app.state.load();
    match current.rooms.get(&room_id) {
        Some(rs) => {
            let room_config = app.config.rooms.get(&room_id);
            let display_name = room_config
                .and_then(|r| r.display_name.as_deref())
                .unwrap_or(&room_id);
            let z2m_group = room_config.and_then(|r| r.z2m_group.as_deref());
            let has_motion = room_config
                .map(|r| r.motion_sensor.is_some())
                .unwrap_or(false);
            let remotes = room_config.map(|r| r.remotes.clone()).unwrap_or_default();
            let motion_timeout_secs = room_config
                .and_then(|r| r.effective_motion_timeout(app.config.general.motion_timeout_secs));
            let circadian_enabled = room_config.map(|r| r.circadian_enabled).unwrap_or(true);
            let circadian_status = if circadian_enabled {
                app.describe_circadian_status(rs)
            } else {
                json!({"status": "disabled", "description": "Circadian is disabled for this room in config"})
            };

            Ok(Json(json!({
                "room_id": room_id,
                "display_name": display_name,
                "z2m_group": z2m_group,
                "lights_on": rs.lights_on,
                "brightness": rs.current_brightness,
                "color_temp_mired": rs.current_color_temp_mired,
                "occupancy": rs.occupancy,
                "has_motion_sensor": has_motion,
                "motion_timeout_secs": motion_timeout_secs,
                "remotes": remotes,
                "night_mode": rs.night_mode_active,
                "circadian_enabled": circadian_enabled,
                "circadian": circadian_status,
            })))
        }
        None => Err(ApiError::NotFound(format!(
            "Room '{}' not found. Use GET /api/rooms to see available room IDs.",
            room_id
        ))),
    }
}

pub async fn light_on(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    Json(body): Json<LightOnRequest>,
) -> Result<Json<Value>, ApiError> {
    app.validate_room(&room_id)?;
    let ct_mired = body.color_temp_k.map(|k| (1_000_000u32 / k as u32) as u16);

    let room_config = app.config.rooms.get(&room_id);
    let use_group = room_config
        .and_then(|r| r.z2m_group.as_ref())
        .and_then(|group_name| {
            let current = app.state.load();
            let group = current.group_map.get(group_name)?;
            if crate::calibration::group_needs_fanout(
                group,
                &app.config.light_calibration,
                &current.device_map,
            ) {
                None
            } else {
                Some(group_name.clone())
            }
        });

    if let Some(ref group) = use_group {
        app.publisher
            .turn_on_group(group, body.brightness, ct_mired, body.transition)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    } else {
        let lights = app.lights_for_room(&room_id);
        for ieee in &lights {
            let _ = app
                .publisher
                .turn_on_ieee(ieee, body.brightness, ct_mired, body.transition)
                .await;
        }
    }

    let is_manual = body.brightness.is_some() || body.color_temp_k.is_some();
    let source = if is_manual {
        UpdateSource::Manual
    } else {
        UpdateSource::Circadian
    };

    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::LightsOn {
                brightness: body.brightness,
                color_temp_mired: ct_mired,
                source,
            },
        })
        .await;

    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::JehaPush {
                brightness: body.brightness,
                color_temp_mired: ct_mired,
            },
        })
        .await;

    if is_manual {
        let ttl_until = body
            .override_ttl_mins
            .map(|m| Instant::now() + Duration::from_secs_f64(m * 60.0));
        let _ = app
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::ManualOverrideTtl { until: ttl_until },
            })
            .await;
    }

    let display_name = app.display_name(&room_id);
    let mut desc = format!("{} lights turned on", display_name);
    if let Some(b) = body.brightness {
        desc.push_str(&format!(", brightness={}", b));
    }
    if let Some(k) = body.color_temp_k {
        desc.push_str(&format!(", color_temp={}K", k));
    }
    if is_manual {
        if let Some(ttl) = body.override_ttl_mins {
            desc.push_str(&format!(
                " (manual override — circadian auto-resumes in {}m)",
                ttl.round() as u32
            ));
        } else {
            desc.push_str(" (manual override — use resume_circadian to re-enable auto)");
        }
    }

    Ok(Json(
        json!({"status": "ok", "room": room_id, "description": desc}),
    ))
}

pub async fn light_off(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    Json(body): Json<LightOffRequest>,
) -> Result<Json<Value>, ApiError> {
    app.validate_room(&room_id)?;
    let group = app
        .config
        .rooms
        .get(&room_id)
        .and_then(|r| r.z2m_group.as_deref());

    if let Some(group) = group {
        app.publisher
            .turn_off_group(group, body.transition)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    } else {
        let lights = app
            .config
            .rooms
            .get(&room_id)
            .map(|r| r.lights.clone())
            .unwrap_or_default();
        for ieee in &lights {
            let _ = app.publisher.turn_off_ieee(ieee, body.transition).await;
        }
    }

    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::LightsOff,
        })
        .await;

    let display_name = app.display_name(&room_id);
    Ok(Json(
        json!({"status": "ok", "room": room_id, "description": format!("{} lights turned off", display_name)}),
    ))
}

pub async fn pause_circadian(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    app.validate_room(&room_id)?;

    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::CircadianPause {
                paused: true,
                until: None,
            },
        })
        .await;

    let display_name = app.display_name(&room_id);
    Ok(Json(json!({
        "status": "ok",
        "room": room_id,
        "circadian": "paused",
        "description": format!("Circadian rhythm paused for {}. Lights will stay at their current state — jeha won't adjust them. Use resume_circadian when done.", display_name)
    })))
}

pub async fn resume_circadian(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    app.validate_room(&room_id)?;

    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::CircadianPause {
                paused: false,
                until: None,
            },
        })
        .await;

    let _ = app
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

    let display_name = app.display_name(&room_id);
    Ok(Json(json!({
        "status": "ok",
        "room": room_id,
        "circadian": "active",
        "description": format!("Circadian rhythm resumed for {}. Lights will smoothly transition to the appropriate setting for the current time of day.", display_name)
    })))
}

pub async fn snooze_circadian(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    Json(body): Json<SnoozeRequest>,
) -> Result<Json<Value>, ApiError> {
    app.validate_room(&room_id)?;

    if body.hours <= 0.0 || body.hours > 24.0 {
        return Err(ApiError::BadRequest(
            "Hours must be between 0 and 24".to_string(),
        ));
    }

    let duration = Duration::from_secs_f64(body.hours * 3600.0);
    let until = Instant::now() + duration;

    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::CircadianPause {
                paused: true,
                until: Some(until),
            },
        })
        .await;

    let display_name = app.display_name(&room_id);
    let time_str = if body.hours >= 1.0 {
        let h = body.hours.floor() as u32;
        let m = ((body.hours - body.hours.floor()) * 60.0).round() as u32;
        if m > 0 {
            format!("{}h {}m", h, m)
        } else {
            format!("{}h", h)
        }
    } else {
        format!("{}m", (body.hours * 60.0).round() as u32)
    };

    Ok(Json(json!({
        "status": "ok",
        "room": room_id,
        "circadian": "snoozed",
        "duration": time_str,
        "description": format!("Circadian rhythm snoozed for {} in {}. Lights stay as-is. Auto-resumes in {}.", display_name, time_str, time_str)
    })))
}

pub async fn set_scene(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    Json(body): Json<SetSceneRequest>,
) -> Result<Json<Value>, ApiError> {
    app.validate_room(&room_id)?;

    let (brightness, color_temp_k, description) = match body.scene.as_str() {
        "bright" => (254u8, 4000u16, "Full brightness, neutral white"),
        "relax" => (150, 2700, "Relaxed — dimmer, warm white"),
        "movie" => (30, 2200, "Movie mode — very dim, warm amber"),
        "energize" => (254, 5500, "Energize — full brightness, cool daylight"),
        "nightlight" => (10, 2200, "Nightlight — barely there, warm amber"),
        _ => {
            return Err(ApiError::BadRequest(format!(
                "Unknown scene '{}'. Available: bright, relax, movie, energize, nightlight",
                body.scene
            )));
        }
    };

    // Pause circadian
    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::CircadianPause {
                paused: true,
                until: None,
            },
        })
        .await;

    // Apply the scene via light_on logic
    let ct_mired = (1_000_000u32 / color_temp_k as u32) as u16;
    let room_config = app.config.rooms.get(&room_id);
    let use_group = room_config
        .and_then(|r| r.z2m_group.as_ref())
        .and_then(|group_name| {
            let current = app.state.load();
            let group = current.group_map.get(group_name)?;
            if crate::calibration::group_needs_fanout(
                group,
                &app.config.light_calibration,
                &current.device_map,
            ) {
                None
            } else {
                Some(group_name.clone())
            }
        });

    if let Some(ref group) = use_group {
        app.publisher
            .turn_on_group(group, Some(brightness), Some(ct_mired), Some(3))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    } else {
        let lights = app.lights_for_room(&room_id);
        for ieee in &lights {
            let _ = app
                .publisher
                .turn_on_ieee(ieee, Some(brightness), Some(ct_mired), Some(3))
                .await;
        }
    }

    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::LightsOn {
                brightness: Some(brightness),
                color_temp_mired: Some(ct_mired),
                source: UpdateSource::Manual,
            },
        })
        .await;

    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::JehaPush {
                brightness: Some(brightness),
                color_temp_mired: Some(ct_mired),
            },
        })
        .await;

    let ttl_until = body
        .override_ttl_mins
        .map(|m| Instant::now() + Duration::from_secs_f64(m * 60.0));
    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::ManualOverrideTtl { until: ttl_until },
        })
        .await;

    let display_name = app.display_name(&room_id);
    let ttl_desc = match body.override_ttl_mins {
        Some(m) => format!(" Auto-resumes in {}m.", m.round() as u32),
        None => " Use resume_circadian to go back to automatic.".to_string(),
    };
    Ok(Json(json!({
        "status": "ok",
        "room": room_id,
        "scene": body.scene,
        "brightness": brightness,
        "color_temp_k": color_temp_k,
        "description": format!("{}: {} — {}. Circadian paused.{}", display_name, body.scene, description, ttl_desc)
    })))
}

pub async fn list_z2m_scenes(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    app.validate_room(&room_id)?;
    let group_name = app
        .config
        .rooms
        .get(&room_id)
        .and_then(|r| r.z2m_group.as_deref());

    let Some(group_name) = group_name else {
        return Ok(Json(json!({
            "room": room_id,
            "scenes": [],
            "description": "This room has no Z2M group — Z2M scenes require a group."
        })));
    };

    let current = app.state.load();
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

    let display_name = app.display_name(&room_id);
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

    Ok(Json(json!({
        "room": room_id,
        "z2m_group": group_name,
        "scenes": scenes,
        "description": desc,
    })))
}

pub async fn recall_z2m_scene(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    Json(body): Json<RecallZ2mSceneRequest>,
) -> Result<Json<Value>, ApiError> {
    app.validate_room(&room_id)?;
    let group_name = app
        .config
        .rooms
        .get(&room_id)
        .and_then(|r| r.z2m_group.as_deref())
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "Room '{}' has no Z2M group — cannot recall scene",
                room_id
            ))
        })?;

    let current = app.state.load();
    let scene_name = current
        .group_map
        .get(group_name)
        .and_then(|g| g.scenes.iter().find(|s| s.id == body.scene_id))
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
        return Err(ApiError::NotFound(format!(
            "Scene ID {} not found in group '{}'. Available: {}",
            body.scene_id,
            group_name,
            if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            }
        )));
    }

    // Pause circadian
    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::CircadianPause {
                paused: true,
                until: None,
            },
        })
        .await;

    let _ = app
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

    let ttl_until = body
        .override_ttl_mins
        .map(|m| Instant::now() + Duration::from_secs_f64(m * 60.0));
    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::ManualOverrideTtl { until: ttl_until },
        })
        .await;

    app.publisher
        .recall_scene_group(group_name, body.scene_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let _ = app
        .state_tx
        .send(StateCommand::UpdateRoomState {
            room_id: room_id.to_string(),
            update: RoomStateUpdate::JehaPush {
                brightness: None,
                color_temp_mired: None,
            },
        })
        .await;

    let display_name = app.display_name(&room_id);
    let scene_label = scene_name.as_deref().unwrap_or("unknown");
    let ttl_desc = match body.override_ttl_mins {
        Some(m) => format!(" Auto-resumes circadian in {}m.", m.round() as u32),
        None => " Use resume_circadian to go back to automatic.".to_string(),
    };

    Ok(Json(json!({
        "status": "ok",
        "room": room_id,
        "z2m_scene": scene_label,
        "scene_id": body.scene_id,
        "description": format!("{}: recalled Z2M scene '{}'. Circadian paused.{}", display_name, scene_label, ttl_desc),
    })))
}

pub async fn set_night_mode(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    Json(body): Json<SetNightModeRequest>,
) -> Result<Json<Value>, ApiError> {
    app.validate_room(&room_id)?;
    let room_config = app.config.rooms.get(&room_id).unwrap();
    let display_name = app.display_name(&room_id);

    if body.active {
        crate::night_mode::activate_night_mode(
            &room_id,
            room_config,
            &app.config,
            &app.publisher,
            &app.state,
            &app.state_tx,
            &app.event_bus,
        )
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        let enm = room_config.effective_night_mode(&app.config.night_mode.defaults);
        Ok(Json(json!({
            "status": "ok",
            "room": room_id,
            "night_mode": true,
            "description": format!("{}: night mode on (brightness {}, {}K). Circadian paused.", display_name, enm.brightness, enm.color_temp_k)
        })))
    } else {
        crate::night_mode::deactivate_night_mode(
            &room_id,
            room_config,
            &app.publisher,
            &app.state,
            &app.state_tx,
            &app.event_bus,
            &app.circadian_engine,
        )
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        Ok(Json(json!({
            "status": "ok",
            "room": room_id,
            "night_mode": false,
            "description": format!("{}: night mode off. Circadian resumed.", display_name)
        })))
    }
}

pub async fn get_circadian_status(State(app): State<Arc<AppState>>) -> Json<Value> {
    let current = app.state.load();
    let mut rooms: Vec<Value> = current
        .rooms
        .iter()
        .map(|(room_id, rs)| {
            let display_name = app.display_name(room_id);
            let circadian_status = app.describe_circadian_status(rs);
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
    Json(json!(rooms))
}

pub async fn get_system_status(State(app): State<Arc<AppState>>) -> Json<Value> {
    let current = app.state.load();
    let uptime = current
        .started_at
        .map(|s| s.elapsed().as_secs())
        .unwrap_or(0);
    let uptime_str = format_duration(uptime);
    Json(json!({
        "mqtt_connected": current.mqtt_connected,
        "z2m_online": current.z2m_online,
        "uptime": uptime_str,
        "uptime_secs": uptime,
        "device_count": current.device_map.len(),
        "group_count": current.group_map.len(),
        "room_count": current.rooms.len(),
    }))
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
