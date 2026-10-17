//! Request and response bodies for the REST API.
//!
//! Every type here derives `ToSchema`, so the OpenAPI document served at
//! `/api/openapi.json` is generated from the same structs the handlers
//! serialize — the spec can't drift from what the API actually returns.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// --- Requests ---

/// Turn a room's lights on. All fields are optional. Without `brightness` or
/// `color_temp_k`, only `state: ON` is sent and circadian control of the room
/// is restored; the lights move to the circadian target on the next circadian
/// update unless circadian is paused or snoozed.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct LightOnRequest {
    /// Brightness, 1-254. Setting this (or `color_temp_k`) is a manual override.
    #[schema(minimum = 1, maximum = 254, example = 200)]
    pub brightness: Option<u8>,
    /// Colour temperature in Kelvin. Setting this (or `brightness`) is a manual override.
    #[schema(minimum = 1000, maximum = 10000, example = 2700)]
    pub color_temp_k: Option<u16>,
    /// Transition time in seconds.
    #[schema(example = 2)]
    pub transition: Option<u32>,
    /// Minutes until a manual override expires and circadian takes over again.
    /// Without it, the override lasts until circadian is resumed.
    #[schema(exclusive_minimum = 0.0, maximum = 10080.0, example = 60.0)]
    pub override_ttl_mins: Option<f64>,
}

/// Turn a room's lights off. The body is optional.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct LightOffRequest {
    /// Transition time in seconds.
    #[schema(example = 2)]
    pub transition: Option<u32>,
}

/// Pause circadian adjustments for a fixed time.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SnoozeRequest {
    /// Hours to snooze for, greater than 0 and at most 24.
    #[schema(exclusive_minimum = 0.0, maximum = 24.0, example = 2.0)]
    pub hours: f64,
}

/// Apply one of jeha's built-in scenes (see `GET /api/scenes`).
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetSceneRequest {
    /// Scene name.
    #[schema(example = "relax")]
    pub scene: String,
    /// Minutes until circadian takes over again. Without it, the scene stays
    /// until circadian is resumed.
    #[schema(exclusive_minimum = 0.0, maximum = 10080.0)]
    pub override_ttl_mins: Option<f64>,
}

/// Recall a scene stored in the room's Zigbee2MQTT group.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RecallZ2mSceneRequest {
    /// Z2M scene ID (see `GET /api/rooms/{room_id}/z2m-scenes`).
    #[schema(example = 1)]
    pub scene_id: u16,
    /// Minutes until circadian takes over again.
    #[schema(exclusive_minimum = 0.0, maximum = 10080.0)]
    pub override_ttl_mins: Option<f64>,
}

/// Switch night mode on or off.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetNightModeRequest {
    pub active: bool,
}

// --- Responses ---

/// Error body returned with every 4xx/5xx response.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorResponse {
    #[schema(example = "Room 'attic' not found. Available rooms: kitchen, office")]
    pub error: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HealthResponse {
    #[schema(example = "ok")]
    pub status: String,
    /// jeha version.
    #[schema(example = "26.10.0")]
    pub version: String,
}

/// - `active`: lights follow the circadian curve
/// - `manual_override`: brightness or colour was set by hand; see `remaining_mins`
/// - `snoozed`: paused for a fixed time; see `remaining_secs`
/// - `paused`: paused until resumed
/// - `disabled`: `circadian_enabled = false` for the room
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CircadianState {
    Active,
    ManualOverride,
    Snoozed,
    Paused,
    Disabled,
}

/// Whether jeha is currently steering a room's lights, and for how long not.
#[derive(Debug, Serialize, ToSchema)]
pub struct CircadianStatus {
    pub status: CircadianState,
    /// Human-readable summary, suitable for showing to a user or an LLM.
    pub description: String,
    /// Minutes until a manual override expires (`manual_override` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_mins: Option<u64>,
    /// Snooze time left, formatted like `1h 20m` (`snoozed` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<String>,
    /// Snooze time left in seconds (`snoozed` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_secs: Option<u64>,
}

