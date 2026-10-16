use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, OptionalFromRequest, Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;

use crate::circadian::CircadianEngine;
use crate::config::types::AppConfig;
use crate::event::EventBus;
use crate::mqtt::publish::Publisher;
use crate::state::{RoomStateUpdate, ScheduleOverride, SharedState, StateCommand, UpdateSource};

use super::types::*;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    NotFound(String),
    MethodNotAllowed(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::MethodNotAllowed(msg) => (StatusCode::METHOD_NOT_ALLOWED, msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        let mut response = (status, Json(ErrorResponse { error: message })).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static("Bearer"),
            );
        }
        response
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        ApiError::BadRequest(rejection.body_text())
    }
}

// --- JSON body extractor ---

/// `axum::Json`, but malformed bodies are reported as the API's usual
/// `{"error": ...}` JSON instead of a plain-text rejection.
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = <Json<T> as FromRequest<S>>::from_request(req, state).await?;
        Ok(ApiJson(value))
    }
}

/// Lets a handler take `Option<ApiJson<T>>` for an optional body: a request
/// with no `Content-Type` yields `None`, anything else must be valid JSON.
impl<T, S> OptionalFromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Option<Self>, Self::Rejection> {
        let value = <Json<T> as OptionalFromRequest<S>>::from_request(req, state).await?;
        Ok(value.map(|Json(v)| ApiJson(v)))
    }
}

impl AppState {
    fn validate_room(&self, room_id: &str) -> Result<(), ApiError> {
        if self.config.rooms.contains_key(room_id) {
            Ok(())
        } else {
            let mut available: Vec<&str> = self.config.rooms.keys().map(|s| s.as_str()).collect();
            available.sort_unstable();
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

    fn describe_circadian_status(&self, rs: &crate::state::RoomState) -> CircadianStatus {
        if rs.update_source == UpdateSource::Manual {
            if let Some(until) = rs.manual_override_until {
                let now = Instant::now();
                if now < until {
                    let remaining = until - now;
                    let mins = remaining.as_secs() / 60;
                    return CircadianStatus {
                        remaining_mins: Some(mins),
                        ..CircadianStatus::new(
                            CircadianState::ManualOverride,
                            format!(
                                "Manual override active — circadian auto-resumes in {}m",
                                mins
                            ),
                        )
                    };
                }
            } else if !rs.circadian_paused {
                return CircadianStatus::new(
                    CircadianState::ManualOverride,
                    "Manual override active indefinitely — use resume_circadian to re-enable",
                );
            }
        }

        if !rs.circadian_paused {
            return CircadianStatus::new(
                CircadianState::Active,
                "Circadian rhythm is active — lights adjust automatically",
            );
        }

        match rs.circadian_paused_until {
            Some(until) => {
                let now = Instant::now();
                if now >= until {
                    CircadianStatus::new(
                        CircadianState::Active,
                        "Circadian snooze has expired — will resume on next cycle",
                    )
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
                    CircadianStatus {
                        remaining_secs: Some(remaining.as_secs()),
                        description: format!("Circadian snoozed — auto-resumes in {}", time_str),
                        remaining: Some(time_str),
                        ..CircadianStatus::new(CircadianState::Snoozed, "")
                    }
                }
            }
            None => CircadianStatus::new(
                CircadianState::Paused,
                "Circadian paused indefinitely — use resume_circadian to re-enable",
            ),
        }
    }
}

// --- Input validation ---

/// Longest manual override the API accepts: one week.
const MAX_OVERRIDE_TTL_MINS: f64 = 7.0 * 24.0 * 60.0;

fn validate_override_ttl(ttl: Option<f64>) -> Result<(), ApiError> {
    match ttl {
        Some(m) if !(m > 0.0 && m <= MAX_OVERRIDE_TTL_MINS) => Err(ApiError::BadRequest(format!(
            "override_ttl_mins must be greater than 0 and at most {}",
            MAX_OVERRIDE_TTL_MINS
        ))),
        _ => Ok(()),
    }
}

fn validate_light_on(body: &LightOnRequest) -> Result<(), ApiError> {
    if let Some(b) = body.brightness
        && !(1..=254).contains(&b)
    {
        return Err(ApiError::BadRequest(
            "brightness must be between 1 and 254".to_string(),
        ));
    }
    if let Some(k) = body.color_temp_k
        && !(1000..=10000).contains(&k)
    {
        return Err(ApiError::BadRequest(
            "color_temp_k must be between 1000 and 10000".to_string(),
        ));
    }
    validate_override_ttl(body.override_ttl_mins)
}

// --- Built-in scenes ---

/// (name, brightness, color_temp_k, description)
pub const SCENES: &[(&str, u8, u16, &str)] = &[
    ("bright", 254, 4000, "Full brightness, neutral white"),
    ("relax", 150, 2700, "Relaxed — dimmer, warm white"),
    ("movie", 30, 2200, "Movie mode — very dim, warm amber"),
    (
        "energize",
        254,
        5500,
        "Energize — full brightness, cool daylight",
    ),
    (
        "nightlight",
        10,
        2200,
        "Nightlight — barely there, warm amber",
    ),
];

// --- Handlers ---

/// List all rooms.
///
/// Rooms come back sorted by display name, each with its light state,
/// occupancy and circadian status.
#[utoipa::path(
    get,
    path = "/api/rooms",
    tag = "rooms",
    operation_id = "list_rooms",
    responses(
        (status = 200, description = "All configured rooms", body = Vec<RoomSummary>),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
    )
)]
pub async fn get_rooms(State(app): State<Arc<AppState>>) -> Json<Vec<RoomSummary>> {
    let current = app.state.load();
    let mut rooms: Vec<RoomSummary> = current
        .rooms
        .iter()
        .map(|(room_id, rs)| RoomSummary {
            room_id: room_id.clone(),
            display_name: app.display_name(room_id),
            lights_on: rs.lights_on,
            brightness: rs.current_brightness,
            color_temp_mired: rs.current_color_temp_mired,
            occupancy: rs.occupancy,
            night_mode: rs.night_mode_active,
            circadian: app.describe_circadian_status(rs),
        })
        .collect();
    rooms.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Json(rooms)
}

