use super::types::*;

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 1883,
            base_topic: "zigbee2mqtt".to_string(),
        }
    }
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            timezone: "UTC".to_string(),
        }
    }
}

impl Default for CircadianDefaults {
    fn default() -> Self {
        Self {
            wake_time: "06:00".to_string(),
            sleep_time: "23:00".to_string(),
            start_temp_k: 2700,
            peak_temp_k: 4500,
            end_temp_k: 2200,
            start_brightness: 180,
            peak_brightness: 254,
            end_brightness: 120,
            ramp_duration_mins: 120,
            curve: CurveType::Cosine,
            transition_secs: 30,
            update_interval_secs: 60,
        }
    }
}

impl Default for NightModeDefaults {
    fn default() -> Self {
        Self {
            color_temp_k: 2200,
            brightness: 20,
            motion_timeout_secs: 120,
        }
    }
}

impl Default for LightsOutConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            time: "01:00".to_string(),
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8420".to_string(),
        }
    }
}

impl RoomConfig {
    pub fn effective_circadian(&self, defaults: &CircadianDefaults) -> CircadianDefaults {
        let d = defaults.clone();
        match &self.circadian {
            Some(o) => CircadianDefaults {
                wake_time: o.wake_time.clone().unwrap_or(d.wake_time),
                sleep_time: o.sleep_time.clone().unwrap_or(d.sleep_time),
                start_temp_k: o.start_temp_k.unwrap_or(d.start_temp_k),
                peak_temp_k: o.peak_temp_k.unwrap_or(d.peak_temp_k),
                end_temp_k: o.end_temp_k.unwrap_or(d.end_temp_k),
                start_brightness: o.start_brightness.unwrap_or(d.start_brightness),
                peak_brightness: o.peak_brightness.unwrap_or(d.peak_brightness),
                end_brightness: o.end_brightness.unwrap_or(d.end_brightness),
                ramp_duration_mins: o.ramp_duration_mins.unwrap_or(d.ramp_duration_mins),
                curve: o.curve.unwrap_or(d.curve),
                transition_secs: o.transition_secs.unwrap_or(d.transition_secs),
                update_interval_secs: o.update_interval_secs.unwrap_or(d.update_interval_secs),
            },
            None => d,
        }
    }

    pub fn effective_night_mode(&self, defaults: &NightModeDefaults) -> Option<EffectiveNightMode> {
        self.night_mode.as_ref().map(|nm| EffectiveNightMode {
            start_time: nm.start_time.clone().unwrap_or_else(|| "23:00".to_string()),
            end_time: nm.end_time.clone().unwrap_or_else(|| "06:00".to_string()),
            color_temp_k: nm.color_temp_k.unwrap_or(defaults.color_temp_k),
            brightness: nm.brightness.unwrap_or(defaults.brightness),
            motion_timeout_secs: nm
                .motion_timeout_secs
                .unwrap_or(defaults.motion_timeout_secs),
        })
    }
}

pub struct EffectiveNightMode {
    pub start_time: String,
    pub end_time: String,
    pub color_temp_k: u16,
    pub brightness: u8,
    pub motion_timeout_secs: u64,
}