impl CircadianStatus {
    pub fn new(status: CircadianState, description: impl Into<String>) -> Self {
        Self {
            status,
            description: description.into(),
            remaining_mins: None,
            remaining: None,
            remaining_secs: None,
        }
    }
}

/// A room as listed by `GET /api/rooms`.
#[derive(Debug, Serialize, ToSchema)]
pub struct RoomSummary {
    #[schema(example = "kitchen")]
    pub room_id: String,
    #[schema(example = "Kitchen")]
    pub display_name: String,
    pub lights_on: bool,
    /// Last known brightness, 0-254.
    pub brightness: Option<u8>,
    /// Last known colour temperature in mired.
    pub color_temp_mired: Option<u16>,
    pub occupancy: bool,
    pub night_mode: bool,
    pub circadian: CircadianStatus,
}

/// Full detail for one room.
#[derive(Debug, Serialize, ToSchema)]
pub struct RoomDetail {
    #[schema(example = "kitchen")]
    pub room_id: String,
    #[schema(example = "Kitchen")]
    pub display_name: String,
    pub z2m_group: Option<String>,
    pub lights_on: bool,
    pub brightness: Option<u8>,
    pub color_temp_mired: Option<u16>,
    pub occupancy: bool,
    pub has_motion_sensor: bool,
    /// Seconds without motion before the lights turn off. Null without a motion sensor.
    pub motion_timeout_secs: Option<u64>,
    /// IEEE addresses of the remotes bound to this room.
    pub remotes: Vec<String>,
    pub night_mode: bool,
    pub circadian_enabled: bool,
    pub circadian: CircadianStatus,
}

/// Circadian state of one room, as listed by `GET /api/circadian`.
#[derive(Debug, Serialize, ToSchema)]
pub struct RoomCircadian {
    pub room_id: String,
    pub display_name: String,
    pub lights_on: bool,
    pub brightness: Option<u8>,
    pub color_temp_mired: Option<u16>,
    pub circadian: CircadianStatus,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SystemStatus {
    pub mqtt_connected: bool,
    /// Whether the Zigbee2MQTT bridge reports itself online.
    pub z2m_online: bool,
    #[schema(example = "3h 12m")]
    pub uptime: String,
    pub uptime_secs: u64,
    pub device_count: usize,
    pub group_count: usize,
    pub room_count: usize,
    #[schema(example = "26.10.0")]
    pub version: String,
}

/// Result of a state-changing room action. Fields beyond `status`, `room` and
/// `description` are only present for the actions they belong to.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct ActionResponse {
    #[schema(example = "ok")]
    pub status: String,
    #[schema(example = "kitchen")]
    pub room: String,
    /// Human-readable summary of what happened.
    pub description: String,
    /// New circadian state (`pause`, `resume`, `snooze`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "paused")]
    pub circadian: Option<String>,
    /// Snooze duration, formatted like `1h 30m` (`snooze`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    /// Scene applied (`scene`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
    /// Brightness applied (`scene`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brightness: Option<u8>,
    /// Colour temperature applied (`scene`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_temp_k: Option<u16>,
    /// Name of the recalled Z2M scene (`z2m-scenes/recall`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub z2m_scene: Option<String>,
    /// ID of the recalled Z2M scene (`z2m-scenes/recall`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scene_id: Option<u16>,
    /// New night mode state (`night-mode`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub night_mode: Option<bool>,
}

impl ActionResponse {
    pub fn ok(room: &str, description: impl Into<String>) -> Self {
        Self {
            status: "ok".to_string(),
            room: room.to_string(),
            description: description.into(),
            ..Default::default()
        }
    }
}

