use anyhow::{bail, Context, Result};
use std::env;
use std::path::PathBuf;
use std::time::Duration;

pub struct Config {
    pub db_path: PathBuf,
    pub poll_interval: Duration,
    pub idle_threshold: Duration,
    pub rotate_segment_every: Duration,
}

impl Config {
    pub fn from_args() -> Result<Self> {
        let mut db_path = default_db_path();
        let mut poll_ms: u64 = 1000;
        let mut idle_secs: u64 = 300;
        let mut rotate_secs: u64 = 10;

        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--db" => {
                    let value = args.next().context("missing value for --db")?;
                    db_path = PathBuf::from(value);
                }
                "--poll-ms" => {
                    let value = args.next().context("missing value for --poll-ms")?;
                    poll_ms = value
                        .parse::<u64>()
                        .with_context(|| format!("invalid --poll-ms value: {value}"))?;
                }
                "--idle-secs" => {
                    let value = args.next().context("missing value for --idle-secs")?;
                    idle_secs = value
                        .parse::<u64>()
                        .with_context(|| format!("invalid --idle-secs value: {value}"))?;
                }
                "--rotate-secs" => {
                    let value = args.next().context("missing value for --rotate-secs")?;
                    rotate_secs = value
                        .parse::<u64>()
                        .with_context(|| format!("invalid --rotate-secs value: {value}"))?;
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                _ => bail!("unknown argument: {arg}"),
            }
        }

        if poll_ms == 0 {
            bail!("--poll-ms must be greater than zero");
        }
        if idle_secs == 0 {
            bail!("--idle-secs must be greater than zero");
        }
        if rotate_secs == 0 {
            bail!("--rotate-secs must be greater than zero");
        }

        Ok(Self {
            db_path,
            poll_interval: Duration::from_millis(poll_ms),
            idle_threshold: Duration::from_secs(idle_secs),
            rotate_segment_every: Duration::from_secs(rotate_secs),
        })
    }
}

fn default_db_path() -> PathBuf {
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local)
            .join("LimeTrace")
            .join("tracker.db");
    }
    PathBuf::from("data").join("tracker.db")
}

fn print_help() {
    println!(
        "\
LimeTrace Backend (Windows)

Usage:
  limetrace-backend [--db <path>] [--poll-ms <ms>] [--idle-secs <s>] [--rotate-secs <s>]

Options:
  --db           SQLite file path (default: %LOCALAPPDATA%\\LimeTrace\\tracker.db)
  --poll-ms      Sampling interval in milliseconds (default: 1000)
  --idle-secs    Idle threshold in seconds (default: 300)
  --rotate-secs  Force-segment rotation interval in seconds (default: 10)
  -h, --help     Print this help"
    );
}

