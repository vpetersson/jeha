use crate::schedule::Schedule;

use super::types::*;

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 1883,
            base_topic: "zigbee2mqtt".to_string(),
            username: None,
            password: None,
        }
    }
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            timezone: "UTC".to_string(),
            motion_timeout_secs: 300,
            external_brightness_tolerance: 15,
            external_color_temp_tolerance: 25,
            external_override_secs: 1800,
            manual_override_ttl_mins: 60,
            remote_brightness_step: 25,
            illuminance_threshold: 100,
            latitude: None,
            geoip_lookup: true,
        }
    }
}

impl Default for CircadianDefaults {
    fn default() -> Self {
        Self {
            wake_time: "06:00".to_string(),
            sleep_time: "23:00".to_string(),
            start_temp_k: 2700,
            peak_temp_k: 4000,
            end_temp_k: 2200,
            start_brightness: 150,
            peak_brightness: 254,
            end_brightness: 77,
            ramp_duration_mins: 180,
            curve: CurveType::Cosine,
            transition_secs: 30,
            update_interval_secs: 60,
            winter: WinterBrightness::default(),
        }
    }
}

const DEFAULT_WINTER_START_BRIGHTNESS: u8 = 230;
const DEFAULT_WINTER_PEAK_BRIGHTNESS: u8 = 254;

impl CircadianDefaults {
    /// Resolved (start, peak, end) brightness for the darkest days. Unset values
    /// only ever lift the morning and day, and never touch bedtime: the end
    /// target follows the regular `end_brightness` unless set explicitly.
    pub fn winter_targets(&self) -> (u8, u8, u8) {
        let w = &self.winter;
        (
            w.start_brightness
                .unwrap_or(DEFAULT_WINTER_START_BRIGHTNESS.max(self.start_brightness)),
            w.peak_brightness
                .unwrap_or(DEFAULT_WINTER_PEAK_BRIGHTNESS.max(self.peak_brightness)),
            w.end_brightness.unwrap_or(self.end_brightness),
        )
    }
}

impl Default for NightModeDefaults {
    fn default() -> Self {
        Self {
            color_temp_k: 2000,
            brightness: 2,
            motion_timeout_secs: 120,
            turn_on_when_off: true,
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

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: "127.0.0.1:8420".to_string(),
            token: None,
            docs: true,
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
                winter: match &o.winter {
                    Some(w) => WinterBrightness {
                        start_brightness: w.start_brightness.or(d.winter.start_brightness),
                        peak_brightness: w.peak_brightness.or(d.winter.peak_brightness),
                        end_brightness: w.end_brightness.or(d.winter.end_brightness),
                    },
                    None => d.winter,
                },
            },
            None => d,
        }
    }

    pub fn effective_night_mode(&self, defaults: &NightModeDefaults) -> EffectiveNightMode {
        // A room's own timeout beats the global night default: a room set to
        // 15 minutes must not silently drop to the 2-minute night default.
        let motion_timeout_secs = self
            .night_mode
            .as_ref()
            .and_then(|nm| nm.motion_timeout_secs)
            .or(self.motion_timeout_secs)
            .unwrap_or(defaults.motion_timeout_secs);
        match &self.night_mode {
            Some(nm) => EffectiveNightMode {
                schedule: nm.schedule.clone(),
                color_temp_k: nm.color_temp_k.unwrap_or(defaults.color_temp_k),
                brightness: nm.brightness.unwrap_or(defaults.brightness),
                motion_timeout_secs,
                turn_on_when_off: nm.turn_on_when_off.unwrap_or(defaults.turn_on_when_off),
            },
            None => EffectiveNightMode {
                schedule: None,
                color_temp_k: defaults.color_temp_k,
                brightness: defaults.brightness,
                motion_timeout_secs,
                turn_on_when_off: defaults.turn_on_when_off,
            },
        }
    }
}

pub struct EffectiveNightMode {
    pub schedule: Option<Schedule>,
    pub color_temp_k: u16,
    pub brightness: u8,
    pub motion_timeout_secs: u64,
    pub turn_on_when_off: bool,
}
