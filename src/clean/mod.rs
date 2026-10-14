//! Deep clean: caches, logs, leftovers and orphaned app data.
//!
//! Flow: scan everything (nothing is deleted), show what was found, then delete
//! only after confirmation. `--dry-run` stops after the scan.

mod engine;
mod orphans;
mod running;
pub mod safety;
mod targets;
mod whitelist;

pub use engine::disk_usage;
pub use orphans::is_reverse_dns;
pub use running::Running;
pub use safety::should_protect_path;

use crate::status::human_bytes;
use anyhow::{Result, bail};
use clap::Args as ClapArgs;
use engine::{Engine, Item};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Delete without asking for confirmation
    #[arg(short, long)]
    pub yes: bool,
    /// Show the active protection rules and where to edit them, then exit
    #[arg(long)]
    pub whitelist: bool,
    /// Protect a path or glob from cleaning (saved to the whitelist)
    #[arg(long, value_name = "PATH")]
    pub protect: Vec<String>,
    /// Remove a path or glob from the whitelist
    #[arg(long, value_name = "PATH")]
    pub unprotect: Vec<String>,
    /// Only run matching sections (repeatable, case-insensitive prefix)
    #[arg(long = "only", value_name = "SECTION")]
    pub only: Vec<String>,
    /// List every item found and every path the safety checks refused
    #[arg(short, long)]
    pub verbose: bool,
}

pub fn run(args: Args, dry_run: bool) -> Result<()> {
    // Under sudo $HOME may be root's: cleaning that is never what was meant.
    if unsafe { libc::geteuid() } == 0 {
        bail!("run as your normal user, not root: this cleans your own home directory");
    }
    let home = dirs::home_dir().filter(|h| h.is_absolute()).ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let ui = Ui::detect();

    if !args.protect.is_empty() || !args.unprotect.is_empty() || args.whitelist {
        return manage_whitelist(&args, &home, &ui, dry_run);
    }

    let sections: Vec<_> = targets::SECTIONS
        .iter()
        .filter(|(name, _)| {
            args.only.is_empty() || args.only.iter().any(|o| name.to_lowercase().starts_with(&o.to_lowercase()))
        })
        .collect();
    if sections.is_empty() {
        let names: Vec<_> = targets::SECTIONS.iter().map(|(n, _)| *n).collect();
        bail!("no section matches {:?}; available: {}", args.only, names.join(", "));
    }

    println!();
    if dry_run {
        println!("{}", ui.yellow("Dry run: preview only, nothing will be deleted"));
    }
    let free_before = free_space(&home);
    println!("{} Free space: {}", ui.blue("●"), free_before.map(human_bytes).unwrap_or_else(|| "unknown".into()));

    let wl = whitelist::Whitelist::load(&home);
    let (core, custom) = wl.counts();
    println!(
        "{} Whitelist: {core} core{} patterns active",
        ui.blue("✓"),
        if custom > 0 { format!(" + {custom} custom") } else { String::new() }
    );

    let mut engine = Engine::new(home.clone(), wl, running::Running::detect());
    for (name, run_section) in &sections {
        engine.section = (*name).to_string();
        ui.progress(&format!("Scanning {name}…"));
        let before = engine.scan.items.len();
        run_section(&mut engine);
        ui.clear_progress();
        print_section(&ui, name, &engine.scan.items[before..], args.verbose);
    }
    for note in &engine.notes {
        println!("  {} {note}", ui.gray("⚠"));
    }
    if args.verbose {
        for (path, why) in &engine.scan.refused {
            println!("  {} refused {path}: {why}", ui.gray("⚠"));
        }
    }

    let items = std::mem::take(&mut engine.scan.items);
    let total: u64 = items.iter().map(|i| i.size).sum();
    let categories = distinct_labels(&items);

    println!();
    if items.is_empty() {
        println!("{}", ui.green("Nothing to clean: the system is already tidy."));
        return Ok(());
    }
    let mut stats = format!("Potential space: {} | Items: {} | Categories: {}", ui.green(&human_bytes(total)), items.len(), categories);
    if engine.scan.protected > 0 {
        stats.push_str(&format!(" | Protected: {}", engine.scan.protected));
    }
    println!("{stats}");
    if engine.scan.deferred > 0 {
        println!("{}", ui.gray(&format!("  {} item(s) skipped because their app is running", engine.scan.deferred)));
    }

    if dry_run {
        if !args.verbose {
            println!("Use {} to list every path", ui.gray("--verbose"));
        }
        println!("Use {} to protect paths from cleaning", ui.gray("sw clean --protect <path>"));
        println!("{}", ui.yellow("Dry run complete: nothing was deleted or written"));
        return Ok(());
    }

    if !args.yes && !confirm(total, items.len())? {
        println!("Cancelled. Nothing was deleted.");
        return Ok(());
    }

    let outcome = engine.execute(&items);
    println!();
    println!(
        "{} Cleaned {} item(s), freed about {}",
        ui.green("✓"),
        outcome.removed,
        ui.green(&human_bytes(outcome.freed))
    );
    if let (Some(before), Some(after)) = (free_before, free_space(&home)) {
        let delta = after as i64 - before as i64;
        println!("  Free space: {} ({}{})", human_bytes(after), if delta >= 0 { "+" } else { "-" }, human_bytes(delta.unsigned_abs()));
    }
    if !outcome.failed.is_empty() {
        println!("{} {} item(s) could not be removed:", ui.gray("⚠"), outcome.failed.len());
        for (path, why) in outcome.failed.iter().take(if args.verbose { usize::MAX } else { 5 }) {
            println!("    {}: {why}", path.display());
        }
        if !args.verbose && outcome.failed.len() > 5 {
            println!("    … and {} more (use --verbose)", outcome.failed.len() - 5);
        }
    }
    Ok(())
}