/// Get one room.
#[utoipa::path(
    get,
    path = "/api/rooms/{room_id}",
    tag = "rooms",
    params(("room_id" = String, Path, description = "Room ID (the key under `[rooms]` in config.toml)")),
    responses(
        (status = 200, description = "Room detail", body = RoomDetail),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
        (status = 404, description = "Unknown room", body = ErrorResponse),
    )
)]
pub async fn get_room(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
) -> Result<Json<RoomDetail>, ApiError> {
    let current = app.state.load();
    let Some(rs) = current.rooms.get(&room_id) else {
        return Err(ApiError::NotFound(format!(
            "Room '{}' not found. Use GET /api/rooms to see available room IDs.",
            room_id
        )));
    };
    let room_config = app.config.rooms.get(&room_id);
    let circadian_enabled = room_config.map(|r| r.circadian_enabled).unwrap_or(true);
    let circadian = if circadian_enabled {
        app.describe_circadian_status(rs)
    } else {
        CircadianStatus::new(
            CircadianState::Disabled,
            "Circadian is disabled for this room in config",
        )
    };

    Ok(Json(RoomDetail {
        display_name: app.display_name(&room_id),
        z2m_group: room_config.and_then(|r| r.z2m_group.clone()),
        lights_on: rs.lights_on,
        brightness: rs.current_brightness,
        color_temp_mired: rs.current_color_temp_mired,
        occupancy: rs.occupancy,
        has_motion_sensor: room_config
            .map(|r| r.motion_sensor.is_some())
            .unwrap_or(false),
        motion_timeout_secs: room_config
            .and_then(|r| r.effective_motion_timeout(app.config.general.motion_timeout_secs)),
        remotes: room_config.map(|r| r.remotes.clone()).unwrap_or_default(),
        night_mode: rs.night_mode_active,
        circadian_enabled,
        circadian,
        room_id,
    }))
}

