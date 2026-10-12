//! Orphaned app data: caches, logs and saved state left behind by apps that are
//! no longer installed. Ported from Mole's `clean_orphaned_app_data`,
//! `scan_installed_apps` and `is_bundle_orphaned`.
//!
//! The scan fails closed: if any installed app's identity cannot be read, no
//! orphan decisions are made at all, because an incomplete app list is exactly
//! what would turn a live app's data into an "orphan".

use super::engine::{Engine, Guard};
use super::safety::should_protect_data;
use glob::{MatchOptions, Pattern};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// Orphan retention in days (Mole: MOLE_ORPHAN_AGE_DAYS).
pub const ORPHAN_AGE_DAYS: u64 = 30;
/// Max matches examined per glob (Mole: MOLE_MAX_ORPHAN_ITERATIONS).
const MAX_PER_PATTERN: usize = 100;
/// Spotlight probe budget (Mole: MOLE_TIMEOUT_MEDIUM_PROBE_SEC).
const MDFIND_TIMEOUT: Duration = Duration::from_secs(5);

/// (base dir under $HOME, label, name globs). Never add LaunchAgents/Daemons
/// (breaks login items), Containers (stubs are expected), Application Scripts or
/// Group Containers (TeamID.BundleID names cause false positives).
const RESOURCE_TYPES: &[(&str, &str, &[&str])] = &[
    ("Library/Caches", "Caches", &["com.*", "org.*", "net.*", "io.*"]),
    ("Library/Logs", "Logs", &["com.*", "org.*", "net.*", "io.*"]),
    ("Library/Saved Application State", "States", &["*.savedState"]),
];

/// Password managers, keys and credentials: never treated as orphan data.
const NEVER_DELETE: &[&str] = &[
    "*1password*", "*keychain*", "*bitwarden*", "*lastpass*", "*keepass*", "*dashlane*", "*enpass*",
    "*ssh*", "*gpg*", "*gnupg*", "com.apple.keychain*",
];

const SYSTEM_COMPONENTS: &[&str] =
    &["loginwindow", "dock", "systempreferences", "systemsettings", "settings", "controlcenter", "finder", "safari"];

pub fn is_reverse_dns(id: &str) -> bool {
    if id.is_empty() || id == "unknown" {
        return false;
    }
    let parts: Vec<&str> = id.split('.').collect();
    parts.len() >= 2
        && parts.iter().all(|p| {
            let mut c = p.chars();
            c.next().is_some_and(|f| f.is_ascii_alphanumeric()) && c.all(|x| x.is_ascii_alphanumeric() || x == '-')
        })
}

/// Core decision. `lookup` answers "does Spotlight know an app with this bundle
/// id?" with `None` meaning the probe timed out or failed (then the app is kept).
pub fn is_orphan(
    id: &str,
    age_days: Option<u64>,
    installed: &HashSet<String>,
    lookup: &mut dyn FnMut(&str) -> Option<bool>,
) -> bool {
    if should_protect_data(id) {
        return false;
    }
    let lower = id.to_lowercase();
    if NEVER_DELETE.iter().any(|p| Pattern::new(p).is_ok_and(|p| p.matches(&lower))) {
        return false;
    }
    if installed.contains(id) || SYSTEM_COMPONENTS.contains(&id) {
        return false;
    }
    if age_days.is_some_and(|d| d < ORPHAN_AGE_DAYS) {
        return false;
    }
    if is_reverse_dns(id) {
        // Catches apps installed outside the standard folders.
        return !matches!(lookup(id), Some(true) | None);
    }
    true
}

/// Identities of everything installed or running. `Err` names the bundle that
/// could not be read, in which case orphan detection must be skipped.
pub fn installed_bundles(home: &Path, running: &super::running::Running) -> Result<HashSet<String>, String> {
    let app_dirs = [
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        home.join("Applications"),
        PathBuf::from("/opt/homebrew/Caskroom"),
        PathBuf::from("/usr/local/Caskroom"),
        home.join("Library/Application Support/Setapp/Applications"),
    ];
    let mut ids = HashSet::new();
    for dir in app_dirs.iter().filter(|d| d.is_dir()) {
        let mut apps = Vec::new();
        find_apps(dir, 1, 3, &mut apps).map_err(|e| format!("{}: {e}", dir.display()))?;
        for app in apps {
            if app.to_string_lossy().contains("/Wrapper/") {
                continue;
            }
            if let Some(id) = read_identity(&app)? {
                ids.insert(id);
            }
        }
    }
    ids.extend(running.bundle_ids.iter().cloned());

    // LaunchAgent labels (by file name) guard against false orphans for helpers.
    for dir in [home.join("Library/LaunchAgents"), PathBuf::from("/Library/LaunchAgents")] {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "plist")
                    && let Some(stem) = p.file_stem() {
                        ids.insert(stem.to_string_lossy().to_string());
                    }
            }
        }
    }
    Ok(ids)
}

