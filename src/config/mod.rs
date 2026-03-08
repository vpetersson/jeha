pub mod defaults;
pub mod migrate;
pub mod types;
pub mod validate;

use std::path::Path;

use anyhow::Result;
use types::AppConfig;

pub fn load_config(path: &Path) -> Result<AppConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: AppConfig = toml::from_str(&content)?;
    validate::validate_config(&config)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minimal_config() {
        let toml_str = r#"
schema_version = 1

[rooms.kitchen]
z2m_group = "Kitchen"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.schema_version, 1);
        assert!(config.rooms.contains_key("kitchen"));
        assert_eq!(config.circadian.defaults.peak_brightness, 254);
        validate::validate_config(&config).unwrap();
    }

    #[test]
    fn test_full_config() {
        let toml_str = r#"
schema_version = 1

[mqtt]
host = "localhost"
port = 1883
base_topic = "zigbee2mqtt"

[general]
timezone = "Europe/London"

[circadian.defaults]
wake_time = "06:00"
sleep_time = "23:00"
start_temp_k = 2700
peak_temp_k = 4000
end_temp_k = 2200
start_brightness = 180
peak_brightness = 254
end_brightness = 150
ramp_duration_mins = 120
curve = "cosine"
transition_secs = 30
update_interval_secs = 60

[night_mode.defaults]
color_temp_k = 2200
brightness = 20
motion_timeout_secs = 120

[mcp]
bind = "127.0.0.1:8420"

[rooms.kitchen]
display_name = "Kitchen"
z2m_group = "Kitchen"
lights = ["0x001788010AAAAAA1"]
motion_sensor = "0x00158d000AAAAAAA"

[rooms.kitchen.circadian]
peak_temp_k = 5500
peak_brightness = 254

[rooms.kitchen.night_mode]
schedule = { after = "22:30", before = "06:30" }

[rooms.office]
display_name = "Office"
z2m_group = "Office"

[[automations]]
id = "hallway_motion"
rooms = ["kitchen"]
[automations.trigger]
type = "motion"
[automations.action]
type = "lights_on"
use_circadian = true
[automations.off_action]
type = "lights_off"
delay_secs = 300
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        validate::validate_config(&config).unwrap();
        assert_eq!(config.rooms.len(), 2);
        assert_eq!(config.automations.len(), 1);
    }

    #[test]
    fn test_invalid_room_no_group_or_lights() {
        let toml_str = r#"
schema_version = 1

[rooms.bad_room]
display_name = "Bad"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(validate::validate_config(&config).is_err());
    }
}
