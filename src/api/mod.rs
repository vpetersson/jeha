pub mod handlers;
pub mod openapi;
pub mod types;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN};
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
        .route(
            "/api/circadian/schedule",
            get(handlers::get_schedule)
                .put(handlers::set_schedule)
                .delete(handlers::reset_schedule),
        )
        .route("/api/night-mode", post(handlers::set_house_night_mode))
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
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn(reject_cross_origin))
        .with_state(app_state)
}

/// A browser sends a body-less POST to another origin without a CORS
/// preflight, so without this any web page could switch lights on a daemon
/// that has no token. Browsers always send `Origin` on POST; curl, Home
/// Assistant and other non-browser clients don't, and the Swagger UI is
/// same-origin, so none of them are affected.
async fn reject_cross_origin(request: Request, next: Next) -> Response {
    if request.method().is_safe() {
        return next.run(request).await;
    }
    let headers = request.headers();
    let Some(origin) = headers.get(ORIGIN) else {
        return next.run(request).await;
    };
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, host)| host);
    let host = headers.get(HOST).and_then(|h| h.to_str().ok());
    match (origin_host, host) {
        (Some(o), Some(h)) if o.eq_ignore_ascii_case(h) => next.run(request).await,
        _ => ApiError::Forbidden("Cross-origin requests are not allowed.".to_string())
            .into_response(),
    }
}

async fn require_token(State(token): State<Arc<String>>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, token)| token.trim_start_matches(' '));
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