/// `find DIR -maxdepth 3 -type d -name '*.app'`, without descending into bundles.
fn find_apps(dir: &Path, depth: usize, max: usize, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)?.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let p = e.path();
        if p.extension().is_some_and(|x| x == "app") {
            out.push(p);
        } else if depth < max {
            // A subfolder we cannot read is not an error for the whole scan.
            let _ = find_apps(&p, depth + 1, max, out);
        }
    }
    Ok(())
}

/// `Ok(Some(id))`: identified. `Ok(None)`: bundle legitimately carries no id.
/// `Err`: unreadable, which must fail the whole scan closed.
fn read_identity(app: &Path) -> Result<Option<String>, String> {
    let mut plist = app.join("Contents/Info.plist");
    if !plist.is_file() {
        // iOS/iPadOS apps on Apple Silicon keep the plist under Wrapper/<name>.app/.
        let wrapped = std::fs::read_dir(app.join("Wrapper"))
            .ok()
            .and_then(|rd| rd.flatten().map(|e| e.path().join("Info.plist")).find(|p| p.is_file()));
        match wrapped {
            Some(w) => plist = w,
            None => {
                // A dangling WrappedBundle link means the payload is gone: no identity
                // can exist, so skipping it invents no orphan.
                let link = app.join("WrappedBundle");
                if link.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) && !link.exists() {
                    return Ok(None);
                }
                return Err(app.file_name().unwrap_or_default().to_string_lossy().to_string());
            }
        }
    }
    match plist::Value::from_file(&plist) {
        Ok(v) => Ok(v
            .as_dictionary()
            .and_then(|d| d.get("CFBundleIdentifier"))
            .and_then(|v| v.as_string())
            .filter(|s| !s.is_empty() && *s != "missing value")
            .map(String::from)),
        Err(_) => Err(app.file_name().unwrap_or_default().to_string_lossy().to_string()),
    }
}

