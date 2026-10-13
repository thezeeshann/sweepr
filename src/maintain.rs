//! System maintenance: flush DNS, refresh QuickLook/icon caches, and compact
//! the SQLite databases behind Safari and Messages.
//!
//! Each task is independent, reports one of done / nothing-to-do / skipped /
//! failed, and never deletes user data. Tasks that need administrator rights say
//! so and are skipped rather than failing the run.

use crate::clean::{Running, Ui, should_protect_path};
use crate::status::human_bytes;
use crate::util::run;
use anyhow::{Result, bail};
use clap::Args as ClapArgs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Databases over this size are left alone (VACUUM needs ~2x the space and time).
const SQLITE_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// Skip databases whose free pages are under this share (already compact).
const MIN_FREELIST_PERCENT: u64 = 5;
const SHORT: Duration = Duration::from_secs(5);
const VACUUM_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Run only these tasks: dns, quicklook, databases (repeatable)
    #[arg(long = "only", value_name = "TASK")]
    pub only: Vec<String>,
    /// List the available tasks and exit
    #[arg(long)]
    pub list: bool,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Status {
    Done,
    Nothing,
    Skipped,
    Failed,
}

struct Report {
    status: Status,
    lines: Vec<String>,
}

impl Report {
    fn new(status: Status, line: impl Into<String>) -> Self {
        Self { status, lines: vec![line.into()] }
    }
}

type TaskFn = fn(&Ctx) -> Report;

const TASKS: &[(&str, &str, TaskFn)] = &[
    ("dns", "Flush DNS cache and verify Spotlight (needs administrator rights)", dns_and_spotlight),
    ("quicklook", "Refresh QuickLook thumbnails and the icon services cache", quicklook_and_icons),
    ("databases", "Compact Safari and Messages SQLite databases (apps must be closed)", databases),
];

struct Ctx {
    home: PathBuf,
    dry_run: bool,
}

pub fn run_cmd(args: Args, dry_run: bool) -> Result<()> {
    if args.list {
        for (name, desc, _) in TASKS {
            println!("{name:<10} {desc}");
        }
        return Ok(());
    }
    let unknown: Vec<_> = args.only.iter().filter(|o| !TASKS.iter().any(|(n, _, _)| n == &o.to_lowercase())).collect();
    if !unknown.is_empty() {
        bail!("unknown task {unknown:?}; available: dns, quicklook, databases");
    }
    let home = dirs::home_dir().filter(|h| h.is_absolute()).ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let ui = Ui::detect();
    let ctx = Ctx { home, dry_run };

    println!();
    if dry_run {
        println!("{}", ui.yellow("Dry run: nothing will be changed"));
    }
    let mut counts = [0usize; 4];
    for (name, desc, task) in TASKS {
        if !args.only.is_empty() && !args.only.iter().any(|o| o.eq_ignore_ascii_case(name)) {
            continue;
        }
        println!("\n{}", ui.bold(&format!("➤ {desc}")));
        let report = task(&ctx);
        let icon = match report.status {
            Status::Done => ui.green("✓"),
            Status::Nothing => ui.gray("·"),
            Status::Skipped => ui.yellow("⚠"),
            Status::Failed => ui.yellow("✗"),
        };
        counts[report.status as usize] += 1;
        for (i, l) in report.lines.iter().enumerate() {
            println!("  {} {l}", if i == 0 { icon.clone() } else { " ".into() });
        }
    }
    println!(
        "\n{} done · {} nothing to do · {} skipped · {} failed",
        counts[Status::Done as usize],
        counts[Status::Nothing as usize],
        counts[Status::Skipped as usize],
        counts[Status::Failed as usize]
    );
    if counts[Status::Failed as usize] > 0 {
        std::process::exit(1);
    }
    Ok(())
}

// ---------- DNS + Spotlight ----------

/// Can we run `sudo`? Uses a cached ticket silently, otherwise prompts once, and
/// only when a terminal is attached.
fn sudo_ready() -> bool {
    if run("/usr/bin/sudo", &["-n", "true"], SHORT).ok {
        return true;
    }
    if !std::io::stdin().is_terminal() {
        return false;
    }
    // Inherit the terminal so sudo can prompt for the password.
    std::process::Command::new("/usr/bin/sudo").arg("-v").status().is_ok_and(|s| s.success())
}

