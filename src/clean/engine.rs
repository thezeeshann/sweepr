//! Scan → review → delete engine.
//!
//! Every candidate passes the same funnel: path
//! validation, protection, whitelist, running-owner deferral, sizing. Nothing is
//! removed during the scan, so `--dry-run` and the real run see identical items.
//! Deletion re-validates each path and checks it is still the same filesystem
//! object that was sized.

use super::running::Running;
use super::safety::{normalize, validate_for_deletion};
use super::whitelist::Whitelist;
use glob::{MatchOptions, Pattern};
use std::collections::{BTreeSet, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
pub struct Item {
    pub path: PathBuf,
    /// Allocated bytes on disk.
    pub size: u64,
    pub label: String,
    pub section: String,
    dev: u64,
    ino: u64,
}

/// Extra condition under which a candidate must be left alone.
#[derive(Clone, Copy)]
pub enum Guard<'a> {
    None,
    /// Defer when the app that owns this Library folder is running.
    OwnerRunning,
    /// Defer while any of these processes is running (case-insensitive names).
    ProcessRunning(&'a [&'a str]),
}

#[derive(Default)]
pub struct Scan {
    pub items: Vec<Item>,
    /// Candidates skipped by protection rules or the whitelist.
    pub protected: usize,
    /// Candidates skipped because their owning app is running.
    pub deferred: usize,
    /// Candidates the safety gate refused outright (reason kept for --verbose).
    pub refused: Vec<(String, String)>,
}

pub struct Engine {
    pub home: PathBuf,
    pub whitelist: Whitelist,
    pub running: Running,
    pub scan: Scan,
    pub section: String,
    /// Human-readable notes for skipped or degraded sections.
    pub notes: Vec<String>,
    seen: BTreeSet<String>,
}

pub struct Outcome {
    pub removed: usize,
    pub freed: u64,
    pub failed: Vec<(PathBuf, String)>,
}

impl Engine {
    pub fn new(home: PathBuf, whitelist: Whitelist, running: Running) -> Self {
        Self { home, whitelist, running, scan: Scan::default(), section: String::new(), notes: Vec::new(), seen: BTreeSet::new() }
    }

    /// Run one candidate through the funnel. Returns the bytes it adds to the scan.
    pub fn consider(&mut self, label: &str, path: &Path, guard: Guard) -> u64 {
        let p = normalize(&path.to_string_lossy());

        // Dedupe: skip anything already covered by (or covering) a recorded item.
        if self.seen.contains(&p) || self.ancestor_seen(&p) || self.descendant_seen(&p) {
            return 0;
        }
        if let Err(reason) = validate_for_deletion(&p, &self.home) {
            // Protection rules are expected and routine; only surprises are kept.
            if reason.starts_with("protected path") {
                self.scan.protected += 1;
            } else {
                self.scan.refused.push((p, reason));
            }
            return 0;
        }
        if self.whitelist.is_protected(&p) {
            self.scan.protected += 1;
            return 0;
        }
        let deferred = match guard {
            Guard::None => false,
            Guard::OwnerRunning => self.running.owner_running(&p),
            Guard::ProcessRunning(names) => names.iter().any(|n| self.running.process_running(n)),
        };
        if deferred {
            self.scan.deferred += 1;
            return 0;
        }
        let Some((size, dev, ino)) = disk_usage(Path::new(&p)) else {
            return 0;
        };
        if size == 0 {
            return 0;
        }
        self.seen.insert(p.clone());
        self.scan.items.push(Item {
            path: PathBuf::from(p),
            size,
            label: label.to_string(),
            section: self.section.clone(),
            dev,
            ino,
        });
        size
    }

    fn ancestor_seen(&self, p: &str) -> bool {
        let mut cur = p;
        while let Some(i) = cur.rfind('/') {
            cur = &cur[..i];
            if !cur.is_empty() && self.seen.contains(cur) {
                return true;
            }
        }
        false
    }

    fn descendant_seen(&self, p: &str) -> bool {
        let prefix = format!("{p}/");
        self.seen.range(prefix.clone()..).next().is_some_and(|s| s.starts_with(&prefix))
    }

    /// `~/Library/Caches/*`-style glob. Like bash, `*` does not match dotfiles.
    pub fn glob(&mut self, label: &str, pattern: &str, guard: Guard) -> u64 {
        // $HOME may contain glob metacharacters; only the rest of the pattern is a glob.
        let pat = match pattern.strip_prefix("~/") {
            Some(rest) => format!("{}/{}", Pattern::escape(&self.home.to_string_lossy()), rest),
            None => pattern.to_string(),
        };
        let opts = MatchOptions { require_literal_leading_dot: true, ..Default::default() };
        let Ok(paths) = glob::glob_with(&pat, opts) else {
            return 0;
        };
        let mut total = 0;
        for entry in paths.flatten() {
            total += self.consider(label, &entry, guard);
        }
        total
    }

