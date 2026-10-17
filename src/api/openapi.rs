//! OpenAPI document for the REST API, generated from the handler annotations.
//!
//! Served at `/api/openapi.json`, printed by `jeha openapi`, and checked in at
//! `docs/api/openapi.json` (a test fails if the checked-in copy is stale).

use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme};
use utoipa::openapi::{RefOr, Required};
use utoipa::{Modify, OpenApi};

use super::handlers;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "jeha",
        description = "REST API for jeha, a light automation daemon for Zigbee2MQTT.\n\n\
            Rooms are identified by their key under `[rooms]` in config.toml. Every \
            response that changes something carries a human-readable `description`, \
            so the API works as well for scripts as it does as a tool for an AI assistant.\n\n\
            Errors use HTTP status codes with a `{\"error\": \"...\"}` body.\n\n\
            When `api.token` is set, every endpoint except `/api/health`, \
            `/api/openapi.json` and `/api/docs` requires `Authorization: Bearer <token>`.",
        license(name = "GPL-3.0", identifier = "GPL-3.0-only"),
    ),
    paths(
        handlers::get_rooms,
        handlers::get_room,
        handlers::light_on,
        handlers::light_off,
        handlers::pause_circadian,
        handlers::resume_circadian,
        handlers::snooze_circadian,
        handlers::set_scene,
        handlers::list_z2m_scenes,
        handlers::recall_z2m_scene,
        handlers::set_night_mode,
        handlers::list_scenes,
        handlers::get_circadian_status,
        handlers::get_schedule,
        handlers::set_schedule,
        handlers::reset_schedule,
        handlers::set_house_night_mode,
        handlers::list_alarms,
        handlers::set_alarm,
        handlers::reset_alarm,
        handlers::get_system_status,
        handlers::health,
    ),
    tags(
        (name = "rooms", description = "Room state"),
        (name = "lights", description = "Turn lights on and off"),
        (name = "circadian", description = "Pause, snooze and resume circadian lighting"),
        (name = "scenes", description = "Built-in scenes and Zigbee2MQTT group scenes"),
        (name = "night_mode", description = "Night mode"),
        (name = "alarms", description = "Wake-up alarms that fade the lights in"),
        (name = "system", description = "Daemon status"),
    ),
    modifiers(&BearerAuth, &OptionalBodies),
)]
pub struct ApiDoc;

/// Declares the optional bearer token. The empty requirement next to it marks
/// authentication as optional, since it only applies when `api.token` is set.
struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some("The value of `api.token` in config.toml"))
                    .build(),
            ),
        );
        openapi.security = Some(vec![
            SecurityRequirement::new("bearer", Vec::<String>::new()),
            SecurityRequirement::default(),
        ]);
    }
}

/// The light on/off bodies are optional, but utoipa marks a body as required
/// unless its type is an `Option`, which would also add a `null` variant to
/// the schema. Clear the flag here instead.
struct OptionalBodies;

impl Modify for OptionalBodies {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        for path in [
            "/api/rooms/{room_id}/light/on",
            "/api/rooms/{room_id}/light/off",
        ] {
            if let Some(body) = openapi
                .paths
                .paths
                .get_mut(path)
                .and_then(|item| item.post.as_mut())
                .and_then(|op| op.request_body.as_mut())
                && let RefOr::T(body) = body
            {
                body.required = Some(Required::False);
            }
        }
    }
}

/// The OpenAPI document as pretty-printed JSON.
pub fn openapi_json() -> String {
    ApiDoc::openapi()
        .to_pretty_json()
        .expect("OpenAPI document serializes")
}

/// Swagger UI page. Assets come from the jsDelivr CDN so the binary stays
/// small; the spec itself is always served by jeha.
pub const SWAGGER_UI_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>jeha API</title>
  <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui.css">
</head>
<body>
  <div id="swagger-ui"></div>
  <script src="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui-bundle.js" crossorigin></script>
  <script>
    window.onload = () => {
      window.ui = SwaggerUIBundle({
        url: "openapi.json",
        dom_id: "#swagger-ui",
        deepLinking: true,
        persistAuthorization: true,
      });
    };
  </script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a spec and blanks `info.version`, so a release bump alone doesn't
    /// make the checked-in copy stale.
    fn normalized(spec: &str) -> serde_json::Value {
        let mut value: serde_json::Value = serde_json::from_str(spec).unwrap();
        value["info"]["version"] = serde_json::Value::Null;
        value
    }

    #[test]
    fn test_checked_in_spec_is_current() {
        let checked_in = include_str!("../../docs/api/openapi.json");
        assert!(
            normalized(checked_in) == normalized(&openapi_json()),
            "docs/api/openapi.json is out of date. Regenerate it with:\n  \
             cargo run -- openapi > docs/api/openapi.json"
        );
    }
}
