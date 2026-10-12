//! Disk scanning for the analyzer: directory listings, parallel directory
//! sizes, large-file search and the overview insights.
//!
//! Sizes are allocated bytes on disk (what actually frees up), symlinks are never
//! followed and hard links count once. A folder macOS will not let us read is
//! flagged `partial` instead of being silently counted as empty.

use crate::util::run;
use std::collections::{HashSet, VecDeque};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// `None` while a directory is still being measured.
    pub size: Option<u64>,
    pub files: u64,
    pub modified: Option<SystemTime>,
    /// Some contents could not be read, so `size` is a lower bound.
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DirSize {
    pub bytes: u64,
    pub files: u64,
    pub partial: bool,
}

/// Paths that would double-count or never hold real files: APFS firmlinks
/// (`/System/Volumes/Data` mirrors the whole disk), other volumes and devices.
pub fn is_ignored(path: &Path) -> bool {
    matches!(path.to_str(), Some("/dev" | "/Volumes" | "/System/Volumes" | "/private/var/vm" | "/.vol" | "/net" | "/home"))
}

/// Immediate children of `dir`. Files get their size now; directories get `None`.
pub fn list_dir(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let path = e.path();
        if is_ignored(&path) {
            continue;
        }
        let Ok(m) = e.metadata().or_else(|_| std::fs::symlink_metadata(&path)) else {
            continue;
        };
        // `DirEntry::metadata` does not follow symlinks, so a link to a folder is a file here.
        let is_dir = m.file_type().is_dir();
        out.push(Entry {
            name: e.file_name().to_string_lossy().to_string(),
            is_dir,
            size: (!is_dir).then(|| m.blocks() * 512),
            files: u64::from(!is_dir),
            modified: m.modified().ok(),
            partial: false,
            path,
        });
    }
    Ok(out)
}

/// Total allocated size under `path`. `None` if cancelled part-way.
pub fn dir_size(path: &Path, cancel: &AtomicBool) -> Option<DirSize> {
    let mut total = DirSize::default();
    let mut links: HashSet<(u64, u64)> = HashSet::new();
    let mut stack = vec![path.to_path_buf()];
    let mut n = 0u32;
    while let Some(p) = stack.pop() {
        n += 1;
        if n.is_multiple_of(512) && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let Ok(m) = std::fs::symlink_metadata(&p) else {
            total.partial = true;
            continue;
        };
        if m.file_type().is_dir() {
            total.bytes += m.blocks() * 512;
            match std::fs::read_dir(&p) {
                Ok(rd) => {
                    for e in rd.flatten() {
                        let c = e.path();
                        if !is_ignored(&c) {
                            stack.push(c);
                        }
                    }
                }
                Err(_) => total.partial = true,
            }
        } else if m.nlink() > 1 && !links.insert((m.dev(), m.ino())) {
            continue;
        } else {
            total.bytes += m.blocks() * 512;
            total.files += 1;
        }
    }
    Some(total)
}

pub struct SizeMsg {
    pub path: PathBuf,
    pub size: DirSize,
}

/// Measure many directories on a small pool, sending each result as it finishes.
/// Setting `cancel` stops the workers promptly.
pub fn spawn_sizes(paths: Vec<PathBuf>, cancel: Arc<AtomicBool>, tx: Sender<SizeMsg>) {
    let queue = Arc::new(Mutex::new(paths.into_iter().collect::<VecDeque<_>>()));
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
    for _ in 0..workers {
        let (queue, cancel, tx) = (queue.clone(), cancel.clone(), tx.clone());
        std::thread::spawn(move || {
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let Some(p) = queue.lock().unwrap().pop_front() else { return };
                match dir_size(&p, &cancel) {
                    Some(size) => {
                        if tx.send(SizeMsg { path: p, size }).is_err() {
                            return; // receiver gone: the UI moved on
                        }
                    }
                    None => return,
                }
            }
        });
    }
}

