use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::api;
use crate::automation::AutomationEngine;
use crate::circadian::CircadianEngine;
use crate::config;
use crate::config_sync::ConfigSync;
use crate::event::EventBus;
use crate::lights_out::LightsOutTask;
use crate::mqtt::MqttHandle;
use crate::mqtt::publish::Publisher;
use crate::night_mode::NightModeScheduler;
use crate::state;

pub async fn run_daemon(
    config_path: &Path,
    mqtt_host: Option<String>,
    mqtt_port: Option<u16>,
    mqtt_topic: Option<String>,
    api_bind: Option<String>,
) -> Result<()> {
    // 1. Parse + validate config
    let mut loaded_config = config::load_config(config_path)?;

    // Apply CLI/env overrides
    if let Some(host) = mqtt_host {
        loaded_config.mqtt.host = host;
    }
    if let Some(port) = mqtt_port {
        loaded_config.mqtt.port = port;
    }
    if let Some(topic) = mqtt_topic {
        loaded_config.mqtt.base_topic = topic;
    }
    if let Some(bind) = api_bind {
        loaded_config.api.bind = bind;
    }

    let app_config = Arc::new(loaded_config);
    info!(
        "Config loaded: {} rooms, {} automations",
        app_config.rooms.len(),
        app_config.automations.len()
    );

    // Set up shared state
    let shared_state = state::new_shared_state();
    let (state_manager, state_tx) = state::StateManager::new(shared_state.clone());

    // Set start time and initialize room states
    {
        let mut initial = (**shared_state.load()).clone();
        initial.started_at = Some(Instant::now());
        for room_id in app_config.rooms.keys() {
            initial.rooms.entry(room_id.clone()).or_default();
        }
        shared_state.store(Arc::new(initial));
    }

    // Start state manager
    tokio::spawn(state_manager.run());

    let event_bus = EventBus::new(256);
    let cancel = CancellationToken::new();

    // 2. Connect MQTT
    let mqtt = MqttHandle::new(
        &app_config.mqtt,
        app_config.clone(),
        shared_state.clone(),
        state_tx.clone(),
        event_bus.clone(),
    )?;

    let mqtt_client = mqtt.client.clone();

    // Create publisher
    let publisher = Arc::new(Publisher::new(
        mqtt_client,
        app_config.mqtt.base_topic.clone(),
        shared_state.clone(),
        app_config.clone(),
    ));

    // 3-4. Start MQTT event loop (subscribes on connect)
    tokio::spawn(mqtt.run());

    // Wait a moment for MQTT to connect and receive retained messages
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // 5. Start circadian engine
    let circadian_for_automations = Arc::new(CircadianEngine::new(
        app_config.clone(),
        shared_state.clone(),
        state_tx.clone(),
        publisher.clone(),
        event_bus.clone(),
        cancel.child_token(),
    ));
    let circadian_runner = CircadianEngine::new(
        app_config.clone(),
        shared_state.clone(),
        state_tx.clone(),
        publisher.clone(),
        event_bus.clone(),
        cancel.child_token(),
    );
    tokio::spawn(circadian_runner.run());

    // 6. Start automation engine
    let automation = AutomationEngine::new(
        app_config.clone(),
        shared_state.clone(),
        state_tx.clone(),
        publisher.clone(),
        event_bus.clone(),
        cancel.child_token(),
        Some(circadian_for_automations.clone()),
    );
    tokio::spawn(automation.run());

    // 6b. Start config sync (auto-discover new Z2M groups)
    let config_sync = ConfigSync::new(
        app_config.clone(),
        config_path,
        shared_state.clone(),
        event_bus.clone(),
        cancel.child_token(),
    );
    tokio::spawn(config_sync.run());

    // 6c. Start lights-out task
    let lights_out = LightsOutTask::new(
        app_config.clone(),
        shared_state.clone(),
        state_tx.clone(),
        publisher.clone(),
        cancel.child_token(),
    );
    tokio::spawn(lights_out.run());

    // 6d. Start night mode scheduler
    let night_scheduler = NightModeScheduler::new(
        app_config.clone(),
        shared_state.clone(),
        state_tx.clone(),
        publisher.clone(),
        event_bus.clone(),
        cancel.child_token(),
        Some(circadian_for_automations.clone()),
    );
    tokio::spawn(night_scheduler.run());

    // 7. Start API server
    let api_state = shared_state.clone();
    let api_state_tx = state_tx.clone();
    let api_publisher = publisher.clone();
    let api_bind_addr = app_config.api.bind.clone();
    let api_config = app_config.clone();
    let api_event_bus = event_bus.clone();
    let api_circadian = Some(circadian_for_automations);
    tokio::spawn(async move {
        if let Err(e) = api::start_api_server(
            &api_bind_addr,
            api_state,
            api_state_tx,
            api_publisher,
            api_config,
            api_event_bus,
            api_circadian,
        )
        .await
        {
            tracing::error!("API server error: {}", e);
        }
    });

    // 8. Log ready
    info!("jeha started");

    // Set up SIGHUP for config validation (hot reload not yet supported — restart to apply)
    let reload_config_path = config_path.to_path_buf();
    tokio::spawn(async move {
        let mut sig = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to register SIGHUP handler: {}", e);
                return;
            }
        };
        loop {
            sig.recv().await;
            info!("SIGHUP received, validating config...");
            match config::load_config(&reload_config_path) {
                Ok(new_config) => {
                    info!(
                        "Config valid: {} rooms, {} automations. Restart jeha to apply.",
                        new_config.rooms.len(),
                        new_config.automations.len()
                    );
                }
                Err(e) => {
                    tracing::error!("Config validation failed: {}", e);
                }
            }
        }
    });

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Shutdown signal received");
    cancel.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    info!("jeha stopped");

    Ok(())
}