/// Turn a room's lights on.
///
/// With no brightness or colour temperature, only `state: ON` is sent: the
/// lights come back at their last state and move to the circadian target on
/// the next circadian update (unless circadian is paused or snoozed). Setting
/// either one is a manual override: circadian stops adjusting the room until
/// `override_ttl_mins` runs out or circadian is resumed.
#[utoipa::path(
    post,
    path = "/api/rooms/{room_id}/light/on",
    tag = "lights",
    params(("room_id" = String, Path, description = "Room ID")),
    request_body(content = LightOnRequest, description = "Optional; the body (and Content-Type) may be omitted"),
    responses(
        (status = 200, description = "Done", body = ActionResponse),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
        (status = 404, description = "Unknown room", body = ErrorResponse),
        (status = 500, description = "Publishing to Zigbee2MQTT failed", body = ErrorResponse),
    )
)]
pub async fn light_on(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    body: Option<ApiJson<LightOnRequest>>,
) -> Result<Json<ActionResponse>, ApiError> {
    app.validate_room(&room_id)?;
    let body = body.map(|ApiJson(b)| b).unwrap_or_default();
    validate_light_on(&body)?;
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
        if lights.is_empty() {
            return Err(ApiError::Internal(format!(
                "No lights resolved for '{}' — Z2M group not discovered yet?",
                room_id
            )));
        }
        let mut failed = 0usize;
        for ieee in &lights {
            if let Err(e) = app
                .publisher
                .turn_on_ieee(ieee, body.brightness, ct_mired, body.transition)
                .await
            {
                tracing::warn!("light_on: publish to {} failed: {}", ieee, e);
                failed += 1;
            }
        }
        if failed == lights.len() {
            return Err(ApiError::Internal(format!(
                "Failed to publish to all {} lights in '{}'",
                lights.len(),
                room_id
            )));
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

    Ok(Json(ActionResponse::ok(&room_id, desc)))
}

/// Turn a room's lights off.
#[utoipa::path(
    post,
    path = "/api/rooms/{room_id}/light/off",
    tag = "lights",
    params(("room_id" = String, Path, description = "Room ID")),
    request_body(content = LightOffRequest, description = "Optional; the body may be omitted"),
    responses(
        (status = 200, description = "Done", body = ActionResponse),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
        (status = 404, description = "Unknown room", body = ErrorResponse),
        (status = 500, description = "Publishing to Zigbee2MQTT failed", body = ErrorResponse),
    )
)]
pub async fn light_off(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    body: Option<ApiJson<LightOffRequest>>,
) -> Result<Json<ActionResponse>, ApiError> {
    app.validate_room(&room_id)?;
    let body = body.map(|ApiJson(b)| b).unwrap_or_default();
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
        let mut failed = 0usize;
        for ieee in &lights {
            if let Err(e) = app.publisher.turn_off_ieee(ieee, body.transition).await {
                tracing::warn!("light_off: publish to {} failed: {}", ieee, e);
                failed += 1;
            }
        }
        if failed > 0 && failed == lights.len() {
            return Err(ApiError::Internal(format!(
                "Failed to publish to all {} lights in '{}'",
                lights.len(),
                room_id
            )));
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
    Ok(Json(ActionResponse::ok(
        &room_id,
        format!("{} lights turned off", display_name),
    )))
}

/// Pause circadian for a room.
///
/// jeha stops adjusting the room's lights until circadian is resumed.
#[utoipa::path(
    post,
    path = "/api/rooms/{room_id}/circadian/pause",
    tag = "circadian",
    params(("room_id" = String, Path, description = "Room ID")),
    responses(
        (status = 200, description = "Done", body = ActionResponse),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
        (status = 404, description = "Unknown room", body = ErrorResponse),
        (status = 500, description = "Publishing to Zigbee2MQTT failed", body = ErrorResponse),
    )
)]
pub async fn pause_circadian(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
) -> Result<Json<ActionResponse>, ApiError> {
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
    Ok(Json(ActionResponse {
        circadian: Some("paused".to_string()),
        ..ActionResponse::ok(
            &room_id,
            format!(
                "Circadian rhythm paused for {}. Lights will stay at their current state — jeha won't adjust them. Use resume_circadian when done.",
                display_name
            ),
        )
    }))
}

