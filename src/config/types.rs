use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::schedule::Schedule;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppConfig {
    pub schema_version: u32,
    #[serde(default)]
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub circadian: CircadianSection,
    #[serde(default)]
    pub night_mode: NightModeSection,
    #[serde(default)]
    pub lights_out: LightsOutConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub rooms: HashMap<String, RoomConfig>,
    #[serde(default)]
    pub automations: Vec<AutomationConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MqttConfig {
    #[serde(default = "default_mqtt_host")]
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    #[serde(default = "default_base_topic")]
    pub base_topic: String,
}

fn default_mqtt_host() -> String {
    "localhost".to_string()
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_base_topic() -> String {
    "zigbee2mqtt".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GeneralConfig {
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Default motion timeout for all rooms with motion sensors (seconds).
    #[serde(default = "default_motion_timeout_secs")]
    pub motion_timeout_secs: u64,
    /// Brightness drift tolerance for external change detection (0-254).
    /// Z2M echoes within this range are ignored. Default: 15.
    #[serde(default = "default_external_brightness_tolerance")]
    pub external_brightness_tolerance: u64,
    /// Color temp drift tolerance for external change detection (mired).
    /// Z2M echoes within this range are ignored. Default: 25.
    #[serde(default = "default_external_color_temp_tolerance")]
    pub external_color_temp_tolerance: u64,
    /// How long to pause circadian after detecting an external light change (seconds).
    /// Default: 1800 (30 minutes).
    #[serde(default = "default_external_override_secs")]
    pub external_override_secs: u64,
    /// Remote brightness step per click (1-254). Default: 25.
    #[serde(default = "default_remote_brightness_step")]
    pub remote_brightness_step: u8,
}

fn default_motion_timeout_secs() -> u64 {
    300
}

fn default_timezone() -> String {
    "UTC".to_string()
}

fn default_external_brightness_tolerance() -> u64 {
    15
}
fn default_external_color_temp_tolerance() -> u64 {
    25
}
fn default_external_override_secs() -> u64 {
    1800
}
fn default_remote_brightness_step() -> u8 {
    25
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct CircadianSection {
    #[serde(default)]
    pub defaults: CircadianDefaults,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CircadianDefaults {
    pub wake_time: String,
    pub sleep_time: String,
    pub start_temp_k: u16,
    pub peak_temp_k: u16,
    pub end_temp_k: u16,
    pub start_brightness: u8,
    pub peak_brightness: u8,
    pub end_brightness: u8,
    pub ramp_duration_mins: u32,
    pub curve: CurveType,
    pub transition_secs: u32,
    pub update_interval_secs: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CurveType {
    Cosine,
    Linear,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct NightModeSection {
    #[serde(default)]
    pub defaults: NightModeDefaults,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NightModeDefaults {
    pub color_temp_k: u16,
    pub brightness: u8,
    pub motion_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpConfig {
    #[serde(default = "default_mcp_bind")]
    pub bind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LightsOutConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_lights_out_time")]
    pub time: String,
}

fn default_lights_out_time() -> String {
    "01:00".to_string()
}

fn default_mcp_bind() -> String {
    "127.0.0.1:8420".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RoomConfig {
    pub display_name: Option<String>,
    pub z2m_group: Option<String>,
    #[serde(default)]
    pub lights: Vec<String>,
    pub motion_sensor: Option<String>,
    /// Per-room motion timeout override (seconds). When set alongside motion_sensor,
    /// jeha automatically turns lights on/off based on motion without needing automations.
    pub motion_timeout_secs: Option<u64>,
    /// Schedule gate for built-in motion handling. When set, motion events are only
    /// processed when the current time matches this schedule.
    pub motion_schedule: Option<Schedule>,
    /// Remote controls (IEEE addresses) bound to this room.
    /// Built-in handling: toggle on/off, dimming, arrow_right=night mode, arrow_left=day mode.
    #[serde(default)]
    pub remotes: Vec<String>,
    /// Set to false to disable circadian for this room entirely.
    #[serde(default = "default_true")]
    pub circadian_enabled: bool,
    /// Set to false to exclude this room from automatic lights-out.
    #[serde(default = "default_true")]
    pub lights_out: bool,
    pub circadian: Option<CircadianOverride>,
    pub night_mode: Option<NightModeOverride>,
}

impl RoomConfig {
    /// Returns the effective motion timeout for this room, or None if
    /// the room has no motion sensor.
    pub fn effective_motion_timeout(&self, global_default: u64) -> Option<u64> {
        self.motion_sensor.as_ref()?;
        Some(self.motion_timeout_secs.unwrap_or(global_default))
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CircadianOverride {
    pub wake_time: Option<String>,
    pub sleep_time: Option<String>,
    pub start_temp_k: Option<u16>,
    pub peak_temp_k: Option<u16>,
    pub end_temp_k: Option<u16>,
    pub start_brightness: Option<u8>,
    pub peak_brightness: Option<u8>,
    pub end_brightness: Option<u8>,
    pub ramp_duration_mins: Option<u32>,
    pub curve: Option<CurveType>,
    pub transition_secs: Option<u32>,
    pub update_interval_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NightModeOverride {
    /// Schedule predicate for automatic night mode activation/deactivation.
    pub schedule: Option<Schedule>,
    pub color_temp_k: Option<u16>,
    pub brightness: Option<u8>,
    pub motion_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AutomationConfig {
    pub id: String,
    pub rooms: Vec<String>,
    pub trigger: TriggerConfig,
    pub action: ActionConfig,
    pub off_action: Option<ActionConfig>,
    /// Schedule gate: automation only fires when the current time matches.
    pub schedule: Option<Schedule>,
    #[serde(default)]
    pub conditions: Vec<crate::automation::condition::ConditionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerConfig {
    Motion,
    MotionCleared,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionConfig {
    LightsOn {
        #[serde(default)]
        use_circadian: bool,
        brightness: Option<u8>,
        color_temp_k: Option<u16>,
        transition: Option<u32>,
    },
    LightsOff {
        #[serde(default)]
        delay_secs: u64,
        transition: Option<u32>,
    },
    SetBrightness {
        brightness: u8,
        transition: Option<u32>,
    },
    SetColorTemp {
        color_temp_k: u16,
        transition: Option<u32>,
    },
}