    /// Every child of `dir`, dotfiles included (e.g. the Trash).
    pub fn children(&mut self, label: &str, dir: &Path, guard: Guard) -> u64 {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut kids: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        kids.sort();
        kids.into_iter().map(|k| self.consider(label, &k, guard)).sum()
    }

    /// Regular files under `dir` (to `max_depth`) matching `name_glob` and not
    /// modified for `age_days`.
    pub fn aged_files(&mut self, label: &str, dir: &Path, name_glob: &str, age_days: u64, max_depth: usize) -> u64 {
        let Ok(name) = Pattern::new(name_glob) else {
            return 0;
        };
        let cutoff = SystemTime::now().checked_sub(Duration::from_secs(age_days * 86_400));
        let Some(cutoff) = cutoff else { return 0 };
        let mut stack = vec![(dir.to_path_buf(), 1usize)];
        let mut found = Vec::new();
        while let Some((d, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                let path = e.path();
                if ft.is_dir() && depth < max_depth {
                    stack.push((path, depth + 1));
                } else if ft.is_file() {
                    let fname = e.file_name().to_string_lossy().to_string();
                    let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < cutoff);
                    if old && name.matches(&fname) {
                        found.push(path);
                    }
                }
            }
        }
        found.sort();
        found.into_iter().map(|f| self.consider(label, &f, Guard::None)).sum()
    }

    /// Delete everything in `items`. Each path is re-validated and must still be
    /// the object that was sized; anything that changed is left alone.
    pub fn execute(&self, items: &[Item]) -> Outcome {
        let mut out = Outcome { removed: 0, freed: 0, failed: Vec::new() };
        for it in items {
            let p = it.path.to_string_lossy().to_string();
            if let Err(e) = validate_for_deletion(&p, &self.home) {
                out.failed.push((it.path.clone(), format!("no longer safe to delete ({e})")));
                continue;
            }
            if self.whitelist.is_protected(&p) {
                continue;
            }
            match std::fs::symlink_metadata(&it.path) {
                // Already gone (e.g. removed by the app meanwhile): nothing to do.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    out.failed.push((it.path.clone(), e.to_string()));
                    continue;
                }
                Ok(m) if m.dev() != it.dev || m.ino() != it.ino => {
                    out.failed.push((it.path.clone(), "changed since it was scanned; skipped".into()));
                    continue;
                }
                Ok(_) => {}
            }
            match remove(&it.path) {
                Ok(()) => {
                    out.removed += 1;
                    out.freed += it.size;
                }
                Err(e) => out.failed.push((it.path.clone(), explain(&e))),
            }
        }
        out
    }
}

/// Remove a file, symlink (never its target) or directory tree.
fn remove(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_dir() {
        // remove_dir_all does not follow symlinks inside the tree.
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

fn explain(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            "permission denied (grant Full Disk Access to your terminal in System Settings)".into()
        }
        _ => e.to_string(),
    }
}

