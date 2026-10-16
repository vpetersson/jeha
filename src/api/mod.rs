pub mod handlers;
pub mod openapi;
pub mod types;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use tokio::sync::mpsc;
use tracing::info;

use crate::circadian::CircadianEngine;
use crate::config::types::{ApiConfig, AppConfig};
use crate::event::EventBus;
use crate::mqtt::publish::Publisher;
use crate::state::{SharedState, StateCommand};

use handlers::{ApiError, AppState};

pub async fn start_api_server(
    api: &ApiConfig,
    state: SharedState,
    state_tx: mpsc::Sender<StateCommand>,
    publisher: Arc<Publisher>,
    config: Arc<AppConfig>,
    event_bus: EventBus,
    circadian_engine: Option<Arc<CircadianEngine>>,
) -> Result<()> {
    info!("Starting API server on {}", api.bind);

    let app_state = Arc::new(AppState::new(
        state,
        state_tx,
        publisher,
        config,
        event_bus,
        circadian_engine,
    ));
    let app = router(app_state, api);

    let addr: std::net::SocketAddr = api.bind.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!(
        "API server listening on {} (auth: {}, docs: {})",
        addr,
        if api.token.is_some() { "token" } else { "none" },
        if api.docs { "/api/docs" } else { "off" }
    );

    axum::serve(listener, app).await?;

    Ok(())
}

/// Builds the API router. `api.token` and `api.docs` decide which routes need
/// a token and whether the spec and Swagger UI are served.
pub fn router(app_state: Arc<AppState>, api: &ApiConfig) -> Router {
    let mut protected = Router::new()
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
        .route("/api/scenes", get(handlers::list_scenes))
        .route("/api/circadian", get(handlers::get_circadian_status))
        .route("/api/system", get(handlers::get_system_status));

    if let Some(token) = api.token.clone() {
        protected = protected.route_layer(middleware::from_fn_with_state(
            Arc::new(token),
            require_token,
        ));
    }

    let mut public = Router::new().route("/api/health", get(handlers::health));
    if api.docs {
        let spec = Arc::new(openapi::openapi_json());
        public = public
            .route(
                "/api/openapi.json",
                get(move || {
                    let spec = spec.clone();
                    async move { ([(CONTENT_TYPE, "application/json")], spec.to_string()) }
                }),
            )
            .route(
                "/api/docs",
                get(|| async { Html(openapi::SWAGGER_UI_HTML) }),
            );
    }

    protected
        .merge(public)
        .fallback(not_found)
        .with_state(app_state)
}

async fn require_token(State(token): State<Arc<String>>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(presented) if constant_time_eq(presented.as_bytes(), token.as_bytes()) => {
            next.run(request).await
        }
        Some(_) => ApiError::Unauthorized("Invalid API token".to_string()).into_response(),
        None => ApiError::Unauthorized(
            "Missing API token. Send it as 'Authorization: Bearer <token>'.".to_string(),
        )
        .into_response(),
    }
}