/// Ask Spotlight whether an app with this bundle id exists. `None` = timed out
/// or failed (callers keep the app rather than guess).
fn mdfind_has(id: &str) -> Option<bool> {
    // `id` has already passed is_reverse_dns, so it cannot carry quotes or operators.
    let mut child = Command::new("/usr/bin/mdfind")
        .arg(format!("kMDItemCFBundleIdentifier == '{id}'"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                use std::io::Read;
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(!out.trim().is_empty());
            }
            Ok(None) if start.elapsed() > MDFIND_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
}

fn age_in_days(path: &Path) -> Option<u64> {
    let modified = std::fs::symlink_metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok().map(|d| d.as_secs() / 86_400)
}

/// Add orphaned app data to the scan. `Err(reason)` means the section was skipped.
pub fn scan(engine: &mut Engine) -> Result<(), String> {
    let installed = installed_bundles(&engine.home, &engine.running)
        .map_err(|bad| format!("Unable to scan installed applications ({bad})"))?;
    let mut cache: HashMap<String, Option<bool>> = HashMap::new();
    let opts = MatchOptions { require_literal_leading_dot: true, ..Default::default() };

    for (base, label, patterns) in RESOURCE_TYPES {
        let base_dir = engine.home.join(base);
        if std::fs::read_dir(&base_dir).is_err() {
            continue;
        }
        for pat in *patterns {
            let full = format!("{}/{}", Pattern::escape(&base_dir.to_string_lossy()), pat);
            let Ok(paths) = glob::glob_with(&full, opts) else { continue };
            for path in paths.flatten().filter(|p| p.symlink_metadata().is_ok()).take(MAX_PER_PATTERN) {
                let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                let id = name
                    .trim_end_matches(".savedState")
                    .trim_end_matches(".binarycookies")
                    .trim_end_matches(".plist")
                    .to_string();
                let age = age_in_days(&path);
                let orphan = is_orphan(&id, age, &installed, &mut |bid| {
                    *cache.entry(bid.to_string()).or_insert_with(|| mdfind_has(bid))
                });
                if orphan {
                    engine.consider(&format!("Orphaned {label}: {id}"), &path, Guard::None);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }
    fn never(_: &str) -> Option<bool> {
        Some(false)
    }

    #[test]
    fn reverse_dns_detection() {
        assert!(is_reverse_dns("com.example.app"));
        assert!(is_reverse_dns("io.github.some-app"));
        assert!(!is_reverse_dns("Slack"));
        assert!(!is_reverse_dns("unknown"));
        assert!(!is_reverse_dns("com..bad"));
        assert!(!is_reverse_dns("com.ex ample.app"));
        assert!(!is_reverse_dns("com.ex'ample.app"));
    }

    #[test]
    fn uninstalled_old_app_is_orphan() {
        assert!(is_orphan("com.gone.app", Some(90), &installed(&[]), &mut never));
    }

    #[test]
    fn installed_app_is_never_orphan() {
        assert!(!is_orphan("com.here.app", Some(90), &installed(&["com.here.app"]), &mut never));
    }

    #[test]
    fn recently_modified_data_is_kept() {
        assert!(!is_orphan("com.gone.app", Some(5), &installed(&[]), &mut never));
        assert!(!is_orphan("com.gone.app", Some(29), &installed(&[]), &mut never));
        assert!(is_orphan("com.gone.app", Some(30), &installed(&[]), &mut never));
    }

    #[test]
    fn spotlight_hit_or_failure_keeps_the_data() {
        assert!(!is_orphan("com.moved.app", Some(90), &installed(&[]), &mut |_| Some(true)));
        // timeout/error must not be treated as "not installed"
        assert!(!is_orphan("com.slow.app", Some(90), &installed(&[]), &mut |_| None));
    }

    #[test]
    fn protected_and_credential_data_is_never_orphan() {
        let inst = installed(&[]);
        assert!(!is_orphan("com.apple.anything", Some(900), &inst, &mut never));
        assert!(!is_orphan("com.agilebits.onepassword", Some(900), &inst, &mut never));
        assert!(!is_orphan("org.example.sshtool", Some(900), &inst, &mut never));
        assert!(!is_orphan("com.vendor.MyKeychainHelper", Some(900), &inst, &mut never));
        assert!(!is_orphan("finder", Some(900), &inst, &mut never));
    }

    #[test]
    fn missing_age_does_not_block_but_lookup_still_runs() {
        assert!(!is_orphan("com.x.y", None, &installed(&[]), &mut |_| Some(true)));
    }

    #[test]
    fn find_apps_respects_depth_and_does_not_enter_bundles() {
        let root = std::env::temp_dir().join(format!("sweepr-apps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for p in ["A.app/Contents", "Util/B.app/Contents", "Util/Deep/C.app/Contents", "A.app/Contents/Frameworks/H.app/Contents", "x/y/z/D.app/Contents"] {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }
        let mut out = Vec::new();
        find_apps(&root, 1, 3, &mut out).unwrap();
        let mut names: Vec<_> = out.iter().map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["A.app", "Util/B.app", "Util/Deep/C.app"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_plist_fails_the_scan_closed() {
        let root = std::env::temp_dir().join(format!("sweepr-plist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let good = root.join("Good.app/Contents");
        let bad = root.join("Bad.app/Contents");
        let none = root.join("NoId.app/Contents");
        for d in [&good, &bad, &none] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(good.join("Info.plist"), r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.good.app</string></dict></plist>"#).unwrap();
        std::fs::write(bad.join("Info.plist"), "not a plist {{{").unwrap();
        std::fs::write(none.join("Info.plist"), r#"<?xml version="1.0"?><plist version="1.0"><dict><key>Other</key><string>x</string></dict></plist>"#).unwrap();
        assert_eq!(read_identity(&root.join("Good.app")), Ok(Some("com.good.app".into())));
        assert_eq!(read_identity(&root.join("NoId.app")), Ok(None));
        assert!(read_identity(&root.join("Bad.app")).is_err());
        std::fs::remove_file(root.join("Good.app/Contents/Info.plist")).unwrap();
        assert!(read_identity(&root.join("Good.app")).is_err(), "missing plist must fail closed");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// End to end: real filesystem, real installed-app scan, real Spotlight probe.
    #[test]
    fn scan_flags_a_true_orphan_and_spares_controls() {
        use crate::clean::{engine::Engine, running::Running, whitelist::Whitelist};
        use std::fs;
        let home = std::env::temp_dir().join(format!("sweepr-orphan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let old = SystemTime::now() - Duration::from_secs(60 * 86_400);
        let mk = |rel: &str, age: Option<SystemTime>| {
            let d = home.join(rel);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("blob"), vec![b'x'; 40_000]).unwrap();
            if let Some(t) = age {
                fs::File::open(&d).unwrap().set_modified(t).unwrap();
            }
        };
        mk("Library/Caches/com.zzz-nonexistent.gone", Some(old)); // orphan
        mk("Library/Caches/com.zzz-nonexistent.recent", None); // too new
        mk("Library/Caches/com.apple.zzz", Some(old)); // system-protected
        mk("Library/Caches/com.zzz-keychain.helper", Some(old)); // credential-ish name
        mk("Library/Logs/com.zzz-nonexistent.gone", Some(old)); // orphan log
        let wl = Whitelist::load(&home);
        let mut e = Engine::new(home.clone(), wl, Running::default());
        scan(&mut e).expect("scan should succeed on a normal machine");
        let mut got: Vec<String> = e.scan.items.iter().map(|i| i.label.clone()).collect();
        got.sort();
        assert_eq!(
            got,
            vec!["Orphaned Caches: com.zzz-nonexistent.gone", "Orphaned Logs: com.zzz-nonexistent.gone"]
        );
        let _ = fs::remove_dir_all(&home);
    }
}
