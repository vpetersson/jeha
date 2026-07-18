use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::types::AppConfig;
use crate::event::{Event, EventBus};
use crate::state::SharedState;

pub struct ConfigSync {
    config: Arc<AppConfig>,
    config_path: PathBuf,
    state: SharedState,
    event_bus: EventBus,
    cancel: CancellationToken,
    /// Groups already appended to the config file this session. The in-memory
    /// `config` is immutable, so without this every retained `bridge/groups`
    /// redelivery (e.g. on MQTT reconnect) would re-append the same rooms —
    /// and duplicate TOML tables make the config unparseable on restart.
    appended_groups: HashSet<String>,
    appended_room_ids: HashSet<String>,
}

impl ConfigSync {
    pub fn new(
        config: Arc<AppConfig>,
        config_path: &Path,
        state: SharedState,
        event_bus: EventBus,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            config_path: config_path.to_path_buf(),
            state,
            event_bus,
            cancel,
            appended_groups: HashSet::new(),
            appended_room_ids: HashSet::new(),
        }
    }

    pub async fn run(mut self) {
        info!("Config sync started");
        let mut event_rx = self.event_bus.subscribe();

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("Config sync shutting down");
                    return;
                }
                result = event_rx.recv() => {
                    match result {
                        Ok(Event::GroupsUpdated) => {
                            if let Err(e) = self.sync_new_groups().await {
                                warn!("Config sync failed: {}", e);
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("Config sync lagged, {} events dropped", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            info!("Event bus closed, config sync shutting down");
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn sync_new_groups(&mut self) -> Result<()> {
        let current_state = self.state.load();

        // Collect all Z2M group names already referenced in config,
        // plus any we already appended to the file this session.
        let known_groups: HashSet<&str> = self
            .config
            .rooms
            .values()
            .filter_map(|r| r.z2m_group.as_deref())
            .chain(self.appended_groups.iter().map(|s| s.as_str()))
            .collect();

        // Find groups that have light members but aren't in config
        let mut new_entries = Vec::new();

        for (group_name, group_info) in current_state.group_map.iter() {
            if known_groups.contains(group_name.as_str()) {
                continue;
            }

            // Skip empty groups
            if group_info.members.is_empty() {
                continue;
            }

            // Check if any member is a light (supports brightness)
            let has_lights = group_info.members.iter().any(|m| {
                current_state
                    .device_map
                    .get(&m.ieee_address)
                    .is_some_and(|d| d.supports_brightness)
            });

            if !has_lights {
                continue;
            }

            // Generate a room_id from the group name
            let room_id = group_name
                .to_lowercase()
                .replace([' ', '-'], "_")
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>();

            // Skip if a room with this ID already exists (in config or
            // appended earlier this session)
            if self.config.rooms.contains_key(&room_id) || self.appended_room_ids.contains(&room_id)
            {
                continue;
            }

            new_entries.push((room_id, group_name.clone()));
        }

        if new_entries.is_empty() {
            return Ok(());
        }

        // Append new rooms to config file
        let mut additions = String::new();
        for (room_id, group_name) in &new_entries {
            additions.push_str(&format!(
                "\n[rooms.{}]\ndisplay_name = \"{}\"\nz2m_group = \"{}\"\n",
                room_id, group_name, group_name
            ));
            info!(
                "Auto-discovered new room: '{}' (group '{}')",
                room_id, group_name
            );
        }

        // Read existing config, append, write back atomically (temp file + rename)
        let existing = tokio::fs::read_to_string(&self.config_path).await?;
        let updated = format!("{}{}", existing.trim_end(), additions);
        let tmp_path = self.config_path.with_extension("toml.tmp");
        tokio::fs::write(&tmp_path, &updated).await?;
        tokio::fs::rename(&tmp_path, &self.config_path).await?;

        for (room_id, group_name) in &new_entries {
            self.appended_room_ids.insert(room_id.clone());
            self.appended_groups.insert(group_name.clone());
        }

        info!(
            "Config updated with {} new room(s). Restart or SIGHUP to apply.",
            new_entries.len()
        );

        Ok(())
    }
}
