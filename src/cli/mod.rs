pub mod init;
pub mod run;
pub mod validate;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "jeha", about = "Light Automation OS", version)]
pub struct Cli {
    /// Enable debug logging
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the daemon
    Run {
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
        /// MQTT broker host (overrides config file)
        #[arg(long, env = "JEHA_MQTT_HOST")]
        mqtt_host: Option<String>,
        /// MQTT broker port (overrides config file)
        #[arg(long, env = "JEHA_MQTT_PORT")]
        mqtt_port: Option<u16>,
        /// MQTT base topic (overrides config file)
        #[arg(long, env = "JEHA_MQTT_TOPIC")]
        mqtt_topic: Option<String>,
        /// MCP server bind address (overrides config file)
        #[arg(long, env = "JEHA_MCP_BIND")]
        mcp_bind: Option<String>,
    },
    /// Validate configuration
    Validate {
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
        /// Also verify against live Z2M
        #[arg(long)]
        check_devices: bool,
    },
    /// Migrate config to latest schema
    Migrate {
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Export JSON Schema for editor integration
    Schema,
    /// Connect to Z2M, discover groups/devices, generate config.toml
    Init {
        /// MQTT broker host
        #[arg(long, default_value = "localhost", env = "JEHA_MQTT_HOST")]
        mqtt_host: String,
        /// MQTT broker port
        #[arg(long, default_value_t = 1883, env = "JEHA_MQTT_PORT")]
        mqtt_port: u16,
        /// Z2M base topic
        #[arg(long, default_value = "zigbee2mqtt", env = "JEHA_MQTT_TOPIC")]
        base_topic: String,
        /// Output file
        #[arg(short, long, default_value = "config.toml")]
        output: PathBuf,
    },
}
