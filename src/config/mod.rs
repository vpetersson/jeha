pub mod defaults;
pub mod migrate;
pub mod types;
pub mod validate;

use std::io;
use std::path::Path;

use anyhow::Result;
use types::AppConfig;

pub fn load_config(path: &Path) -> Result<AppConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: AppConfig = toml::from_str(&content)?;
    validate::validate_config(&config)?;
    Ok(config)
}

/// Write `content` to the config file, atomically where possible (temp file + rename).
///
/// When the config is a single-file bind mount (`./config.toml:/config.toml` in
/// Docker), the path is a mount point and can't be replaced by rename: the kernel
/// returns EBUSY (or EXDEV if the temp file landed on another filesystem). In that
/// case fall back to overwriting the file in place.
pub async fn write_config(path: &Path, content: &str) -> Result<()> {
    let tmp_path = path.with_extension("toml.tmp");
    let renamed = match tokio::fs::write(&tmp_path, content).await {
        Ok(()) => tokio::fs::rename(&tmp_path, path).await,
        Err(e) => Err(e),
    };
    match renamed {
        Ok(()) => Ok(()),
        Err(e) if needs_in_place_write(&e) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            tokio::fs::write(path, content).await?;
            Ok(())
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            Err(e.into())
        }
    }
}

/// Errors from the temp-file + rename path that an in-place write can still get past.
fn needs_in_place_write(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ResourceBusy
            | io::ErrorKind::CrossesDevices
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::ReadOnlyFilesystem
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_write_config_replaces_file() {
        let dir = std::env::temp_dir().join(format!("jeha-write-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "old = 1\n").unwrap();

        write_config(&path, "new = 2\n").await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new = 2\n");
        assert!(!path.with_extension("toml.tmp").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_bind_mount_errors_fall_back_to_in_place_write() {
        // EBUSY is what rename(2) returns when the target is a bind-mount point
        assert!(needs_in_place_write(&io::Error::from_raw_os_error(16)));
        // EXDEV: temp file on a different filesystem
        assert!(needs_in_place_write(&io::Error::from_raw_os_error(18)));
        assert!(!needs_in_place_write(&io::Error::from(
            io::ErrorKind::NotFound
        )));
    }

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

[api]
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

    #[test]
    fn test_winter_mode_config() {
        let toml_str = r#"
schema_version = 1

[general]
latitude = 59.33

[circadian.defaults.winter]
start_brightness = 240

[rooms.kitchen]
z2m_group = "Kitchen"

[rooms.bedroom]
z2m_group = "Bedroom"

[rooms.bedroom.circadian.winter]
end_brightness = 60
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        validate::validate_config(&config).unwrap();
        assert_eq!(config.general.latitude, Some(59.33));

        let defaults = &config.circadian.defaults;
        assert_eq!(defaults.winter_targets(), (240, 254, 77));

        let kitchen = config.rooms["kitchen"].effective_circadian(defaults);
        assert_eq!(kitchen.winter_targets(), (240, 254, 77));

        let bedroom = config.rooms["bedroom"].effective_circadian(defaults);
        assert_eq!(bedroom.winter_targets(), (240, 254, 60));
    }

    #[test]
    fn test_winter_defaults_follow_regular_curve() {
        let toml_str = r#"
schema_version = 1

[rooms.den]
z2m_group = "Den"

[rooms.den.circadian]
start_brightness = 250
end_brightness = 20
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let den = config.rooms["den"].effective_circadian(&config.circadian.defaults);
        // Bedtime keeps the room's own end brightness; the default winter start
        // never dims a room that is already brighter than it.
        assert_eq!(den.winter_targets(), (250, 254, 20));

        let defaults = &config.circadian.defaults;
        assert_eq!(defaults.winter_targets(), (230, 254, 77));
    }

    #[test]
    fn test_invalid_latitude() {
        let toml_str = r#"
schema_version = 1

[general]
latitude = 95.0
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(validate::validate_config(&config).is_err());
    }
}
