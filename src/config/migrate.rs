use std::path::Path;

use anyhow::{Result, bail};

pub fn migrate_config(path: &Path, dry_run: bool) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let table: toml::Table = content.parse()?;

    let version = table
        .get("schema_version")
        .and_then(|v| v.as_integer())
        .unwrap_or(0);

    if version == 1 {
        tracing::info!("Config is already at schema_version 1. No migration needed.");
        return Ok(());
    }

    if version == 0 {
        bail!(
            "schema_version 0 or missing — cannot auto-migrate. Please add schema_version = 1 to your config."
        );
    }

    if version > 1 {
        bail!(
            "schema_version {} is newer than this version of jeha supports.",
            version
        );
    }

    if dry_run {
        tracing::info!("Dry run: would migrate from version {} to 1.", version);
    } else {
        let backup = path.with_extension("toml.bak");
        std::fs::copy(path, &backup)?;
        tracing::info!("Backup created at {}", backup.display());
        // Future migrations go here
    }

    Ok(())
}