/// Resume circadian for a room.
///
/// Clears any pause, snooze or manual override; the lights transition to the
/// circadian target on the next cycle.
#[utoipa::path(
    post,
    path = "/api/rooms/{room_id}/circadian/resume",
    tag = "circadian",
    params(("room_id" = String, Path, description = "Room ID")),
    responses(
        (status = 200, description = "Done", body = ActionResponse),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
        (status = 404, description = "Unknown room", body = ErrorResponse),
        (status = 500, description = "Publishing to Zigbee2MQTT failed", body = ErrorResponse),
    )
)]
pub async fn resume_circadian(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
) -> Result<Json<ActionResponse>, ApiError> {
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
    Ok(Json(ActionResponse {
        circadian: Some("active".to_string()),
        ..ActionResponse::ok(
            &room_id,
            format!(
                "Circadian rhythm resumed for {}. Lights will smoothly transition to the appropriate setting for the current time of day.",
                display_name
            ),
        )
    }))
}

/// Snooze circadian for a room.
///
/// Like pause, but circadian resumes by itself after `hours`.
#[utoipa::path(
    post,
    path = "/api/rooms/{room_id}/circadian/snooze",
    tag = "circadian",
    params(("room_id" = String, Path, description = "Room ID")),
    request_body = SnoozeRequest,
    responses(
        (status = 200, description = "Done", body = ActionResponse),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
        (status = 404, description = "Unknown room", body = ErrorResponse),
        (status = 500, description = "Publishing to Zigbee2MQTT failed", body = ErrorResponse),
    )
)]
pub async fn snooze_circadian(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    ApiJson(body): ApiJson<SnoozeRequest>,
) -> Result<Json<ActionResponse>, ApiError> {
    app.validate_room(&room_id)?;

    if !(body.hours > 0.0 && body.hours <= 24.0) {
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

    let description = format!(
        "Circadian rhythm snoozed for {} in {}. Lights stay as-is. Auto-resumes in {}.",
        display_name, time_str, time_str
    );
    Ok(Json(ActionResponse {
        circadian: Some("snoozed".to_string()),
        duration: Some(time_str),
        ..ActionResponse::ok(&room_id, description)
    }))
}

/// Apply a built-in scene.
///
/// Pauses circadian for the room. See `GET /api/scenes` for the available scenes.
#[utoipa::path(
    post,
    path = "/api/rooms/{room_id}/scene",
    tag = "scenes",
    params(("room_id" = String, Path, description = "Room ID")),
    request_body = SetSceneRequest,
    responses(
        (status = 200, description = "Done", body = ActionResponse),
        (status = 400, description = "Unknown scene or invalid body", body = ErrorResponse),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
        (status = 404, description = "Unknown room", body = ErrorResponse),
        (status = 500, description = "Publishing to Zigbee2MQTT failed", body = ErrorResponse),
    )
)]
pub async fn set_scene(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    ApiJson(body): ApiJson<SetSceneRequest>,
) -> Result<Json<ActionResponse>, ApiError> {
    app.validate_room(&room_id)?;
    validate_override_ttl(body.override_ttl_mins)?;

    let Some(&(_, brightness, color_temp_k, description)) =
        SCENES.iter().find(|(name, ..)| *name == body.scene)
    else {
        let available: Vec<&str> = SCENES.iter().map(|(name, ..)| *name).collect();
        return Err(ApiError::BadRequest(format!(
            "Unknown scene '{}'. Available: {}",
            body.scene,
            available.join(", ")
        )));
    };

    // Resolve the publish target BEFORE pausing circadian, so a room whose
    // lights can't be resolved yet errors out without side effects.
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
    let lights = if use_group.is_none() {
        let lights = app.lights_for_room(&room_id);
        if lights.is_empty() {
            return Err(ApiError::Internal(format!(
                "No lights resolved for '{}' — Z2M group not discovered yet?",
                room_id
            )));
        }
        lights
    } else {
        Vec::new()
    };

    // Pause circadian before publishing so a circadian tick can't race the
    // scene. Snapshot the prior pause state first: if the publish fails we
    // roll the pause back so a failed call leaves no side effects.
    let prior_pause = {
        let current = app.state.load();
        current
            .rooms
            .get(&room_id)
            .map(|rs| (rs.circadian_paused, rs.circadian_paused_until))
            .unwrap_or((false, None))
    };
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

    let publish_result: Result<(), String> = if let Some(ref group) = use_group {
        app.publisher
            .turn_on_group(group, Some(brightness), Some(ct_mired), Some(3))
            .await
            .map_err(|e| e.to_string())
    } else {
        let mut failed = 0usize;
        for ieee in &lights {
            if let Err(e) = app
                .publisher
                .turn_on_ieee(ieee, Some(brightness), Some(ct_mired), Some(3))
                .await
            {
                tracing::warn!("set_scene: publish to {} failed: {}", ieee, e);
                failed += 1;
            }
        }
        if failed == lights.len() {
            Err(format!(
                "Failed to publish to all {} lights in '{}'",
                lights.len(),
                room_id
            ))
        } else {
            Ok(())
        }
    };
    if let Err(e) = publish_result {
        let _ = app
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::CircadianPause {
                    paused: prior_pause.0,
                    until: prior_pause.1,
                },
            })
            .await;
        return Err(ApiError::Internal(e));
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
    let description = format!(
        "{}: {} — {}. Circadian paused.{}",
        display_name, body.scene, description, ttl_desc
    );
    Ok(Json(ActionResponse {
        scene: Some(body.scene),
        brightness: Some(brightness),
        color_temp_k: Some(color_temp_k),
        ..ActionResponse::ok(&room_id, description)
    }))
}

