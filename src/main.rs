use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use jeha::cli::{Cli, Commands};
use jeha::config;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            config,
            mqtt_host,
            mqtt_port,
            mqtt_topic,
            mcp_bind,
        } => {
            jeha::cli::run::run_daemon(&config, mqtt_host, mqtt_port, mqtt_topic, mcp_bind).await?;
        }
        Commands::Validate {
            config: config_path,
            check_devices,
        } => {
            jeha::cli::validate::run_validate(&config_path, check_devices)?;
        }
        Commands::Migrate {
            config: config_path,
            dry_run,
        } => {
            config::migrate::migrate_config(&config_path, dry_run)?;
        }
        Commands::Schema => {
            let schema = schemars::schema_for!(config::types::AppConfig);
            println!("{}", serde_json::to_string_pretty(&schema)?);
        }
        Commands::Init { mqtt, output } => {
            println!(
                "jeha init: would connect to MQTT at {} and generate config",
                mqtt
            );
            println!("This feature requires a running MQTT broker with Z2M.");
            if let Some(path) = output {
                println!("Output would be written to: {}", path.display());
            }
        }
    }

    Ok(())
}
