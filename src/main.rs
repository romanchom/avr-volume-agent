use std::process::{Command, Stdio};

use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use avr_volume_agent::pipewire_backend::{self, PipeWireConfig};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// PipeWire sink that receives the agent's output
    #[arg(long)]
    target_sink: Option<String>,

    /// Name assigned to the virtual PipeWire sink
    #[arg(long, default_value = "avr_volume_agent")]
    node_name: String,

    /// Denon AVR host name or IP address
    #[arg(long, default_value = "10.1.92.186")]
    avr_host: String,

    /// Denon AVR TCP control port
    #[arg(long, default_value_t = 23, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,

    /// Run as an audio passthrough without connecting to the AVR
    #[arg(long)]
    no_network: bool,

    /// Keep AVR status synchronization, but do not send volume or mute commands
    #[arg(long)]
    read_only: bool,
}

fn main() {
    init_logging();
    if let Err(error) = run() {
        error!(%error, "avr-volume-agent stopped with an error");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let target_sink = match args.target_sink {
        Some(target) => target,
        None => default_target_sink()?,
    };
    if target_sink == args.node_name {
        return Err("target sink cannot be the virtual sink itself".into());
    }

    info!(
        node = %args.node_name,
        target = %target_sink,
        "starting AVR volume agent"
    );
    pipewire_backend::run(PipeWireConfig {
        node_name: args.node_name,
        target_sink,
        avr_host: args.avr_host,
        avr_port: args.port,
        network_enabled: !args.no_network,
        write_enabled: !args.read_only,
    })
}

fn default_target_sink() -> Result<String, Box<dyn std::error::Error>> {
    // Resolve the default once; graph discovery/linking itself uses PipeWire's registry.
    let output = Command::new("pactl")
        .args(["get-default-sink"])
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "could not determine default PipeWire sink (pactl exited with {}); use --target-sink",
            output.status
        )
        .into());
    }

    let target = String::from_utf8(output.stdout)?;
    let target = target.trim();
    if target.is_empty() {
        return Err("pactl returned an empty default sink name; use --target-sink".into());
    }
    Ok(target.to_owned())
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
