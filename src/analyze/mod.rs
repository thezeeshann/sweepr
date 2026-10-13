//! Disk analyzer: interactive TUI (or JSON) showing where space went.

mod scan;
mod tui;

use anyhow::{Result, bail};
use clap::Args as ClapArgs;
use serde_json::json;
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Folder to analyze (omit for the overview of your Mac)
    pub path: Option<PathBuf>,
    /// Print the analysis as JSON instead of opening the TUI
    #[arg(long)]
    pub json: bool,
    /// Smallest file the large-files view lists (e.g. 100MB, 1GB)
    #[arg(long, default_value = "100MB", value_name = "SIZE")]
    pub min_size: String,
}

pub fn run_cmd(args: Args, dry_run: bool) -> Result<()> {
    let home = dirs::home_dir().filter(|h| h.is_absolute()).ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let min = parse_size(&args.min_size)?;
    let start = match &args.path {
        Some(p) => {
            let p = p.canonicalize().map_err(|e| anyhow::anyhow!("cannot open {}: {e}", p.display()))?;
            if !p.is_dir() {
                bail!("{} is not a folder", p.display());
            }
            Some(p)
        }
        None => None,
    };

    if args.json {
        return print_json(start, &home);
    }
    if !std::io::stdout().is_terminal() {
        bail!("the analyzer needs a terminal; use --json for scripts");
    }
    tui::run_tui(start, home, min, dry_run)
}

fn print_json(start: Option<PathBuf>, home: &std::path::Path) -> Result<()> {
    let (path, overview, entries): (PathBuf, bool, Vec<scan::Entry>) = match start {
        Some(p) => {
            let e = scan::measure_blocking(&p)?;
            (p, false, e)
        }
        None => {
            let rows = tui::overview_rows(home);
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (tx, rx) = std::sync::mpsc::channel();
            scan::spawn_sizes(rows.iter().map(|(_, p)| p.clone()).collect(), cancel, tx);
            let mut sizes = std::collections::HashMap::new();
            for _ in 0..rows.len() {
                if let Ok(m) = rx.recv() {
                    sizes.insert(m.path.clone(), m.size);
                }
            }
            let mut v: Vec<scan::Entry> = rows
                .into_iter()
                .map(|(name, path)| {
                    let s = sizes.get(&path).copied().unwrap_or_default();
                    scan::Entry { name, path, is_dir: true, size: Some(s.bytes), files: s.files, modified: None, partial: s.partial }
                })
                .collect();
            v.sort_by(|a, b| b.size.cmp(&a.size));
            (home.to_path_buf(), true, v)
        }
    };
    let total_size: u64 = if overview { 0 } else { entries.iter().filter_map(|e| e.size).sum() };
    let total_files: u64 = if overview { 0 } else { entries.iter().map(|e| e.files).sum() };
    let out = json!({
        "path": path,
        "overview": overview,
        "entries": entries.iter().map(|e| json!({
            "name": e.name, "path": e.path, "size": e.size.unwrap_or(0), "is_dir": e.is_dir,
            "partial": e.partial,
        })).collect::<Vec<_>>(),
        "total_size": total_size,
        "total_files": total_files,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// "100MB", "1.5G", "500m", "2048" (bytes). Units are 1024-based, like the rest of the UI.
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim().to_lowercase();
    let split = t.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.trim().parse().map_err(|_| anyhow::anyhow!("invalid size {s:?} (try 100MB or 1GB)"))?;
    let mult: f64 = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => bail!("invalid size unit in {s:?} (use KB, MB, GB or TB)"),
    };
    if !n.is_finite() || n <= 0.0 {
        bail!("size must be greater than zero: {s:?}");
    }
    Ok((n * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("100MB").unwrap(), 100 * 1024 * 1024);
        assert_eq!(parse_size("1.5g").unwrap(), (1.5 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(parse_size("2048").unwrap(), 2048);
        assert_eq!(parse_size(" 5 kb ").unwrap(), 5120);
        for bad in ["", "MB", "0", "-5MB", "12XB", "abc"] {
            assert!(parse_size(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn json_for_a_folder_matches_the_recovered_schema() {
        let d = std::env::temp_dir().join(format!("sweepr-json-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/a"), vec![0u8; 200_000]).unwrap();
        std::fs::write(d.join("b"), vec![0u8; 100_000]).unwrap();
        let entries = scan::measure_blocking(&d).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_dir && entries[0].size.unwrap() >= 200_000);
        let _ = std::fs::remove_dir_all(&d);
    }
}
