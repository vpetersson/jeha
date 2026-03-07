use chrono::{NaiveTime, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::state::SharedState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConditionConfig {
    TimeRange {
        after: String,
        before: String,
    },
    NightMode {
        room: String,
        active: bool,
    },
    LightState {
        room: String,
        #[serde(rename = "on")]
        is_on: bool,
    },
}

pub fn evaluate_condition(
    condition: &ConditionConfig,
    state: &SharedState,
    _timezone: &str,
) -> bool {
    match condition {
        ConditionConfig::TimeRange { after, before } => {
            let now = Utc::now();
            let current_time = NaiveTime::from_hms_opt(now.hour(), now.minute(), 0)
                .unwrap_or(NaiveTime::from_hms_opt(0, 0, 0).unwrap());
            let after_time = parse_naive_time(after);
            let before_time = parse_naive_time(before);

            if after_time <= before_time {
                current_time >= after_time && current_time < before_time
            } else {
                // Crosses midnight
                current_time >= after_time || current_time < before_time
            }
        }
        ConditionConfig::NightMode { room, active } => {
            let current = state.load();
            current
                .rooms
                .get(room)
                .map(|r| r.night_mode_active == *active)
                .unwrap_or(false)
        }
        ConditionConfig::LightState { room, is_on } => {
            let current = state.load();
            current
                .rooms
                .get(room)
                .map(|r| r.lights_on == *is_on)
                .unwrap_or(false)
        }
    }
}

fn parse_naive_time(s: &str) -> NaiveTime {
    let parts: Vec<&str> = s.split(':').collect();
    let hour = parts[0].parse().unwrap_or(0);
    let minute = parts.get(1).and_then(|m| m.parse().ok()).unwrap_or(0);
    NaiveTime::from_hms_opt(hour, minute, 0).unwrap_or(NaiveTime::from_hms_opt(0, 0, 0).unwrap())
}