/// A built-in jeha scene.
#[derive(Debug, Serialize, ToSchema)]
pub struct SceneInfo {
    #[schema(example = "relax")]
    pub name: String,
    pub brightness: u8,
    pub color_temp_k: u16,
    #[schema(example = "Relaxed — dimmer, warm white")]
    pub description: String,
}

/// A scene stored in a Zigbee2MQTT group.
#[derive(Debug, Serialize, ToSchema)]
pub struct Z2mScene {
    pub id: u16,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Z2mSceneList {
    pub room: String,
    /// The room's Z2M group. Absent when the room has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub z2m_group: Option<String>,
    pub scenes: Vec<Z2mScene>,
    pub description: String,
}

/// Set the house-wide wake and sleep times. A field that is left out falls
/// back to `circadian.defaults` in config.toml.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetScheduleRequest {
    /// Wake time, `HH:MM` (24h).
    #[schema(example = "06:45")]
    pub wake_time: Option<String>,
    /// Sleep time, `HH:MM` (24h).
    #[schema(example = "22:30")]
    pub sleep_time: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleSource {
    /// Set through the API.
    Api,
    /// From `circadian.defaults` in config.toml.
    Config,
}

/// The house-wide circadian schedule.
#[derive(Debug, Serialize, ToSchema)]
pub struct ScheduleResponse {
    #[schema(example = "06:45")]
    pub wake_time: String,
    pub wake_time_source: ScheduleSource,
    #[schema(example = "22:30")]
    pub sleep_time: String,
    pub sleep_time_source: ScheduleSource,
    /// Rooms that set their own `wake_time` or `sleep_time` in config.toml.
    /// The house-wide schedule doesn't change those times for them.
    pub rooms_with_own_schedule: Vec<String>,
    pub description: String,
}

/// Switch night mode on or off for every room.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetHouseNightModeRequest {
    pub active: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RoomError {
    pub room: String,
    pub error: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HouseNightModeResponse {
    /// `ok` when every room switched, `partial` when some failed.
    #[schema(example = "ok")]
    pub status: String,
    pub night_mode: bool,
    /// Rooms that were switched.
    pub rooms: Vec<String>,
    /// Rooms that failed, with the reason.
    pub failed: Vec<RoomError>,
    pub description: String,
}

/// Change an alarm at runtime. Each field that is set replaces the value from
/// config.toml; a field left out goes back to the config value.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetAlarmRequest {
    /// When the lights are fully on, `HH:MM` (24h).
    #[schema(example = "07:15")]
    pub time: Option<String>,
    /// Days the alarm rings on. An empty list means every day.
    #[schema(example = json!(["mon", "tue", "wed", "thu", "fri"]))]
    pub days: Option<Vec<crate::schedule::DayOfWeek>>,
    /// Switch the alarm on or off.
    pub enabled: Option<bool>,
}

/// A wake-up alarm.
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmInfo {
    #[schema(example = "weekdays")]
    pub id: String,
    pub rooms: Vec<String>,
    /// When the lights are fully on, `HH:MM`.
    #[schema(example = "06:30")]
    pub time: String,
    /// Days the alarm rings on. Empty: every day.
    pub days: Vec<crate::schedule::DayOfWeek>,
    pub enabled: bool,
    /// Minutes the lights take to fade in, ending at `time`.
    pub fade_mins: u32,
    /// Brightness at the end of the fade. Absent: the circadian brightness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brightness: Option<u8>,
    /// Colour temperature at the end of the fade. Absent: the circadian value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_temp_k: Option<u16>,
    /// `api` when `time`, `days` or `enabled` were changed through the API.
    pub source: ScheduleSource,
    /// Next time the lights are fully on, local time (`YYYY-MM-DDTHH:MM`).
    /// Absent when the alarm is off.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "2026-10-19T06:30")]
    pub next: Option<String>,
    /// Rooms that are fading in right now.
    pub fading_rooms: Vec<String>,
    pub description: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmList {
    pub alarms: Vec<AlarmInfo>,
}