fn dns_and_spotlight(ctx: &Ctx) -> Report {
    let mut lines = Vec::new();
    let mut status = Status::Done;

    if ctx.dry_run {
        lines.push("Would flush the DNS cache and restart mDNSResponder".to_string());
    } else if !sudo_ready() {
        lines.push("DNS flush skipped: administrator access is required (run in a terminal and enter your password)".to_string());
        status = Status::Skipped;
    } else {
        let a = run("/usr/bin/sudo", &["-n", "/usr/bin/dscacheutil", "-flushcache"], SHORT);
        let b = run("/usr/bin/sudo", &["-n", "/usr/bin/killall", "-HUP", "mDNSResponder"], SHORT);
        if a.ok && b.ok {
            lines.push("DNS cache flushed".to_string());
        } else {
            lines.push("DNS flush failed".to_string());
            status = Status::Failed;
        }
    }

    // Verifying the index needs no privileges.
    let md = run("/usr/bin/mdutil", &["-s", "/"], SHORT);
    if !md.ok {
        lines.push("Could not verify the Spotlight index".to_string());
        if status == Status::Done {
            status = Status::Failed;
        }
    } else if md.stdout.to_lowercase().contains("indexing disabled") {
        lines.push("Spotlight indexing is disabled".to_string());
    } else {
        lines.push("Spotlight index verified".to_string());
    }
    Report { status, lines }
}

// ---------- QuickLook + icons ----------

fn quicklook_and_icons(ctx: &Ctx) -> Report {
    if ctx.dry_run {
        return Report {
            status: Status::Done,
            lines: vec!["Would reset the QuickLook thumbnail cache".into(), "Would reset QuickLook generators and icon services".into()],
        };
    }
    let thumbs = run("/usr/bin/qlmanage", &["-r", "cache"], Duration::from_secs(30));
    let gens = run("/usr/bin/qlmanage", &["-r"], Duration::from_secs(30));
    let mut lines = Vec::new();
    lines.push(if thumbs.ok { "QuickLook thumbnails refreshed".into() } else { "QuickLook thumbnail refresh failed".to_string() });
    lines.push(if gens.ok { "QuickLook generators and icon services reset".into() } else { "QuickLook generator reset failed".to_string() });
    let failed = [thumbs.ok, gens.ok].iter().filter(|o| !**o).count();
    Report { status: if failed == 0 { Status::Done } else { Status::Failed }, lines }
}

// ---------- SQLite databases ----------

fn db_candidates(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for pat in ["Library/Messages/chat.db", "Library/Safari/History.db", "Library/Safari/TopSites.db"] {
        let p = home.join(pat);
        if p.is_file() {
            out.push(p);
        }
    }
    // Mail keeps its index under a versioned folder; the protection rules exclude
    // ~/Library/Mail, so it is listed here only to be reported as skipped.
    if let Ok(rd) = std::fs::read_dir(home.join("Library/Mail")) {
        for v in rd.flatten().filter(|e| e.file_name().to_string_lossy().starts_with('V')) {
            let idx = v.path().join("MailData/Envelope Index");
            if idx.is_file() {
                out.push(idx);
            }
        }
    }
    out
}

#[derive(Debug, PartialEq)]
enum Probe {
    Sqlite,
    NotSqlite,
    /// Exists but cannot be read (typically missing Full Disk Access).
    Unreadable,
}

fn probe(path: &Path) -> Probe {
    use std::io::Read;
    let mut hdr = [0u8; 16];
    match std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut hdr)) {
        Ok(()) if &hdr == b"SQLite format 3\0" => Probe::Sqlite,
        Ok(()) => Probe::NotSqlite,
        // Too short to hold a header: not a database.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Probe::NotSqlite,
        Err(_) => Probe::Unreadable,
    }
}

/// (page_count, freelist_count, page_size), read straight from the 100-byte file
/// header so no SQLite connection is opened: a connection can create -wal/-shm
/// files, and a preview must not touch anything.
fn page_info(db: &Path) -> Option<(u64, u64, u64)> {
    use std::io::Read;
    let mut hdr = [0u8; 100];
    std::fs::File::open(db).ok()?.read_exact(&mut hdr).ok()?;
    parse_header(&hdr, db.metadata().ok()?.len())
}