/// Measure `dir`'s children and wait for all of them (used by `--json`).
pub fn measure_blocking(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut entries = list_dir(dir)?;
    let pending: Vec<PathBuf> = entries.iter().filter(|e| e.is_dir).map(|e| e.path.clone()).collect();
    let n = pending.len();
    let (tx, rx) = std::sync::mpsc::channel();
    spawn_sizes(pending, Arc::new(AtomicBool::new(false)), tx);
    for _ in 0..n {
        match rx.recv() {
            Ok(m) => {
                if let Some(e) = entries.iter_mut().find(|e| e.path == m.path) {
                    e.size = Some(m.size.bytes);
                    e.files = m.size.files;
                    e.partial = m.size.partial;
                }
            }
            Err(_) => break,
        }
    }
    entries.sort_by(|a, b| b.size.unwrap_or(0).cmp(&a.size.unwrap_or(0)).then(a.name.cmp(&b.name)));
    Ok(entries)
}

#[derive(Debug, Default)]
pub struct LargeFiles {
    /// (path, allocated bytes, modified), biggest first.
    pub files: Vec<(PathBuf, u64, Option<SystemTime>)>,
    pub via_spotlight: bool,
}

/// Files of at least `min_bytes` under `root`. Uses Spotlight (instant, but only
/// knows what is indexed) and falls back to walking the tree if it is unavailable.
pub fn large_files(root: &Path, min_bytes: u64, limit: usize, cancel: &AtomicBool) -> LargeFiles {
    let query = format!("kMDItemFSSize >= {min_bytes}");
    let out = run("/usr/bin/mdfind", &["-onlyin", &root.to_string_lossy(), &query], Duration::from_secs(20));
    let mut found: Vec<(PathBuf, u64, Option<SystemTime>)> = Vec::new();
    let via_spotlight = out.ok;
    if out.ok {
        for line in out.stdout.lines() {
            let p = PathBuf::from(line);
            if let Ok(m) = std::fs::symlink_metadata(&p)
                && m.file_type().is_file() && m.blocks() * 512 >= min_bytes / 2 {
                    found.push((p, m.blocks() * 512, m.modified().ok()));
                }
        }
    } else {
        walk_large(root, min_bytes, cancel, &mut found);
    }
    found.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    found.truncate(limit);
    LargeFiles { files: found, via_spotlight }
}

fn walk_large(root: &Path, min: u64, cancel: &AtomicBool, out: &mut Vec<(PathBuf, u64, Option<SystemTime>)>) {
    let mut stack = vec![root.to_path_buf()];
    let mut n = 0u32;
    while let Some(d) = stack.pop() {
        n += 1;
        if n.is_multiple_of(256) && cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if is_ignored(&p) {
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            if m.file_type().is_dir() {
                stack.push(p);
            } else if m.file_type().is_file() && m.blocks() * 512 >= min {
                out.push((p, m.blocks() * 512, m.modified().ok()));
            }
        }
    }
}

/// Total size of files in `dir` not modified for `days` (the "old downloads" insight).
pub fn old_files_size(dir: &Path, days: u64) -> u64 {
    let Some(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(days * 86_400)) else { return 0 };
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let cancel = AtomicBool::new(false);
    rd.flatten()
        .filter(|e| e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < cutoff))
        .map(|e| dir_size(&e.path(), &cancel).map(|d| d.bytes).unwrap_or(0))
        .sum()
}

