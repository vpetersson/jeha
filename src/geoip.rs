//! Automatic location for winter mode.
//!
//! When `general.latitude` is unset, jeha asks a GeoIP service where it is once at
//! startup and writes the result back into the config file, so the lookup happens
//! once rather than on every restart and the value is visible and editable.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const GEOIP_URL: &str = "https://geoip.vpetersson.com/";
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Deserialize)]
pub struct GeoIpLocation {
    pub latitude: f64,
    pub city: Option<String>,
    pub country_name: Option<String>,
}

impl GeoIpLocation {
    /// "City, Country" for log lines, or whatever parts are known.
    pub fn place(&self) -> String {
        [self.city.as_deref(), self.country_name.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Look up this host's approximate location from its public IP.
pub async fn lookup() -> Result<GeoIpLocation> {
    let body = tokio::task::spawn_blocking(|| -> Result<String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(LOOKUP_TIMEOUT))
            .build()
            .into();
        Ok(agent.get(GEOIP_URL).call()?.body_mut().read_to_string()?)
    })
    .await??;
    parse_response(&body)
}

fn parse_response(body: &str) -> Result<GeoIpLocation> {
    let location: GeoIpLocation =
        serde_json::from_str(body).context("unexpected GeoIP response")?;
    if !(-90.0..=90.0).contains(&location.latitude) {
        bail!("GeoIP returned invalid latitude {}", location.latitude);
    }
    Ok(location)
}

/// City-level precision (~1 km) is all day length needs; don't store more.
pub fn round_latitude(latitude: f64) -> f64 {
    (latitude * 100.0).round() / 100.0
}

/// Write `latitude` into the `[general]` table of the config file.
pub async fn persist_latitude(config_path: &Path, latitude: f64) -> Result<()> {
    let content = tokio::fs::read_to_string(config_path).await?;
    let Some(updated) = insert_latitude(&content, latitude) else {
        bail!("couldn't add `latitude` to this config layout safely; add it by hand");
    };
    crate::config::write_config(config_path, &updated).await
}

/// Insert `latitude = …` into the config text, keeping the user's formatting and
/// comments. The edit is line-based, so it is verified by parsing: the result must
/// be the original document plus `general.latitude` and nothing else. Anything
/// the line edit can't handle (e.g. `[ general ]`, dotted `general.*` keys, a
/// `[general]` line inside a multi-line string) returns `None` and the file is
/// left untouched.
fn insert_latitude(content: &str, latitude: f64) -> Option<String> {
    let original: toml::Table = content.parse().ok()?;
    let updated = insert_latitude_line(content, latitude)?;
    let mut parsed: toml::Table = updated.parse().ok()?;
    let general = parsed.get_mut("general")?.as_table_mut()?;
    if general.remove("latitude")?.as_float()? != latitude {
        return None;
    }
    if general.is_empty() && !original.contains_key("general") {
        parsed.remove("general");
    }
    (parsed == original).then_some(updated)
}

/// Best-effort line edit: add `latitude` as the first key of a `[general]` header,
/// or append a new `[general]` table when the file has none.
fn insert_latitude_line(content: &str, latitude: f64) -> Option<String> {
    // `{:?}` keeps a trailing `.0` on whole degrees (`52.0`, not `52`), so TOML
    // reads it back as a float rather than an integer.
    let line = format!(
        "latitude = {:?}  # Set automatically from GeoIP — edit if wrong",
        latitude
    );
    let mut out = String::with_capacity(content.len() + line.len() + 16);
    let mut inserted = false;
    for l in content.lines() {
        out.push_str(l);
        out.push('\n');
        if !inserted && l.split('#').next().unwrap_or("").trim() == "[general]" {
            out.push_str(&line);
            out.push('\n');
            inserted = true;
        }
    }
    if inserted {
        return Some(out);
    }
    let sets_general_inline = content.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("general.") || t.starts_with("general =") || t.starts_with("general=")
    });
    if sets_general_inline {
        return None;
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str("[general]\n");
    out.push_str(&line);
    out.push('\n');
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::AppConfig;

    #[test]
    fn test_parse_response() {
        let body = r#"{"latitude":51.5177,"longitude":-0.6215,"city":"Slough","country_name":"United Kingdom","time_zone":{"name":"Europe/London"},"languages":"en-GB,en"}"#;
        let loc = parse_response(body).unwrap();
        assert_eq!(loc.latitude, 51.5177);
        assert_eq!(loc.place(), "Slough, United Kingdom");
    }

    #[test]
    fn test_parse_response_rejects_garbage() {
        assert!(parse_response("<html>").is_err());
        assert!(parse_response(r#"{"latitude": 123.0}"#).is_err());
    }

    #[test]
    fn test_round_latitude() {
        assert_eq!(round_latitude(51.5177), 51.52);
        assert_eq!(round_latitude(-33.8688), -33.87);
    }

    #[test]
    fn test_insert_into_existing_general() {
        let content = "schema_version = 1\n\n[general]\ntimezone = \"Europe/Stockholm\"\n\n[rooms.kitchen]\nz2m_group = \"Kitchen\"\n";
        let updated = insert_latitude(content, 59.33).unwrap();
        let config: AppConfig = toml::from_str(&updated).unwrap();
        assert_eq!(config.general.latitude, Some(59.33));
        assert_eq!(config.general.timezone, "Europe/Stockholm");
        assert!(config.rooms.contains_key("kitchen"));
    }

    #[test]
    fn test_insert_after_commented_header() {
        let content = "schema_version = 1\n[general] # global settings\ntimezone = \"UTC\"\n";
        let updated = insert_latitude(content, 59.33).unwrap();
        let config: AppConfig = toml::from_str(&updated).unwrap();
        assert_eq!(config.general.latitude, Some(59.33));
    }

    #[test]
    fn test_insert_whole_degree_latitude() {
        let content = "schema_version = 1\n\n[general]\ntimezone = \"UTC\"\n";
        let updated = insert_latitude(content, 52.0).unwrap();
        assert!(updated.contains("latitude = 52.0 "), "{updated}");
        let config: AppConfig = toml::from_str(&updated).unwrap();
        assert_eq!(config.general.latitude, Some(52.0));
        assert_eq!(insert_latitude(content, -0.0).map(|_| ()), Some(()));
    }

    #[test]
    fn test_insert_creates_general() {
        let content = "schema_version = 1\n\n[rooms.kitchen]\nz2m_group = \"Kitchen\"\n";
        let updated = insert_latitude(content, -33.87).unwrap();
        let config: AppConfig = toml::from_str(&updated).unwrap();
        assert_eq!(config.general.latitude, Some(-33.87));
        assert!(config.rooms.contains_key("kitchen"));
    }

    #[test]
    fn test_insert_refuses_dotted_general_keys() {
        let content = "schema_version = 1\ngeneral.timezone = \"UTC\"\n";
        assert!(insert_latitude(content, 59.33).is_none());
    }

    #[test]
    fn test_insert_refuses_layouts_it_cannot_edit() {
        // Valid TOML that the line edit doesn't recognise as the [general] header.
        // Appending a second [general] table would make the file unparseable.
        for content in [
            "schema_version = 1\n[ general ]\ntimezone = \"UTC\"\n",
            "schema_version = 1\n[\"general\"]\ntimezone = \"UTC\"\n",
            "schema_version = 1\ngeneral . timezone = \"UTC\"\n",
            "schema_version = 1\ngeneral = { timezone = \"UTC\" }\n",
        ] {
            assert!(insert_latitude(content, 59.33).is_none(), "{content}");
        }
    }

    #[test]
    fn test_insert_ignores_header_inside_multiline_string() {
        let content = "schema_version = 1\nnote = \"\"\"\n[general]\n\"\"\"\n";
        assert!(insert_latitude(content, 59.33).is_none());
    }

    #[test]
    fn test_insert_refuses_unparseable_config() {
        assert!(insert_latitude("schema_version = [", 59.33).is_none());
    }
}
