use std::sync::Arc;

use anyhow::Result;
use rumqttc::{AsyncClient, QoS};
use serde_json::json;
use tracing::{debug, warn};

use crate::calibration;
use crate::config::types::AppConfig;
use crate::state::SharedState;

use super::z2m;

pub struct Publisher {
    client: AsyncClient,
    base_topic: String,
    state: SharedState,
    config: Arc<AppConfig>,
}

impl Publisher {
    pub fn new(
        client: AsyncClient,
        base_topic: String,
        state: SharedState,
        config: Arc<AppConfig>,
    ) -> Self {
        Self {
            client,
            base_topic,
            state,
            config,
        }
    }

    pub async fn set_light_group(
        &self,
        group_name: &str,
        payload: &serde_json::Value,
    ) -> Result<()> {
        let topic = format!("{}/{}/set", self.base_topic, group_name);
        let data = serde_json::to_vec(payload)?;
        debug!("Publishing to {}: {}", topic, payload);
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, data)
            .await?;
        Ok(())
    }

    pub async fn set_light_ieee(&self, ieee: &str, payload: &serde_json::Value) -> Result<()> {
        let topic = z2m::resolve_topic(&self.state, ieee, &self.base_topic);
        match topic {
            Some(topic) => {
                let set_topic = format!("{}/set", topic);
                let data = serde_json::to_vec(payload)?;
                debug!("Publishing to {}: {}", set_topic, payload);
                self.client
                    .publish(&set_topic, QoS::AtLeastOnce, false, data)
                    .await?;
                Ok(())
            }
            None => {
                warn!("Cannot resolve IEEE {} to friendly name", ieee);
                Ok(())
            }
        }
    }

    pub async fn turn_on_group(
        &self,
        group_name: &str,
        brightness: Option<u8>,
        color_temp_mired: Option<u16>,
        transition: Option<u32>,
    ) -> Result<()> {
        let mut payload = json!({"state": "ON"});
        if let Some(b) = brightness {
            payload["brightness"] = json!(b);
        }
        if let Some(ct) = color_temp_mired {
            payload["color_temp"] = json!(ct);
        }
        if let Some(t) = transition {
            payload["transition"] = json!(t);
        }
        self.set_light_group(group_name, &payload).await
    }

    pub async fn turn_off_group(&self, group_name: &str, transition: Option<u32>) -> Result<()> {
        let mut payload = json!({"state": "OFF"});
        if let Some(t) = transition {
            payload["transition"] = json!(t);
        }
        self.set_light_group(group_name, &payload).await
    }

    pub async fn turn_on_ieee(
        &self,
        ieee: &str,
        brightness: Option<u8>,
        color_temp_mired: Option<u16>,
        transition: Option<u32>,
    ) -> Result<()> {
        let current = self.state.load();
        let cal = calibration::resolve_for_device(ieee, &self.config, &current.device_map);
        let device_info = current.device_map.get(ieee);

        let mut payload = json!({"state": "ON"});
        if let Some(b) = brightness {
            let calibrated = cal.apply_brightness(b);
            if calibrated != b {
                debug!(
                    "Calibration for {}: brightness {} -> {}",
                    ieee, b, calibrated
                );
            }
            payload["brightness"] = json!(calibrated);
        }
        if let Some(ct) = color_temp_mired {
            let calibrated = cal.apply_color_temp(ct, device_info);
            if calibrated != ct {
                debug!(
                    "Calibration for {}: color_temp {} -> {} mired",
                    ieee, ct, calibrated
                );
            }
            payload["color_temp"] = json!(calibrated);
        }
        if let Some(t) = transition {
            payload["transition"] = json!(t);
        }
        self.set_light_ieee(ieee, &payload).await
    }

    pub async fn turn_off_ieee(&self, ieee: &str, transition: Option<u32>) -> Result<()> {
        let mut payload = json!({"state": "OFF"});
        if let Some(t) = transition {
            payload["transition"] = json!(t);
        }
        self.set_light_ieee(ieee, &payload).await
    }

    pub async fn push_circadian_group(
        &self,
        group_name: &str,
        brightness: u8,
        color_temp_mired: u16,
        transition: u32,
    ) -> Result<()> {
        let payload = json!({
            "brightness": brightness,
            "color_temp": color_temp_mired,
            "transition": transition,
        });
        self.set_light_group(group_name, &payload).await
    }

    pub async fn recall_scene_group(&self, group_name: &str, scene_id: u16) -> Result<()> {
        let payload = json!({"scene_recall": scene_id});
        self.set_light_group(group_name, &payload).await
    }

    pub async fn push_circadian_brightness_only_group(
        &self,
        group_name: &str,
        brightness: u8,
        transition: u32,
    ) -> Result<()> {
        let payload = json!({
            "brightness": brightness,
            "transition": transition,
        });
        self.set_light_group(group_name, &payload).await
    }
}
