//! `rtflow-server` CLI.
use clap::Parser;
use rtflow_server::{Config, server::ServerApp};
use std::{path::PathBuf, process::ExitCode};

#[derive(Debug, Parser)]
#[command(
    name = "rtflow-server",
    version,
    about = "RTSP/RTMP/SRT/WebRTC/HLS/DASH streaming server with recording, VOD and path control plane"
)]
struct Args {
    /// Configuration file.
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Validate the configuration and exit.
    #[arg(long)]
    check_config: bool,
    /// Print the effective configuration and exit.
    #[arg(long)]
    print_config: bool,
    /// Write a complete example configuration to ./rtflow.toml.
    #[arg(long)]
    init_config: bool,
    /// Override the configured log level.
    #[arg(long)]
    log_level: Option<String>,
    /// Print SQLite schema version/size and exit.
    #[arg(long)]
    db_info: bool,
    /// Write a consistent SQLite backup and exit.
    #[arg(long, value_name = "FILE")]
    db_backup: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from("rtflow.toml"));

    if args.init_config {
        let path = args
            .config
            .clone()
            .unwrap_or_else(|| PathBuf::from("rtflow.toml"));
        if path.exists() {
            eprintln!("{} already exists; refusing to overwrite", path.display());
            return ExitCode::FAILURE;
        }
        if let Err(error) = std::fs::write(&path, DEFAULT_CONFIG) {
            eprintln!("cannot write {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
        println!("wrote {}", path.display());
        return ExitCode::SUCCESS;
    }

    let config = match load_config(&config_path, args.config.is_some()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("configuration error: {error}");
            return ExitCode::FAILURE;
        }
    };
    if args.check_config {
        println!("{}: ok", config_path.display());
        return ExitCode::SUCCESS;
    }
    if args.db_info {
        match rtflow_server::storage_info(&config) {
            Ok((version, bytes)) => {
                println!(
                    "database: {} (schema {version}, {bytes} bytes)",
                    config
                        .storage
                        .database_path(&config.server.state_dir)
                        .display()
                );
            }
            Err(error) => {
                eprintln!("database error: {error}");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }
    if let Some(path) = &args.db_backup {
        match rtflow_server::storage_backup(&config, path) {
            Ok(()) => println!("backup written to {}", path.display()),
            Err(error) => {
                eprintln!("backup failed: {error}");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }
    if args.print_config {
        match config.redacted().to_toml() {
            Ok(text) => print!("{text}"),
            Err(error) => {
                eprintln!("configuration error: {error}");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }

    let filter = args
        .log_level
        .clone()
        .or_else(|| std::env::var("RUST_LOG").ok())
        .or_else(|| Some(config.log.level.clone()));
    init_logging(filter, config.log.format);

    let config_path = config_path.exists().then(|| config_path.clone());
    let handle = match ServerApp::start(config).await {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("startup failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    handle.wait_for_signal(config_path).await;
    ExitCode::SUCCESS
}

fn init_logging(filter: Option<String>, format: rtflow_server::config::LogFormat) {
    use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
    let filter = filter.unwrap_or_else(|| "info".into());
    let filter = EnvFilter::try_new(&filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);
    match format {
        rtflow_server::config::LogFormat::Json => {
            let _ = registry.with(fmt::layer().json()).try_init();
        }
        rtflow_server::config::LogFormat::Text => {
            let _ = registry
                .with(fmt::layer().with_target(true).with_thread_ids(true))
                .try_init();
        }
    }
}

fn load_config(path: &PathBuf, explicit: bool) -> Result<Config, Box<dyn std::error::Error>> {
    let mut config = if path.exists() {
        Config::load(path)?
    } else if explicit {
        return Err(format!("{} does not exist", path.display()).into());
    } else {
        eprintln!("{} not found; using built-in defaults", path.display());
        Config::default()
    };
    if let Ok(password) = std::env::var("RTFLOW_ADMIN_PASSWORD") {
        config.http.admin_password = password;
    }
    if let Ok(secret) = std::env::var("RTFLOW_TOKEN_SECRET")
        && config.auth.token_secrets.is_empty()
    {
        config.auth.token_secrets.push(secret);
    }
    config.validate()?;
    Ok(config)
}

const DEFAULT_CONFIG: &str = r#"# rtflow-server configuration
[server]
name = "rtflow"
shutdown_grace = "10s"
state_dir = "./state"

[log]
level = "info"
format = "text"

[rtsp]
enabled = true
listen = "0.0.0.0:8554"
session_timeout = "60s"
max_sessions = 256
dynamic_publishers = true

[rtmp]
enabled = true
listen = "0.0.0.0:1935"

[srt]
enabled = true
listen = "0.0.0.0:9000"

[webrtc]
enabled = true

[http]
listen = "0.0.0.0:3000"
admin = true
metrics = true
health = true
admin_username = "admin"
# Set a password here or via RTFLOW_ADMIN_PASSWORD. Empty disables the console.
admin_password = ""

[hls]
enabled = true
segment_duration = "1s"
window = 7
low_latency = true

[dash]
enabled = true

[flv]
enabled = true
ws_listen = "0.0.0.0:8088"

[recording]
enabled = false
root = "./recordings"
segment_duration = "10s"
retention = "7d"
max_disk_bytes = 53687091200
auto_start = true

[auth]
mode = "none"
realm = "rtflow"
enforce_path_rules = true

[keys]
default_ttl = "24h"
max_per_path = 10

[hooks]
timeout = "2s"
fail_closed = true

[discovery]
public = false
compat_mediamtx = false

[paths."**"]
enabled = true
publish = true
read = true
record = true
hls = true
dash = true
flv = true
webrtc = true
max_readers = 100
"#;