/// Allocated size of a path without following symlinks. Hard links are counted
/// once. Returns (bytes, dev, ino) of the top-level object.
pub fn disk_usage(path: &Path) -> Option<(u64, u64, u64)> {
    let top = std::fs::symlink_metadata(path).ok()?;
    let (dev, ino) = (top.dev(), top.ino());
    let mut total = 0u64;
    let mut links: HashSet<(u64, u64)> = HashSet::new();
    let mut stack = vec![path.to_path_buf()];
    let mut first = true;
    while let Some(p) = stack.pop() {
        let m = if first { first = false; top.clone() } else { match std::fs::symlink_metadata(&p) { Ok(m) => m, Err(_) => continue } };
        if m.file_type().is_dir() {
            total += m.blocks() * 512;
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        } else if m.nlink() > 1 && !links.insert((m.dev(), m.ino())) {
            continue;
        } else {
            total += m.blocks() * 512;
        }
    }
    Some((total, dev, ino))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// Build an engine rooted in a throwaway "home" directory.
    fn rig(name: &str) -> (Engine, PathBuf) {
        let home = std::env::temp_dir().join(format!("sweepr-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let wl = Whitelist::load(&home);
        (Engine::new(home.clone(), wl, Running::default()), home)
    }

    fn write(p: &Path, bytes: usize) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn scan_then_delete_only_targets() {
        let (mut e, home) = rig("basic");
        write(&home.join("Library/Caches/com.example.app/blob"), 50_000);
        write(&home.join("Documents/keep.txt"), 50_000);
        let n = e.glob("User app cache", "~/Library/Caches/*", Guard::None);
        assert!(n >= 50_000);
        assert_eq!(e.scan.items.len(), 1);
        let out = e.execute(&e.scan.items.clone());
        assert_eq!(out.removed, 1);
        assert!(!home.join("Library/Caches/com.example.app").exists());
        assert!(home.join("Documents/keep.txt").exists(), "must not touch unrelated files");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn dry_scan_deletes_nothing() {
        let (mut e, home) = rig("dry");
        write(&home.join("Library/Caches/com.example.app/blob"), 10_000);
        e.glob("c", "~/Library/Caches/*", Guard::None);
        assert!(home.join("Library/Caches/com.example.app/blob").exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn symlink_is_unlinked_but_target_survives() {
        let (mut e, home) = rig("symlink");
        write(&home.join("Documents/precious.txt"), 20_000);
        fs::create_dir_all(home.join("Library/Caches")).unwrap();
        symlink(home.join("Documents"), home.join("Library/Caches/sneaky")).unwrap();
        e.glob("c", "~/Library/Caches/*", Guard::None);
        // A symlink has ~0 allocated blocks, so it may legitimately not be listed;
        // either way the target must survive a full scan + delete.
        let items = e.scan.items.clone();
        e.execute(&items);
        assert!(home.join("Documents/precious.txt").exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn symlinked_cache_root_cannot_redirect_into_documents() {
        let (mut e, home) = rig("ancestor");
        write(&home.join("Documents/precious.txt"), 20_000);
        fs::create_dir_all(home.join("Library")).unwrap();
        // ~/Library/Caches -> ~/Documents: a naive sweep would wipe Documents.
        symlink(home.join("Documents"), home.join("Library/Caches")).unwrap();
        e.glob("c", "~/Library/Caches/*", Guard::None);
        let items = e.scan.items.clone();
        let out = e.execute(&items);
        assert!(home.join("Documents/precious.txt").exists(), "ancestor symlink escaped the guard");
        assert_eq!(out.removed, 0);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn whitelist_and_protection_block_candidates() {
        let (mut e, home) = rig("protect");
        write(&home.join("Library/Caches/CloudKit/db"), 30_000); // safety whitelist
        write(&home.join("Library/Caches/com.1password.x/db"), 30_000); // protected app
        write(&home.join("Library/Caches/com.example.junk/db"), 30_000);
        e.glob("c", "~/Library/Caches/*", Guard::None);
        let names: Vec<_> = e.scan.items.iter().map(|i| i.path.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["com.example.junk"]);
        assert_eq!(e.scan.protected, 2);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn item_swapped_after_scan_is_not_deleted() {
        let (mut e, home) = rig("swap");
        let dir = home.join("Library/Caches/com.example.app");
        write(&dir.join("blob"), 30_000);
        e.glob("c", "~/Library/Caches/*", Guard::None);
        let items = e.scan.items.clone();
        // Replace the scanned directory with a different object (new inode).
        fs::remove_dir_all(&dir).unwrap();
        write(&dir.join("other"), 30_000);
        let out = e.execute(&items);
        assert_eq!(out.removed, 0);
        assert!(dir.join("other").exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn aged_files_respects_age_and_pattern() {
        let (mut e, home) = rig("aged");
        let old = home.join("Library/Logs/old.log");
        let new = home.join("Library/Logs/new.log");
        let other = home.join("Library/Logs/old.txt");
        for p in [&old, &new, &other] {
            write(p, 10_000);
        }
        let ten_days = SystemTime::now() - Duration::from_secs(10 * 86_400);
        for p in [&old, &other] {
            fs::File::options().write(true).open(p).unwrap().set_modified(ten_days).unwrap();
        }
        e.aged_files("logs", &home.join("Library/Logs"), "*.log", 7, 3);
        let names: Vec<_> = e.scan.items.iter().map(|i| i.path.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["old.log"]);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn nested_candidates_are_not_double_counted() {
        let (mut e, home) = rig("dedupe");
        write(&home.join("Library/Caches/com.example.app/Sub/blob"), 30_000);
        let a = e.glob("sweep", "~/Library/Caches/*", Guard::None);
        let b = e.glob("specific", "~/Library/Caches/com.example.app/Sub", Guard::None);
        assert!(a > 0);
        assert_eq!(b, 0);
        assert_eq!(e.scan.items.len(), 1);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn disk_usage_counts_hardlinks_once() {
        let (_, home) = rig("hardlink");
        write(&home.join("d/a"), 100_000);
        fs::hard_link(home.join("d/a"), home.join("d/b")).unwrap();
        let (both, _, _) = disk_usage(&home.join("d")).unwrap();
        let (one, _, _) = disk_usage(&home.join("d/a")).unwrap();
        assert!(both < 2 * one, "hard link double-counted: dir={both} file={one}");
        let _ = fs::remove_dir_all(&home);
    }
}
