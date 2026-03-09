use anyhow::{Result, bail};
use tracing::warn;

use crate::schedule::{self, TimeOfDay};

use super::types::AppConfig;

pub fn validate_config(config: &AppConfig) -> Result<()> {
    if config.schema_version != 1 {
        bail!(
            "Unsupported schema_version: {}. Expected 1.",
            config.schema_version
        );
    }

    for (room_id, room) in &config.rooms {
        if room.z2m_group.is_none() && room.lights.is_empty() {
            bail!(
                "Room '{}' must have either z2m_group or lights (or both).",
                room_id
            );
        }

        for ieee in &room.lights {
            validate_ieee(ieee, &format!("rooms.{}.lights", room_id))?;
        }

        if let Some(ref sensor) = room.motion_sensor {
            validate_ieee(sensor, &format!("rooms.{}.motion_sensor", room_id))?;
        }

        for (i, remote) in room.remotes.iter().enumerate() {
            validate_ieee(remote, &format!("rooms.{}.remotes[{}]", room_id, i))?;
        }

        if room.motion_timeout_secs.is_some() && room.motion_sensor.is_none() {
            warn!(
                "Room '{}' has motion_timeout_secs but no motion_sensor — timeout will have no effect",
                room_id
            );
        }

        if let Some(ref sched) = room.motion_schedule {
            schedule::validate_schedule(sched, &format!("rooms.{}.motion_schedule", room_id))?;
        }

        if let Some(ref circ) = room.circadian {
            if let Some(ref wt) = circ.wake_time {
                validate_time_str(wt, &format!("rooms.{}.circadian.wake_time", room_id))?;
            }
            if let Some(ref st) = circ.sleep_time {
                validate_time_str(st, &format!("rooms.{}.circadian.sleep_time", room_id))?;
            }
            if let Some(temp) = circ.start_temp_k {
                validate_color_temp(temp, &format!("rooms.{}.circadian.start_temp_k", room_id))?;
            }
            if let Some(temp) = circ.peak_temp_k {
                validate_color_temp(temp, &format!("rooms.{}.circadian.peak_temp_k", room_id))?;
            }
            if let Some(temp) = circ.end_temp_k {
                validate_color_temp(temp, &format!("rooms.{}.circadian.end_temp_k", room_id))?;
            }
        }

        if let Some(ref nm) = room.night_mode
            && let Some(ref sched) = nm.schedule
        {
            schedule::validate_schedule(sched, &format!("rooms.{}.night_mode.schedule", room_id))?;
        }
    }

    validate_time_str(
        &config.circadian.defaults.wake_time,
        "circadian.defaults.wake_time",
    )?;
    validate_time_str(
        &config.circadian.defaults.sleep_time,
        "circadian.defaults.sleep_time",
    )?;
    validate_color_temp(
        config.circadian.defaults.start_temp_k,
        "circadian.defaults.start_temp_k",
    )?;
    validate_color_temp(
        config.circadian.defaults.peak_temp_k,
        "circadian.defaults.peak_temp_k",
    )?;
    validate_color_temp(
        config.circadian.defaults.end_temp_k,
        "circadian.defaults.end_temp_k",
    )?;

    // Validate light calibration
    if config.light_calibration.rgbw_color_temp_offset.abs() > 100 {
        bail!(
            "light_calibration.rgbw_color_temp_offset must be between -100 and 100, got {}",
            config.light_calibration.rgbw_color_temp_offset
        );
    }
    if config.light_calibration.rgbw_brightness_offset.abs() > 100 {
        bail!(
            "light_calibration.rgbw_brightness_offset must be between -100 and 100, got {}",
            config.light_calibration.rgbw_brightness_offset
        );
    }

    let all_lights: Vec<&str> = config
        .rooms
        .values()
        .flat_map(|r| r.lights.iter().map(|s| s.as_str()))
        .collect();

    for (ieee, ovr) in &config.light_calibration.overrides {
        validate_ieee(ieee, &format!("light_calibration.overrides.{}", ieee))?;
        if let Some(ct) = ovr.color_temp_offset
            && ct.abs() > 100
        {
            bail!(
                "light_calibration.overrides.{}.color_temp_offset must be between -100 and 100, got {}",
                ieee,
                ct
            );
        }
        if let Some(br) = ovr.brightness_offset
            && br.abs() > 100
        {
            bail!(
                "light_calibration.overrides.{}.brightness_offset must be between -100 and 100, got {}",
                ieee,
                br
            );
        }
        if !all_lights.contains(&ieee.as_str()) {
            warn!(
                "light_calibration.overrides contains IEEE {} which doesn't appear in any room's lights list",
                ieee
            );
        }
    }

    for automation in &config.automations {
        if automation.id.is_empty() {
            bail!("Automation must have a non-empty id.");
        }
        for room_ref in &automation.rooms {
            if !config.rooms.contains_key(room_ref) {
                bail!(
                    "Automation '{}' references unknown room '{}'.",
                    automation.id,
                    room_ref
                );
            }
        }
        if matches!(automation.trigger, super::types::TriggerConfig::Motion) {
            for room_ref in &automation.rooms {
                if let Some(room) = config.rooms.get(room_ref)
                    && room.motion_sensor.is_none()
                {
                    bail!(
                        "Automation '{}' uses motion trigger but room '{}' has no motion_sensor.",
                        automation.id,
                        room_ref
                    );
                }
            }
        }
        if let Some(ref sched) = automation.schedule {
            schedule::validate_schedule(sched, &format!("automations.{}.schedule", automation.id))?;
        }
    }

    Ok(())
}

fn validate_ieee(addr: &str, field: &str) -> Result<()> {
    if !addr.starts_with("0x") || addr.len() != 18 {
        bail!(
            "Invalid IEEE address '{}' in {}: must be 0x followed by 16 hex digits.",
            addr,
            field
        );
    }
    if !addr[2..].chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "Invalid IEEE address '{}' in {}: contains non-hex characters.",
            addr,
            field
        );
    }
    Ok(())
}

fn validate_time_str(time: &str, field: &str) -> Result<()> {
    TimeOfDay::from_hm_str(time).map_err(|e| anyhow::anyhow!("{} in {}", e, field))?;
    Ok(())
}

fn validate_color_temp(temp: u16, field: &str) -> Result<()> {
    if !(1000..=10000).contains(&temp) {
        bail!(
            "Invalid color temperature {} in {}: must be 1000-10000K.",
            temp,
            field
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_ieee_valid() {
        assert!(validate_ieee("0x00158d0004abcdef", "test").is_ok());
        assert!(validate_ieee("0x001788010AAAAAA1", "test").is_ok());
    }

    #[test]
    fn test_validate_ieee_invalid() {
        assert!(validate_ieee("not_an_ieee", "test").is_err());
        assert!(validate_ieee("0x123", "test").is_err());
        assert!(validate_ieee("0x00158d0004abcdeg", "test").is_err());
    }

    #[test]
    fn test_validate_time_str() {
        assert!(validate_time_str("06:00", "test").is_ok());
        assert!(validate_time_str("23:59", "test").is_ok());
        assert!(validate_time_str("24:00", "test").is_err());
        assert!(validate_time_str("12:60", "test").is_err());
        assert!(validate_time_str("noon", "test").is_err());
    }

    #[test]
    fn test_validate_color_temp() {
        assert!(validate_color_temp(2700, "test").is_ok());
        assert!(validate_color_temp(999, "test").is_err());
        assert!(validate_color_temp(10001, "test").is_err());
    }
}