/// Number of Time Machine local snapshots (they hold space `df` does not show).
pub fn local_snapshot_count() -> Option<usize> {
    let out = run("/usr/bin/tmutil", &["listlocalsnapshots", "/"], Duration::from_secs(8));
    out.ok.then(|| out.stdout.lines().filter(|l| l.starts_with("com.apple.TimeMachine.")).count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sweepr-an-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }
    fn file(p: &Path, n: usize) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![b'x'; n]).unwrap();
    }

    #[test]
    fn firmlinks_and_devices_are_ignored() {
        for p in ["/System/Volumes", "/Volumes", "/dev"] {
            assert!(is_ignored(Path::new(p)), "{p}");
        }
        assert!(!is_ignored(Path::new("/Users")));
        assert!(!is_ignored(Path::new("/System/Library")));
    }

    #[test]
    fn list_dir_gives_file_sizes_now_and_dirs_later() {
        let d = tmp("list");
        file(&d.join("a.bin"), 100_000);
        file(&d.join("sub/b.bin"), 100_000);
        let mut e = list_dir(&d).unwrap();
        e.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(e.len(), 2);
        assert!(!e[0].is_dir && e[0].size.unwrap() >= 100_000);
        assert!(e[1].is_dir && e[1].size.is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn symlinks_are_not_followed() {
        let d = tmp("symlink");
        file(&d.join("real/big.bin"), 500_000);
        fs::create_dir_all(d.join("scan")).unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("scan/link")).unwrap();
        let s = dir_size(&d.join("scan"), &AtomicBool::new(false)).unwrap();
        assert!(s.bytes < 100_000, "followed the link: {}", s.bytes);
        let e = list_dir(&d.join("scan")).unwrap();
        assert!(!e[0].is_dir, "a symlink to a folder must not be treated as a folder");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn dir_size_sums_nested_files_and_counts_hardlinks_once() {
        let d = tmp("sum");
        file(&d.join("a/1.bin"), 200_000);
        file(&d.join("a/b/2.bin"), 200_000);
        fs::hard_link(d.join("a/1.bin"), d.join("a/1-link.bin")).unwrap();
        let s = dir_size(&d, &AtomicBool::new(false)).unwrap();
        assert_eq!(s.files, 2);
        assert!(s.bytes >= 400_000 && s.bytes < 560_000, "{}", s.bytes);
        assert!(!s.partial);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unreadable_folder_is_flagged_partial_not_silently_zero() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("partial");
        file(&d.join("open/x.bin"), 50_000);
        file(&d.join("locked/y.bin"), 50_000);
        fs::set_permissions(d.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let s = dir_size(&d, &AtomicBool::new(false)).unwrap();
        assert!(s.partial, "an unreadable folder must be reported");
        fs::set_permissions(d.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cancel_stops_a_scan() {
        let d = tmp("cancel");
        for i in 0..2000 {
            file(&d.join(format!("f{i}")), 10);
        }
        assert_eq!(dir_size(&d, &AtomicBool::new(true)), None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn measure_blocking_sorts_biggest_first() {
        let d = tmp("measure");
        file(&d.join("small/a"), 10_000);
        file(&d.join("big/b"), 400_000);
        file(&d.join("loose.bin"), 100_000);
        let e = measure_blocking(&d).unwrap();
        let names: Vec<_> = e.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["big", "loose.bin", "small"]);
        assert!(e.iter().all(|e| e.size.is_some()));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn walk_finds_large_files_when_spotlight_is_unavailable() {
        let d = tmp("large");
        file(&d.join("a/huge.bin"), 600_000);
        file(&d.join("a/tiny.bin"), 100);
        file(&d.join("huge2.bin"), 300_000);
        let mut found = Vec::new();
        walk_large(&d, 200_000, &AtomicBool::new(false), &mut found);
        found.sort_by(|a, b| b.1.cmp(&a.1));
        assert_eq!(found.len(), 2);
        assert!(found[0].0.ends_with("huge.bin"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn old_files_only_counts_stale_entries() {
        let d = tmp("old");
        file(&d.join("old.bin"), 300_000);
        file(&d.join("new.bin"), 300_000);
        let t = SystemTime::now() - Duration::from_secs(200 * 86_400);
        fs::File::options().write(true).open(d.join("old.bin")).unwrap().set_modified(t).unwrap();
        let sz = old_files_size(&d, 90);
        assert!(sz >= 300_000 && sz < 500_000, "{sz}");
        let _ = fs::remove_dir_all(&d);
    }
}