/// List a room's Zigbee2MQTT scenes.
///
/// Scenes are stored in the room's Z2M group; create them in the Z2M frontend.
/// A room without a Z2M group returns an empty list.
#[utoipa::path(
    get,
    path = "/api/rooms/{room_id}/z2m-scenes",
    tag = "scenes",
    params(("room_id" = String, Path, description = "Room ID")),
    responses(
        (status = 200, description = "Scenes stored in the room's Z2M group", body = Z2mSceneList),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
        (status = 404, description = "Unknown room", body = ErrorResponse),
    )
)]
pub async fn list_z2m_scenes(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
) -> Result<Json<Z2mSceneList>, ApiError> {
    app.validate_room(&room_id)?;
    let group_name = app
        .config
        .rooms
        .get(&room_id)
        .and_then(|r| r.z2m_group.as_deref());

    let Some(group_name) = group_name else {
        return Ok(Json(Z2mSceneList {
            room: room_id,
            z2m_group: None,
            scenes: Vec::new(),
            description: "This room has no Z2M group — Z2M scenes require a group.".to_string(),
        }));
    };

    let current = app.state.load();
    let scenes: Vec<Z2mScene> = current
        .group_map
        .get(group_name)
        .map(|g| {
            g.scenes
                .iter()
                .map(|s| Z2mScene {
                    id: s.id,
                    name: s.name.clone(),
                })
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

    Ok(Json(Z2mSceneList {
        room: room_id,
        z2m_group: Some(group_name.to_string()),
        scenes,
        description: desc,
    }))
}

/// Recall a Zigbee2MQTT scene.
///
/// Pauses circadian for the room.
#[utoipa::path(
    post,
    path = "/api/rooms/{room_id}/z2m-scenes/recall",
    tag = "scenes",
    params(("room_id" = String, Path, description = "Room ID")),
    request_body = RecallZ2mSceneRequest,
    responses(
        (status = 200, description = "Done", body = ActionResponse),
        (status = 400, description = "Room has no Z2M group, or invalid body", body = ErrorResponse),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
        (status = 404, description = "Unknown room or scene", body = ErrorResponse),
        (status = 500, description = "Publishing to Zigbee2MQTT failed", body = ErrorResponse),
    )
)]
pub async fn recall_z2m_scene(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    ApiJson(body): ApiJson<RecallZ2mSceneRequest>,
) -> Result<Json<ActionResponse>, ApiError> {
    app.validate_room(&room_id)?;
    validate_override_ttl(body.override_ttl_mins)?;
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

    // Snapshot the room before mutating, so a failed publish rolls back the
    // pause / Manual source / override TTL instead of leaving them applied.
    let prior = app
        .state
        .load()
        .rooms
        .get(&room_id)
        .cloned()
        .unwrap_or_default();

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

    if let Err(e) = app
        .publisher
        .recall_scene_group(group_name, body.scene_id)
        .await
    {
        // Restore the TTL first so RestoreLights sees an unchanged
        // manual_override_until and restores update_source too.
        let _ = app
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::ManualOverrideTtl {
                    until: prior.manual_override_until,
                },
            })
            .await;
        let _ = app
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: room_id.to_string(),
                update: RoomStateUpdate::RestoreLights(Box::new(prior)),
            })
            .await;
        return Err(ApiError::Internal(e.to_string()));
    }

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

    Ok(Json(ActionResponse {
        z2m_scene: Some(scene_label.to_string()),
        scene_id: Some(body.scene_id),
        ..ActionResponse::ok(
            &room_id,
            format!(
                "{}: recalled Z2M scene '{}'. Circadian paused.{}",
                display_name, scene_label, ttl_desc
            ),
        )
    }))
}

