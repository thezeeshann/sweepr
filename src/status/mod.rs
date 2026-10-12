//! Live system monitoring: CPU, memory, disk, disk I/O, network and processes.
//!
//! The JSON field names mirror the schema observed from Mole's `status-go`
//! (`--json`), so tooling written against that output keeps working.

mod battery;
mod collect;
mod controls;
mod tui;

use anyhow::{Result, bail};
use clap::Args as ClapArgs;
use std::time::Duration;

pub use collect::{Collector, human_bytes};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Print one metrics snapshot as JSON instead of opening the TUI
    #[arg(long)]
    pub json: bool,
    /// Stream snapshots continuously as newline-delimited JSON
    #[arg(long)]
    pub watch: bool,
    /// With --watch or the TUI, the collection interval (e.g. 500ms, 1s, 2)
    #[arg(long, default_value = "1s")]
    pub interval: String,
    /// Number of top processes to report
    #[arg(long, default_value_t = 10)]
    pub top: usize,
    /// Also show what Music/Spotify is playing (macOS asks once for permission)
    #[arg(long)]
    pub now_playing: bool,
}

pub fn run(args: Args) -> Result<()> {
    let interval = parse_interval(&args.interval)?;
    if args.json {
        let mut c = Collector::new(args.top);
        c.refresh_controls_blocking(args.now_playing);
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(400)));
        let snap = c.collect();
        println!("{}", serde_json::to_string_pretty(&snap)?);
        return Ok(());
    }
    if args.watch {
        return watch(args.top, interval, args.now_playing);
    }
    tui::run(args.top.max(25), interval, args.now_playing)
}

fn watch(top: usize, interval: Duration, now_playing: bool) -> Result<()> {
    let mut c = Collector::new(top);
    c.start_controls(now_playing);
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(400)));
    loop {
        let snap = c.collect();
        // A closed pipe (e.g. `| head`) should end the stream quietly.
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{}", serde_json::to_string(&snap)?).is_err() {
            return Ok(());
        }
        let _ = out.flush();
        drop(out);
        std::thread::sleep(interval);
    }
}

/// Accepts `500ms`, `1s`, `2m`, or a bare number of seconds (`1.5`).
pub fn parse_interval(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: f64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid --interval {s:?}"))?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        _ => bail!("invalid --interval {s:?} (use ms, s or m)"),
    };
    if !secs.is_finite() || secs <= 0.0 {
        bail!("invalid --interval {s:?} (must be > 0)");
    }
    Ok(Duration::from_secs_f64(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_parsing() {
        assert_eq!(parse_interval("1s").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_interval("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_interval("2").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_interval("1m").unwrap(), Duration::from_secs(60));
        assert!(parse_interval("0s").is_err());
        assert!(parse_interval("-1").is_err());
        assert!(parse_interval("abc").is_err());
        assert!(parse_interval("5h").is_err());
    }
}
