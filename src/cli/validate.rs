use std::path::Path;

use anyhow::Result;
use tracing::info;

use crate::config;

pub fn run_validate(config_path: &Path, _check_devices: bool) -> Result<()> {
    let cfg = config::load_config(config_path)?;
    info!(
        "Config valid: {} rooms, {} automations",
        cfg.rooms.len(),
        cfg.automations.len()
    );

    for (room_id, room) in &cfg.rooms {
        let display_name = room.display_name.as_deref().unwrap_or(room_id);
        let group = room.z2m_group.as_deref().unwrap_or("(none)");
        info!(
            "  Room '{}' ({}): group={}, lights={}, motion_sensor={}",
            room_id,
            display_name,
            group,
            room.lights.len(),
            room.motion_sensor.as_deref().unwrap_or("none")
        );
    }

    for auto in &cfg.automations {
        info!(
            "  Automation '{}': rooms={:?}, trigger={:?}",
            auto.id, auto.rooms, auto.trigger
        );
    }

    println!("Config is valid.");
    Ok(())
}
