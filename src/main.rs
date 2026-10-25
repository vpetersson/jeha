use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use jeha::cli::{Cli, Commands};
use jeha::config;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let default_level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level)),
        )
        .init();

    match cli.command {
        Commands::Run {
            config,
            mqtt_host,
            mqtt_port,
            mqtt_topic,
            mqtt_username,
            mqtt_password,
            api_bind,
            api_enabled,
            api_token,
        } => {
            let api = jeha::cli::run::ApiOverrides {
                enabled: api_enabled,
                bind: api_bind,
                token: api_token,
            };
            jeha::cli::run::run_daemon(
                &config,
                mqtt_host,
                mqtt_port,
                mqtt_topic,
                mqtt_username,
                mqtt_password,
                api,
            )
            .await?;
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
        Commands::Openapi => {
            println!("{}", jeha::api::openapi::openapi_json());
        }
        Commands::Init {
            mqtt_host,
            mqtt_port,
            base_topic,
            mqtt_username,
            mqtt_password,
            output,
        } => {
            jeha::cli::init::run_init(
                &mqtt_host,
                mqtt_port,
                &output,
                &base_topic,
                mqtt_username.as_deref(),
                mqtt_password.as_deref(),
            )
            .await?;
        }
    }

    Ok(())
}
