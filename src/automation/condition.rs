use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::state::SharedState;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConditionConfig {
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
) -> bool {
    match condition {
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
