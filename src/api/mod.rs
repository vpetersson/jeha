pub mod handlers;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::routing::{get, post};
use tokio::sync::mpsc;
use tracing::info;

use crate::circadian::CircadianEngine;
use crate::config::types::AppConfig;
use crate::event::EventBus;
use crate::mqtt::publish::Publisher;
use crate::state::{SharedState, StateCommand};

use handlers::AppState;

pub async fn start_api_server(
    bind_addr: &str,
    state: SharedState,
    state_tx: mpsc::Sender<StateCommand>,
    publisher: Arc<Publisher>,
    config: Arc<AppConfig>,
    event_bus: EventBus,
    circadian_engine: Option<Arc<CircadianEngine>>,
) -> Result<()> {
    info!("Starting API server on {}", bind_addr);

    let app_state = Arc::new(AppState::new(
        state,
        state_tx,
        publisher,
        config,
        event_bus,
        circadian_engine,
    ));

    let app = Router::new()
        .route("/api/rooms", get(handlers::get_rooms))
        .route("/api/rooms/{room_id}", get(handlers::get_room))
        .route("/api/rooms/{room_id}/light/on", post(handlers::light_on))
        .route("/api/rooms/{room_id}/light/off", post(handlers::light_off))
        .route(
            "/api/rooms/{room_id}/circadian/pause",
            post(handlers::pause_circadian),
        )
        .route(
            "/api/rooms/{room_id}/circadian/resume",
            post(handlers::resume_circadian),
        )
        .route(
            "/api/rooms/{room_id}/circadian/snooze",
            post(handlers::snooze_circadian),
        )
        .route("/api/rooms/{room_id}/scene", post(handlers::set_scene))
        .route(
            "/api/rooms/{room_id}/z2m-scenes",
            get(handlers::list_z2m_scenes),
        )
        .route(
            "/api/rooms/{room_id}/z2m-scenes/recall",
            post(handlers::recall_z2m_scene),
        )
        .route(
            "/api/rooms/{room_id}/night-mode",
            post(handlers::set_night_mode),
        )
        .route("/api/circadian", get(handlers::get_circadian_status))
        .route("/api/system", get(handlers::get_system_status))
        .with_state(app_state);

    let addr: std::net::SocketAddr = bind_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!("API server listening on {}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}