/// Switch night mode on or off.
///
/// Night mode sets the room to its dim, warm night values and pauses
/// circadian; switching it off resumes circadian.
#[utoipa::path(
    post,
    path = "/api/rooms/{room_id}/night-mode",
    tag = "night_mode",
    params(("room_id" = String, Path, description = "Room ID")),
    request_body = SetNightModeRequest,
    responses(
        (status = 200, description = "Done", body = ActionResponse),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
        (status = 404, description = "Unknown room", body = ErrorResponse),
        (status = 500, description = "Publishing to Zigbee2MQTT failed", body = ErrorResponse),
    )
)]
pub async fn set_night_mode(
    State(app): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    ApiJson(body): ApiJson<SetNightModeRequest>,
) -> Result<Json<ActionResponse>, ApiError> {
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
            crate::night_mode::NightModeTrigger::User,
        )
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        let enm = room_config.effective_night_mode(&app.config.night_mode.defaults);
        Ok(Json(ActionResponse {
            night_mode: Some(true),
            ..ActionResponse::ok(
                &room_id,
                format!(
                    "{}: night mode on (brightness {}, {}K). Circadian paused.",
                    display_name, enm.brightness, enm.color_temp_k
                ),
            )
        }))
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

        Ok(Json(ActionResponse {
            night_mode: Some(false),
            ..ActionResponse::ok(
                &room_id,
                format!("{}: night mode off. Circadian resumed.", display_name),
            )
        }))
    }
}

/// Circadian status of every room.
#[utoipa::path(
    get,
    path = "/api/circadian",
    tag = "circadian",
    responses(
        (status = 200, description = "Circadian status per room, sorted by display name", body = Vec<RoomCircadian>),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
    )
)]
pub async fn get_circadian_status(State(app): State<Arc<AppState>>) -> Json<Vec<RoomCircadian>> {
    let current = app.state.load();
    let mut rooms: Vec<RoomCircadian> = current
        .rooms
        .iter()
        .map(|(room_id, rs)| RoomCircadian {
            room_id: room_id.clone(),
            display_name: app.display_name(room_id),
            lights_on: rs.lights_on,
            brightness: rs.current_brightness,
            color_temp_mired: rs.current_color_temp_mired,
            circadian: app.describe_circadian_status(rs),
        })
        .collect();
    rooms.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Json(rooms)
}

/// System status.
///
/// MQTT and Zigbee2MQTT connectivity, uptime and device counts.
#[utoipa::path(
    get,
    path = "/api/system",
    tag = "system",
    responses(
        (status = 200, description = "System status", body = SystemStatus),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
    )
)]
pub async fn get_system_status(State(app): State<Arc<AppState>>) -> Json<SystemStatus> {
    let current = app.state.load();
    let uptime = current
        .started_at
        .map(|s| s.elapsed().as_secs())
        .unwrap_or(0);
    Json(SystemStatus {
        mqtt_connected: current.mqtt_connected,
        z2m_online: current.z2m_online,
        uptime: format_duration(uptime),
        uptime_secs: uptime,
        device_count: current.device_map.len(),
        group_count: current.group_map.len(),
        room_count: current.rooms.len(),
        version: VERSION.to_string(),
    })
}

