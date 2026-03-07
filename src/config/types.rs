use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
}

fn default_timezone() -> String {
    "UTC".to_string()
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
    /// Set to false to disable circadian for this room entirely.
    #[serde(default = "default_true")]
    pub circadian_enabled: bool,
    /// Set to false to exclude this room from automatic lights-out.
    #[serde(default = "default_true")]
    pub lights_out: bool,
    pub circadian: Option<CircadianOverride>,
    pub night_mode: Option<NightModeOverride>,
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
    pub start_time: Option<String>,
    pub end_time: Option<String>,
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
    #[serde(default)]
    pub conditions: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerConfig {
    Motion,
    MotionCleared,
    Time { cron: String },
    StateChange { device: String, attribute: String },
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