async fn method_not_allowed(request: Request) -> ApiError {
    ApiError::MethodNotAllowed(format!(
        "{} is not supported on {}. See /api/docs for the API reference.",
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

[rooms.den]
display_name = "Den"
lights = ["0x001788010AAAAAA2"]

[rooms.den.circadian]
sleep_time = "21:00"
"#;

    /// The returned event loop is never polled, but has to stay alive: it owns
    /// the receiving end of the client's request queue, so publishes succeed.
    fn test_router(api: &ApiConfig) -> (Router, rumqttc::EventLoop) {
        let config: AppConfig = toml::from_str(CONFIG).unwrap();
        let config = Arc::new(config);
        let state = crate::state::new_shared_state();
        {
            let mut initial = (**state.load()).clone();
            for room_id in config.rooms.keys() {
                initial.rooms.entry(room_id.clone()).or_default();
            }
            state.store(Arc::new(initial));
        }
        let event_bus = EventBus::new(16);
        let (state_manager, state_tx) =
            crate::state::StateManager::new(state.clone(), event_bus.clone());
        tokio::spawn(state_manager.run());
        let (client, eventloop) =
            rumqttc::AsyncClient::new(rumqttc::MqttOptions::new("test", "localhost", 1883), 64);
        let publisher = Arc::new(Publisher::new(
            client,
            "zigbee2mqtt".to_string(),
            state.clone(),
            config.clone(),
        ));
        let app_state = Arc::new(AppState::new(
            state, state_tx, publisher, config, event_bus, None,
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
        assert_eq!(body["room_count"], 2);

        // Health and docs stay open.
        let (status, _) = send(&app, Method::GET, "/api/health", None, None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(&app, Method::GET, "/api/openapi.json", None, None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_bearer_scheme_is_case_insensitive() {
        let api = ApiConfig {
            token: Some("s3cret".to_string()),
            ..ApiConfig::default()
        };
        let (app, _eventloop) = test_router(&api);
        for (header, expected) in [
            ("bearer s3cret", StatusCode::OK),
            ("BEARER   s3cret", StatusCode::OK),
            ("Bearer S3CRET", StatusCode::UNAUTHORIZED),
            ("Basic s3cret", StatusCode::UNAUTHORIZED),
        ] {
            let req = Request::builder()
                .uri("/api/system")
                .header(AUTHORIZATION, header)
                .body(Body::empty())
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), expected, "{}", header);
            if expected == StatusCode::UNAUTHORIZED {
                // RFC 9110: a 401 must carry a challenge.
                assert_eq!(
                    res.headers()[axum::http::header::WWW_AUTHENTICATE],
                    "Bearer"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_cross_origin_actions_are_rejected() {
        let (app, _eventloop) = test_router(&ApiConfig::default());
        let post = |origin: &str| {
            Request::builder()
                .method(Method::POST)
                .uri("/api/rooms/kitchen/light/off")
                .header(HOST, "localhost:8420")
                .header(ORIGIN, origin)
                .body(Body::empty())
                .unwrap()
        };

        let res = app
            .clone()
            .oneshot(post("https://evil.example"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = app.clone().oneshot(post("null")).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Same origin (the Swagger UI) and no Origin at all (curl) both work.
        let res = app
            .clone()
            .oneshot(post("http://localhost:8420"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let (status, _) = send(
            &app,
            Method::POST,
            "/api/rooms/kitchen/light/off",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Reads are left alone: the browser won't let the page see the response.
        let req = Request::builder()
            .uri("/api/health")
            .header(ORIGIN, "https://evil.example")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_wrong_method_is_json() {
        let (app, _eventloop) = test_router(&ApiConfig::default());
        let (status, body) = send(&app, Method::POST, "/api/health", None, None).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert!(body["error"].as_str().unwrap().contains("POST"));
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
            for (method, op) in [
                (Method::GET, &item.get),
                (Method::POST, &item.post),
                (Method::PUT, &item.put),
                (Method::DELETE, &item.delete),
            ] {
                if op.is_none() {
                    continue;
                }
                // An empty JSON object gets past routing without publishing anything
                // for most actions; all we care about is that it's not the fallback.
                let body = (method == Method::POST || method == Method::PUT).then(|| json!({}));
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
        assert_eq!(checked, 19);
    }

    /// Lets the state actor apply commands the handlers sent.
    async fn settle() {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn test_house_schedule() {
        let (app, _eventloop) = test_router(&ApiConfig::default());
        let (status, body) = send(&app, Method::GET, "/api/circadian/schedule", None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["wake_time"], "06:00");
        assert_eq!(body["wake_time_source"], "config");

        let (status, body) = send(
            &app,
            Method::PUT,
            "/api/circadian/schedule",
            None,
            Some(json!({"wake_time": "07:15"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", body);
        assert_eq!(body["wake_time"], "07:15");
        assert_eq!(body["wake_time_source"], "api");
        assert_eq!(body["sleep_time"], "23:00");
        assert_eq!(body["sleep_time_source"], "config");
        assert_eq!(body["rooms_with_own_schedule"], json!(["den"]));

        settle().await;
        let (_, body) = send(&app, Method::GET, "/api/circadian/schedule", None, None).await;
        assert_eq!(body["wake_time"], "07:15");

        let (status, _) = send(
            &app,
            Method::PUT,
            "/api/circadian/schedule",
            None,
            Some(json!({"sleep_time": "25:00"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) =
            send(&app, Method::DELETE, "/api/circadian/schedule", None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["wake_time"], "06:00");
        settle().await;
        let (_, body) = send(&app, Method::GET, "/api/circadian/schedule", None, None).await;
        assert_eq!(body["wake_time_source"], "config");
    }

    #[tokio::test]
    async fn test_house_night_mode() {
        let (app, _eventloop) = test_router(&ApiConfig::default());
        let (status, body) = send(
            &app,
            Method::POST,
            "/api/night-mode",
            None,
            Some(json!({"active": true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", body);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["rooms"], json!(["den", "kitchen"]));

        settle().await;
        let (_, rooms) = send(&app, Method::GET, "/api/rooms", None, None).await;
        for room in rooms.as_array().unwrap() {
            assert_eq!(room["night_mode"], true, "{}", room["room_id"]);
        }

        let (status, body) = send(
            &app,
            Method::POST,
            "/api/night-mode",
            None,
            Some(json!({"active": false})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", body);
        settle().await;
        let (_, rooms) = send(&app, Method::GET, "/api/rooms", None, None).await;
        for room in rooms.as_array().unwrap() {
            assert_eq!(room["night_mode"], false, "{}", room["room_id"]);
        }
    }
}