/// Health check.
///
/// Always answers 200 while the daemon is running, and never requires a
/// token, so it can back a container health check.
#[utoipa::path(
    get,
    path = "/api/health",
    tag = "system",
    security(()),
    responses((status = 200, description = "jeha is running", body = HealthResponse))
)]
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: VERSION.to_string(),
    })
}

/// List the built-in scenes.
///
/// These can be applied to any room with `POST /api/rooms/{room_id}/scene`.
#[utoipa::path(
    get,
    path = "/api/scenes",
    tag = "scenes",
    responses(
        (status = 200, description = "Built-in scenes", body = Vec<SceneInfo>),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
    )
)]
pub async fn list_scenes() -> Json<Vec<SceneInfo>> {
    Json(
        SCENES
            .iter()
            .map(|&(name, brightness, color_temp_k, description)| SceneInfo {
                name: name.to_string(),
                brightness,
                color_temp_k,
                description: description.to_string(),
            })
            .collect(),
    )
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

// --- House-wide schedule and night mode ---

impl AppState {
    fn schedule_response(&self, schedule: &ScheduleOverride) -> ScheduleResponse {
        let defaults = &self.config.circadian.defaults;
        let source = |o: &Option<String>| {
            if o.is_some() {
                ScheduleSource::Api
            } else {
                ScheduleSource::Config
            }
        };
        let mut rooms_with_own_schedule: Vec<String> = self
            .config
            .rooms
            .iter()
            .filter(|(_, r)| {
                r.circadian
                    .as_ref()
                    .is_some_and(|c| c.wake_time.is_some() || c.sleep_time.is_some())
            })
            .map(|(id, _)| id.clone())
            .collect();
        rooms_with_own_schedule.sort();
        let wake_time = schedule
            .wake_time
            .clone()
            .unwrap_or_else(|| defaults.wake_time.clone());
        let sleep_time = schedule
            .sleep_time
            .clone()
            .unwrap_or_else(|| defaults.sleep_time.clone());
        let description = format!(
            "Circadian wakes at {} and winds down for {}.",
            wake_time, sleep_time
        );
        ScheduleResponse {
            wake_time,
            wake_time_source: source(&schedule.wake_time),
            sleep_time,
            sleep_time_source: source(&schedule.sleep_time),
            rooms_with_own_schedule,
            description,
        }
    }
}

/// Get the house-wide circadian schedule.
#[utoipa::path(
    get,
    path = "/api/circadian/schedule",
    tag = "circadian",
    responses(
        (status = 200, description = "Current wake and sleep times", body = ScheduleResponse),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
    )
)]
pub async fn get_schedule(State(app): State<Arc<AppState>>) -> Json<ScheduleResponse> {
    let current = app.state.load();
    Json(app.schedule_response(&current.schedule_override))
}

/// Set the house-wide wake and sleep times.
///
/// Replaces `wake_time`/`sleep_time` from `circadian.defaults` for every room
/// that doesn't set its own in config.toml. Meant to be synced from another
/// system (e.g. Home Assistant) when the times vary from day to day. A field
/// left out goes back to the config value. Lights follow on the next circadian
/// update. The times are kept in memory only: after a restart jeha uses
/// config.toml again until they are set again.
#[utoipa::path(
    put,
    path = "/api/circadian/schedule",
    tag = "circadian",
    request_body = SetScheduleRequest,
    responses(
        (status = 200, description = "Schedule updated", body = ScheduleResponse),
        (status = 400, description = "Invalid time or body", body = ErrorResponse),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
    )
)]
pub async fn set_schedule(
    State(app): State<Arc<AppState>>,
    ApiJson(body): ApiJson<SetScheduleRequest>,
) -> Result<Json<ScheduleResponse>, ApiError> {
    for (field, value) in [
        ("wake_time", &body.wake_time),
        ("sleep_time", &body.sleep_time),
    ] {
        if let Some(v) = value {
            crate::schedule::TimeOfDay::from_hm_str(v)
                .map_err(|e| ApiError::BadRequest(format!("{}: {}", field, e)))?;
        }
    }
    let schedule = ScheduleOverride {
        wake_time: body.wake_time,
        sleep_time: body.sleep_time,
    };
    let _ = app
        .state_tx
        .send(StateCommand::SetScheduleOverride(schedule.clone()))
        .await;
    Ok(Json(app.schedule_response(&schedule)))
}

