use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::types::AppConfig;
use crate::mqtt::publish::Publisher;
use crate::schedule::LocalNow;
use crate::state::{RoomStateUpdate, SharedState, StateCommand};

pub struct LightsOutTask {
    config: Arc<AppConfig>,
    state: SharedState,
    state_tx: tokio::sync::mpsc::Sender<StateCommand>,
    publisher: Arc<Publisher>,
    cancel: CancellationToken,
}

impl LightsOutTask {
    pub fn new(
        config: Arc<AppConfig>,
        state: SharedState,
        state_tx: tokio::sync::mpsc::Sender<StateCommand>,
        publisher: Arc<Publisher>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            state,
            state_tx,
            publisher,
            cancel,
        }
    }

    pub async fn run(self) {
        if !self.config.lights_out.enabled {
            info!("Lights-out disabled in config");
            return;
        }

        let target_minutes = {
            let tod = crate::schedule::TimeOfDay::from_hm_str(&self.config.lights_out.time)
                .unwrap_or_else(|_| crate::schedule::TimeOfDay::from_hm_str("01:00").unwrap());
            tod.as_minutes() as u32
        };
        info!(
            "Lights-out task started (target: {})",
            self.config.lights_out.time
        );

        let mut fired_today = false;
        let mut interval = tokio::time::interval(Duration::from_secs(30));

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("Lights-out task shutting down");
                    return;
                }
                _ = interval.tick() => {
                    let now = LocalNow::now(&self.config.general.timezone);
                    let now_minutes = now.minutes as u32;

                    // Reset the flag when we move past the target window
                    if now_minutes.abs_diff(target_minutes) > 2 {
                        fired_today = false;
                    }

                    // Fire if we're within the target minute and haven't fired yet
                    if !fired_today && now_minutes == target_minutes {
                        fired_today = true;
                        if let Err(e) = self.turn_off_all().await {
                            warn!("Lights-out failed: {}", e);
                        }
                    }
                }
            }
        }
    }

    async fn turn_off_all(&self) -> Result<()> {
        let current = self.state.load();

        for (room_id, room_config) in &self.config.rooms {
            if !room_config.lights_out {
                debug!("Skipping room '{}': lights_out disabled", room_id);
                continue;
            }

            let lights_on = current
                .rooms
                .get(room_id)
                .map(|r| r.lights_on)
                .unwrap_or(false);

            if !lights_on {
                continue;
            }

            info!("Lights-out: turning off '{}'", room_id);

            if let Some(ref group) = room_config.z2m_group {
                self.publisher.turn_off_group(group, Some(5)).await?;
            } else {
                for ieee in &room_config.lights {
                    let _ = self.publisher.turn_off_ieee(ieee, Some(5)).await;
                }
            }

            let _ = self
                .state_tx
                .send(StateCommand::UpdateRoomState {
                    room_id: room_id.clone(),
                    update: RoomStateUpdate::LightsOff,
                })
                .await;
        }

        info!("Lights-out complete");
        Ok(())
    }
}
