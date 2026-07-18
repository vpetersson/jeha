pub mod publish;
pub mod z2m;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::types::{AppConfig, MqttConfig};
use crate::event::{self, EventBus};
use crate::state::{SharedState, StateCommand};

pub struct MqttHandle {
    pub client: AsyncClient,
    event_loop: EventLoop,
    config: MqttConfig,
    app_config: Arc<AppConfig>,
    state: SharedState,
    state_tx: mpsc::Sender<StateCommand>,
    event_bus: EventBus,
}

impl MqttHandle {
    pub fn new(
        config: &MqttConfig,
        app_config: Arc<AppConfig>,
        state: SharedState,
        state_tx: mpsc::Sender<StateCommand>,
        event_bus: EventBus,
    ) -> Result<Self> {
        let mut opts = MqttOptions::new("jeha", &config.host, config.port);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(true);
        opts.set_max_packet_size(1024 * 1024, 1024 * 1024); // 1MB - Z2M bridge/devices can be large

        let (client, event_loop) = AsyncClient::new(opts, 256);

        Ok(Self {
            client,
            event_loop,
            config: config.clone(),
            app_config,
            state,
            state_tx,
            event_bus,
        })
    }

    async fn subscribe_z2m(client: &AsyncClient, base_topic: &str) -> Result<()> {
        client
            .subscribe(format!("{}/bridge/devices", base_topic), QoS::AtLeastOnce)
            .await?;
        client
            .subscribe(format!("{}/bridge/groups", base_topic), QoS::AtLeastOnce)
            .await?;
        client
            .subscribe(format!("{}/bridge/state", base_topic), QoS::AtLeastOnce)
            .await?;
        client
            .subscribe(format!("{}/+", base_topic), QoS::AtLeastOnce)
            .await?;
        client
            .subscribe(format!("{}/+/availability", base_topic), QoS::AtLeastOnce)
            .await?;
        info!("Subscribed to Z2M topics under '{}'", base_topic);
        Ok(())
    }

    pub async fn run(mut self) {
        let base_topic = self.config.base_topic.clone();
        let client = self.client.clone();
        let room_lookup = z2m::RoomLookup::from_config(&self.app_config);

        const RECONNECT_DELAY_MIN: Duration = Duration::from_secs(1);
        const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(60);
        let mut reconnect_delay = RECONNECT_DELAY_MIN;

        loop {
            match self.event_loop.poll().await {
                Ok(event) => {
                    reconnect_delay = RECONNECT_DELAY_MIN;
                    if let Event::Incoming(Packet::Publish(publish)) = event {
                        debug!("MQTT message on topic: {}", publish.topic);
                        if let Err(e) = z2m::handle_message(
                            &publish.topic,
                            &publish.payload,
                            &base_topic,
                            &self.state,
                            &self.state_tx,
                            &self.event_bus,
                            &self.app_config,
                            &room_lookup,
                        )
                        .await
                        {
                            warn!("Error handling MQTT message on '{}': {}", publish.topic, e);
                        }
                    } else if let Event::Incoming(Packet::ConnAck(_)) = event {
                        info!("MQTT connected");
                        self.event_bus.publish(event::Event::MqttConnected);
                        let _ = self
                            .state_tx
                            .send(StateCommand::SetMqttConnected(true))
                            .await;
                        // Subscribe from a separate task: the client's request
                        // channel may be full of publishes queued during an
                        // outage, and it only drains via poll() in THIS task —
                        // awaiting subscribe() here would deadlock the loop.
                        let sub_client = client.clone();
                        let sub_topic = base_topic.clone();
                        tokio::spawn(async move {
                            if let Err(e) = Self::subscribe_z2m(&sub_client, &sub_topic).await {
                                error!("Failed to subscribe to Z2M topics: {}", e);
                            }
                        });
                    }
                }
                Err(e) => {
                    warn!(
                        "MQTT connection error: {}. Reconnecting in {:?}...",
                        e, reconnect_delay
                    );
                    self.event_bus.publish(event::Event::MqttDisconnected);
                    let _ = self
                        .state_tx
                        .send(StateCommand::SetMqttConnected(false))
                        .await;
                    tokio::time::sleep(reconnect_delay).await;
                    reconnect_delay = (reconnect_delay * 2).min(RECONNECT_DELAY_MAX);
                }
            }
        }
    }
}
