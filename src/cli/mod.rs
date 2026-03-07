pub mod run;
pub mod validate;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "jeha", about = "Light Automation OS", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the daemon
    Run {
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
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
        /// MQTT broker address
        #[arg(long, default_value = "localhost:1883")]
        mqtt: String,
        /// Output file (default: stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}