/// Compares without short-circuiting on the first differing byte, so response
/// timing doesn't reveal how much of a guessed token was right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn not_found(request: Request) -> ApiError {
    ApiError::NotFound(format!(
        "No such endpoint: {} {}. See /api/docs for the API reference.",
        request.method(),
        request.uri().path()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{Method, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    const CONFIG: &str = r#"
schema_version = 1

[rooms.kitchen]
display_name = "Kitchen"
lights = ["0x001788010AAAAAA1"]
"#;

    /// The returned event loop is never polled, but has to stay alive: it owns
    /// the receiving end of the client's request queue, so publishes succeed.
    fn test_router(api: &ApiConfig) -> (Router, rumqttc::EventLoop) {
        let config: AppConfig = toml::from_str(CONFIG).unwrap();
        let config = Arc::new(config);
        let state = crate::state::new_shared_state();
        let (state_tx, _state_rx) = mpsc::channel(64);
        let (client, eventloop) =
            rumqttc::AsyncClient::new(rumqttc::MqttOptions::new("test", "localhost", 1883), 64);
        let publisher = Arc::new(Publisher::new(
            client,
            "zigbee2mqtt".to_string(),
            state.clone(),
            config.clone(),
        ));
        let app_state = Arc::new(AppState::new(
            state,
            state_tx,
            publisher,
            config,
            EventBus::new(16),
            None,
        ));
        (router(app_state, api), eventloop)
    }

    async fn send(
        app: &Router,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            req = req.header(AUTHORIZATION, format!("Bearer {}", token));
        }
        let req = match body {
            Some(body) => req
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn test_health_and_scenes() {
        let (app, _eventloop) = test_router(&ApiConfig::default());
        let (status, body) = send(&app, Method::GET, "/api/health", None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");

        let (status, body) = send(&app, Method::GET, "/api/scenes", None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), handlers::SCENES.len());
    }

    #[tokio::test]
    async fn test_errors_are_json() {
        let (app, _eventloop) = test_router(&ApiConfig::default());

        let (status, body) = send(&app, Method::GET, "/api/nope", None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("/api/nope"));

        let (status, body) = send(
            &app,
            Method::POST,
            "/api/rooms/attic/circadian/pause",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("kitchen"));

        // Malformed bodies come back as JSON errors, not axum's plain text.
        let (status, body) = send(
            &app,
            Method::POST,
            "/api/rooms/kitchen/circadian/snooze",
            None,
            Some(json!({"hours": "two"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string());

        let (status, _) = send(
            &app,
            Method::POST,
            "/api/rooms/kitchen/circadian/snooze",
            None,
            Some(json!({"hours": 0})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) = send(
            &app,
            Method::POST,
            "/api/rooms/kitchen/scene",
            None,
            Some(json!({"scene": "disco"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("relax"));

        // Out-of-range values are rejected before anything is published (a
        // zero colour temperature used to divide by zero).
        for body in [
            json!({"color_temp_k": 0}),
            json!({"brightness": 255}),
            json!({"brightness": 100, "override_ttl_mins": -5}),
        ] {
            let (status, _) = send(
                &app,
                Method::POST,
                "/api/rooms/kitchen/light/on",
                None,
                Some(body.clone()),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{}", body);
        }
    }

    #[tokio::test]
    async fn test_actions_without_body() {
        let (app, _eventloop) = test_router(&ApiConfig::default());
        // light/off takes an optional body: no Content-Type at all is fine.
        let (status, body) = send(
            &app,
            Method::POST,
            "/api/rooms/kitchen/light/off",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", body);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["room"], "kitchen");

        let (status, body) = send(
            &app,
            Method::POST,
            "/api/rooms/kitchen/circadian/snooze",
            None,
            Some(json!({"hours": 1.5})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["circadian"], "snoozed");
        assert_eq!(body["duration"], "1h 30m");
        // Fields that belong to other actions are left out, not null.
        assert!(body.get("scene").is_none());
    }

    #[tokio::test]
    async fn test_token_auth() {
        let api = ApiConfig {
            token: Some("s3cret".to_string()),
            ..ApiConfig::default()
        };
        let (app, _eventloop) = test_router(&api);

        let (status, _) = send(&app, Method::GET, "/api/system", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = send(&app, Method::GET, "/api/system", Some("wrong"), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, body) = send(&app, Method::GET, "/api/system", Some("s3cret"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["room_count"], 0);

        // Health and docs stay open.
        let (status, _) = send(&app, Method::GET, "/api/health", None, None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(&app, Method::GET, "/api/openapi.json", None, None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_docs_can_be_disabled() {
        let (app, _eventloop) = test_router(&ApiConfig::default());
        let (status, spec) = send(&app, Method::GET, "/api/openapi.json", None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(spec["info"]["title"], "jeha");

        let api = ApiConfig {
            docs: false,
            ..ApiConfig::default()
        };
        let (app, _eventloop) = test_router(&api);
        let (status, _) = send(&app, Method::GET, "/api/openapi.json", None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(&app, Method::GET, "/api/docs", None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Every operation in the spec must be routed, so the docs can't describe
    /// an endpoint that doesn't exist (or the wrong method for one).
    #[tokio::test]
    async fn test_spec_matches_router() {
        use utoipa::OpenApi;

        let (app, _eventloop) = test_router(&ApiConfig::default());
        let spec = openapi::ApiDoc::openapi();
        let mut checked = 0;
        for (path, item) in &spec.paths.paths {
            let uri = path.replace("{room_id}", "kitchen");
            for (method, op) in [(Method::GET, &item.get), (Method::POST, &item.post)] {
                if op.is_none() {
                    continue;
                }
                // An empty JSON object gets past routing without publishing anything
                // for most actions; all we care about is that it's not the fallback.
                let body = (method == Method::POST).then(|| json!({}));
                let (status, body) = send(&app, method.clone(), &uri, None, body).await;
                let fallback = body["error"]
                    .as_str()
                    .is_some_and(|e| e.starts_with("No such endpoint"));
                assert!(
                    !fallback && status != StatusCode::METHOD_NOT_ALLOWED,
                    "{} {} is documented but not routed",
                    method,
                    path
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 15);
    }
}