fn print_section(ui: &Ui, name: &str, items: &[Item], verbose: bool) {
    if items.is_empty() {
        return;
    }
    println!("\n{}", ui.bold(&format!("➤ {name}")));
    // Aggregate by label, keeping first-seen order.
    let mut groups: Vec<(String, u64, usize)> = Vec::new();
    for it in items {
        match groups.iter_mut().find(|g| g.0 == it.label) {
            Some(g) => {
                g.1 += it.size;
                g.2 += 1;
            }
            None => groups.push((it.label.clone(), it.size, 1)),
        }
    }
    for (label, size, n) in groups {
        let count = if n > 1 { format!(" ({n} items)") } else { String::new() };
        println!("  {} {label} · {}{count}", ui.green("✓"), human_bytes(size));
    }
    if verbose {
        for it in items {
            println!("      {}  {}", ui.gray(&human_bytes(it.size)), it.path.display());
        }
    }
}

fn distinct_labels(items: &[Item]) -> usize {
    let mut seen = std::collections::HashSet::new();
    items.iter().filter(|i| seen.insert((&i.section, &i.label))).count()
}

fn confirm(total: u64, count: usize) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("refusing to delete without confirmation in a non-interactive session; pass --yes to proceed");
    }
    print!("Permanently delete {count} item(s), about {}? [y/N] ", human_bytes(total));
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn manage_whitelist(args: &Args, home: &Path, ui: &Ui, dry_run: bool) -> Result<()> {
    if dry_run && !(args.protect.is_empty() && args.unprotect.is_empty()) {
        for p in &args.protect {
            println!("{} would protect {p}", ui.gray("·"));
        }
        for p in &args.unprotect {
            println!("{} would stop protecting {p}", ui.gray("·"));
        }
        println!("{}", ui.yellow("Dry run: the whitelist was not changed"));
        return Ok(());
    }
    for p in &args.protect {
        whitelist::add(home, p)?;
        println!("{} protected {p}", ui.green("✓"));
    }
    for p in &args.unprotect {
        if whitelist::remove(home, p)? {
            println!("{} no longer protected: {p}", ui.green("✓"));
        } else {
            println!("{} not in the whitelist: {p}", ui.gray("⚠"));
        }
    }
    if args.whitelist || (args.protect.is_empty() && args.unprotect.is_empty()) {
        let wl = whitelist::Whitelist::load(home);
        println!("Whitelist file: {}", wl.file.display());
        println!("Active protection patterns:");
        for p in wl.patterns() {
            println!("  {p}");
        }
    }
    Ok(())
}

/// Free bytes on the volume holding `path` (APFS reports container-wide free space).
fn free_space(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.to_str()?).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a zeroed statvfs.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    (rc == 0).then(|| st.f_bavail as u64 * st.f_frsize)
}

/// Minimal ANSI styling that switches itself off for pipes and NO_COLOR.
pub struct Ui {
    color: bool,
    tty: bool,
}

impl Ui {
    pub fn detect() -> Self {
        let tty = std::io::stdout().is_terminal();
        Self { color: tty && std::env::var_os("NO_COLOR").is_none(), tty }
    }
    pub fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    pub fn green(&self, s: &str) -> String { self.paint("32", s) }
    pub fn yellow(&self, s: &str) -> String { self.paint("33", s) }
    pub fn blue(&self, s: &str) -> String { self.paint("34", s) }
    pub fn gray(&self, s: &str) -> String { self.paint("90", s) }
    pub fn bold(&self, s: &str) -> String { self.paint("1", s) }
    pub fn progress(&self, msg: &str) {
        if self.tty {
            eprint!("\r\x1b[2K  {}", self.gray(msg));
            let _ = std::io::stderr().flush();
        }
    }
    pub fn clear_progress(&self) {
        if self.tty {
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }
}