fn parse_header(hdr: &[u8; 100], file_len: u64) -> Option<(u64, u64, u64)> {
    let be32 = |o: usize| u32::from_be_bytes([hdr[o], hdr[o + 1], hdr[o + 2], hdr[o + 3]]) as u64;
    let page_size = match u16::from_be_bytes([hdr[16], hdr[17]]) {
        1 => 65536,
        n if n >= 512 && n.is_power_of_two() => n as u64,
        _ => return None,
    };
    // The in-header page count is only trusted when it matches the change counter.
    let pages = if be32(24) == be32(92) && be32(28) > 0 { be32(28) } else { file_len / page_size };
    Some((pages, be32(36), page_size))
}

fn databases(ctx: &Ctx) -> Report {
    // Vacuuming a database an app has open can corrupt it, so refuse if any owner runs.
    let running = Running::detect();
    let busy: Vec<&str> = ["Mail", "Safari", "Messages"].into_iter().filter(|a| running.process_running(a)).collect();
    if !busy.is_empty() {
        return Report::new(Status::Skipped, format!("Close these apps first: {}", busy.join(", ")));
    }

    let mut lines = Vec::new();
    let (mut vacuumed, mut compact, mut failed, mut too_big, mut protected) = (0, 0, 0, 0, 0);
    let mut saved: u64 = 0;
    for db in db_candidates(&ctx.home) {
        let shown = db.strip_prefix(&ctx.home).map(|p| format!("~/{}", p.display())).unwrap_or_else(|_| db.display().to_string());
        if should_protect_path(&db.to_string_lossy()) {
            protected += 1;
            continue;
        }
        match probe(&db) {
            Probe::Sqlite => {}
            Probe::NotSqlite => continue,
            Probe::Unreadable => {
                failed += 1;
                lines.push(format!("{shown}: cannot read it (grant your terminal Full Disk Access in System Settings)"));
                continue;
            }
        }
        let size = db.metadata().map(|m| m.len()).unwrap_or(0);
        if size > SQLITE_MAX_BYTES {
            too_big += 1;
            lines.push(format!("{shown} is {} (over the {} limit), skipped", human_bytes(size), human_bytes(SQLITE_MAX_BYTES)));
            continue;
        }
        let Some((pages, free, page_size)) = page_info(&db) else {
            failed += 1;
            lines.push(format!("{shown}: cannot read (grant your terminal Full Disk Access)"));
            continue;
        };
        if pages == 0 || free * 100 < pages * MIN_FREELIST_PERCENT {
            compact += 1;
            continue;
        }
        let reclaim = free * page_size;
        if ctx.dry_run {
            vacuumed += 1;
            saved += reclaim;
            lines.push(format!("Would compact {shown} (about {} reclaimable)", human_bytes(reclaim)));
            continue;
        }
        let integrity = run("/usr/bin/sqlite3", &[&db.to_string_lossy(), "PRAGMA integrity_check;"], Duration::from_secs(60));
        if !integrity.ok || integrity.stdout.trim() != "ok" {
            failed += 1;
            lines.push(format!("{shown}: integrity check did not pass, left untouched"));
            continue;
        }
        let v = run("/usr/bin/sqlite3", &[&db.to_string_lossy(), "VACUUM;"], VACUUM_TIMEOUT);
        if v.ok {
            vacuumed += 1;
            let after = db.metadata().map(|m| m.len()).unwrap_or(size);
            saved += size.saturating_sub(after);
            lines.push(format!("Compacted {shown}: {} → {}", human_bytes(size), human_bytes(after)));
        } else {
            failed += 1;
            lines.push(format!("{shown}: {}", if v.timed_out { "timed out" } else { "VACUUM failed" }));
        }
    }

    let verb = if ctx.dry_run { "would compact" } else { "compacted" };
    let mut head = if vacuumed > 0 {
        format!("{vacuumed} database(s) {verb}, {} {}", human_bytes(saved), if ctx.dry_run { "reclaimable" } else { "reclaimed" })
    } else if failed == 0 && too_big == 0 && compact > 0 {
        "All databases already compact".to_string()
    } else if failed == 0 && too_big == 0 {
        "No databases found to optimize".to_string()
    } else {
        "No databases compacted".to_string()
    };
    if compact > 0 && vacuumed > 0 {
        head.push_str(&format!(" · {compact} already compact"));
    }
    if protected > 0 {
        head.push_str(&format!(" · {protected} protected (e.g. Mail) left alone"));
    }
    lines.insert(0, head);
    let status = if failed > 0 {
        Status::Failed
    } else if vacuumed > 0 {
        Status::Done
    } else if too_big > 0 {
        Status::Skipped
    } else {
        Status::Nothing
    };
    Report { status, lines }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite_db(path: &Path, rows: usize, delete: bool) {
        let sql = format!(
            "CREATE TABLE t(a BLOB); WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<{rows}) \
             INSERT INTO t SELECT zeroblob(2000) FROM c; {}",
            if delete { "DELETE FROM t;" } else { "" }
        );
        assert!(run("/usr/bin/sqlite3", &[&path.to_string_lossy(), &sql], Duration::from_secs(20)).ok);
    }

    #[test]
    fn detects_sqlite_files_by_header() {
        let d = std::env::temp_dir().join(format!("sweepr-sqlite-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        sqlite_db(&d.join("a.db"), 10, false);
        std::fs::write(d.join("b.db"), "not a database at all").unwrap();
        assert_eq!(probe(&d.join("a.db")), Probe::Sqlite);
        assert_eq!(probe(&d.join("b.db")), Probe::NotSqlite);
        std::fs::write(d.join("empty.db"), "").unwrap();
        assert_eq!(probe(&d.join("empty.db")), Probe::NotSqlite);
        // A file we are not allowed to read must surface as Unreadable, never as "absent".
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(d.join("a.db"), std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(probe(&d.join("a.db")), Probe::Unreadable);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn vacuum_shrinks_a_fragmented_database_and_keeps_it_valid() {
        let d = std::env::temp_dir().join(format!("sweepr-vacuum-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let db = d.join("x.db");
        sqlite_db(&db, 500, true); // ~1 MB of pages, all freed
        let (pages, free, _) = page_info(&db).unwrap();
        assert!(free * 100 >= pages * MIN_FREELIST_PERCENT, "fixture should be fragmented");
        let before = db.metadata().unwrap().len();
        assert!(run("/usr/bin/sqlite3", &[&db.to_string_lossy(), "VACUUM;"], VACUUM_TIMEOUT).ok);
        assert!(db.metadata().unwrap().len() < before);
        assert_eq!(run("/usr/bin/sqlite3", &[&db.to_string_lossy(), "PRAGMA integrity_check;"], SHORT).stdout.trim(), "ok");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn compact_database_is_not_a_candidate() {
        let d = std::env::temp_dir().join(format!("sweepr-compact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        sqlite_db(&d.join("y.db"), 100, false);
        let (pages, free, _) = page_info(&d.join("y.db")).unwrap();
        assert!(free * 100 < pages * MIN_FREELIST_PERCENT);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dry_run_changes_nothing_and_never_needs_sudo() {
        let ctx = Ctx { home: std::env::temp_dir().join("sweepr-no-such-home"), dry_run: true };
        let r = dns_and_spotlight(&ctx);
        assert!(r.lines[0].starts_with("Would flush"));
        let r = quicklook_and_icons(&ctx);
        assert_eq!(r.status, Status::Done);
        assert!(r.lines.iter().all(|l| l.starts_with("Would")));
    }

    #[test]
    fn reads_page_counts_from_the_sqlite_header() {
        let mut h = [0u8; 100];
        h[16..18].copy_from_slice(&4096u16.to_be_bytes());
        h[24..28].copy_from_slice(&7u32.to_be_bytes());
        h[92..96].copy_from_slice(&7u32.to_be_bytes());
        h[28..32].copy_from_slice(&1000u32.to_be_bytes());
        h[36..40].copy_from_slice(&120u32.to_be_bytes());
        assert_eq!(parse_header(&h, 0), Some((1000, 120, 4096)));
        h[92..96].copy_from_slice(&8u32.to_be_bytes()); // stale count: fall back to file size
        assert_eq!(parse_header(&h, 4096 * 900), Some((900, 120, 4096)));
        h[16..18].copy_from_slice(&1u16.to_be_bytes());
        assert_eq!(parse_header(&h, 65536 * 3).unwrap().2, 65536);
        h[16..18].copy_from_slice(&1000u16.to_be_bytes());
        assert_eq!(parse_header(&h, 0), None, "not a power of two");
    }

    #[test]
    fn unknown_task_is_rejected() {
        let a = Args { only: vec!["bogus".into()], list: false };
        assert!(run_cmd(a, true).is_err());
    }
}