/// Reset the house-wide schedule to config.toml.
#[utoipa::path(
    delete,
    path = "/api/circadian/schedule",
    tag = "circadian",
    responses(
        (status = 200, description = "Schedule reset", body = ScheduleResponse),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
    )
)]
pub async fn reset_schedule(
    State(app): State<Arc<AppState>>,
) -> Result<Json<ScheduleResponse>, ApiError> {
    let schedule = ScheduleOverride::default();
    let _ = app
        .state_tx
        .send(StateCommand::SetScheduleOverride(schedule.clone()))
        .await;
    Ok(Json(app.schedule_response(&schedule)))
}

/// Switch night mode on or off for the whole house.
///
/// Same as `POST /api/rooms/{room_id}/night-mode` for every room, so an
/// automation doesn't need updating when rooms are added. A room that fails
/// doesn't stop the others; it's listed under `failed`.
#[utoipa::path(
    post,
    path = "/api/night-mode",
    tag = "night_mode",
    request_body = SetHouseNightModeRequest,
    responses(
        (status = 200, description = "Night mode switched in all or some rooms", body = HouseNightModeResponse),
        (status = 400, description = "Invalid request body", body = ErrorResponse),
        (status = 401, description = "Missing or wrong API token", body = ErrorResponse),
        (status = 500, description = "No room could be switched", body = ErrorResponse),
    )
)]
pub async fn set_house_night_mode(
    State(app): State<Arc<AppState>>,
    ApiJson(body): ApiJson<SetHouseNightModeRequest>,
) -> Result<Json<HouseNightModeResponse>, ApiError> {
    let mut room_ids: Vec<&String> = app.config.rooms.keys().collect();
    room_ids.sort();

    let mut rooms = Vec::new();
    let mut failed = Vec::new();
    for room_id in room_ids {
        let room_config = &app.config.rooms[room_id];
        let result = if body.active {
            crate::night_mode::activate_night_mode(
                room_id,
                room_config,
                &app.config,
                &app.publisher,
                &app.state,
                &app.state_tx,
                &app.event_bus,
                crate::night_mode::NightModeTrigger::User,
            )
            .await
        } else {
            crate::night_mode::deactivate_night_mode(
                room_id,
                room_config,
                &app.publisher,
                &app.state,
                &app.state_tx,
                &app.event_bus,
                &app.circadian_engine,
            )
            .await
        };
        match result {
            Ok(()) => rooms.push(room_id.clone()),
            Err(e) => failed.push(RoomError {
                room: room_id.clone(),
                error: e.to_string(),
            }),
        }
    }

    if rooms.is_empty() && !failed.is_empty() {
        let errors: Vec<String> = failed
            .iter()
            .map(|f| format!("{}: {}", f.room, f.error))
            .collect();
        return Err(ApiError::Internal(format!(
            "Night mode failed in every room: {}",
            errors.join("; ")
        )));
    }

    let state_word = if body.active { "on" } else { "off" };
    let mut description = format!("Night mode {} in {} room(s).", state_word, rooms.len());
    if !failed.is_empty() {
        let names: Vec<&str> = failed.iter().map(|f| f.room.as_str()).collect();
        description.push_str(&format!(" Failed in: {}.", names.join(", ")));
    }
    Ok(Json(HouseNightModeResponse {
        status: if failed.is_empty() { "ok" } else { "partial" }.to_string(),
        night_mode: body.active,
        rooms,
        failed,
        description,
    }))
}
