//! Deletion safety: the gate every candidate path passes before it is sized or
//! removed.
//!
//! Only user-level locations are cleaned, so every target must resolve to a
//! path inside `$HOME`.

use glob::Pattern;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// Pattern lists, kept as data files next to this module.
const PROTECTED_NAMES: &str = include_str!("protected_names.txt");
const DATA_PROTECTED: &str = include_str!("data_protected.txt");
const SYSTEM_CRITICAL: &str = include_str!("system_critical.txt");
const ENDPOINT_PREFIXES: &str = include_str!("endpoint_prefixes.txt");

fn compile(text: &str) -> Vec<Pattern> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| Pattern::new(l).unwrap_or_else(|e| panic!("bad protection pattern {l:?}: {e}")))
        .collect()
}

fn names() -> &'static [Pattern] {
    static P: OnceLock<Vec<Pattern>> = OnceLock::new();
    P.get_or_init(|| compile(PROTECTED_NAMES))
}
fn data_protected() -> &'static [Pattern] {
    static P: OnceLock<Vec<Pattern>> = OnceLock::new();
    P.get_or_init(|| compile(DATA_PROTECTED))
}
fn system_critical() -> &'static [Pattern] {
    static P: OnceLock<Vec<Pattern>> = OnceLock::new();
    P.get_or_init(|| compile(SYSTEM_CRITICAL))
}

/// Core macOS components that must never be uninstalled (Finder, Safari, Settings...).
pub fn is_system_critical_bundle(id: &str) -> bool {
    system_critical().iter().any(|p| p.matches(id))
}

/// Bundle-id / filename protection: system components,
/// password managers, IDEs, VPN/proxy tools and so on.
pub fn should_protect_data(name: &str) -> bool {
    names().iter().any(|p| p.matches(name)) || data_protected().iter().any(|p| p.matches(name))
}

/// Path-glob protections. `*` also matches `/`, which is `glob`'s default too.
const PROTECTED_PATH_GLOBS: &[&str] = &[
    // system UI components
    "*[Ss]ystem[Ss]ettings*",
    "*[Ss]ystem[Pp]references*",
    "*[Cc]ontrol[Cc]enter*",
    "*com.apple.[Ss]ettings*",
    "*com.apple.[Nn]otes*",
    "*com.apple.systempreferences.cache*",
    "*com.apple.finder*",
    "*com.apple.dock*",
    "*/Library/Containers/com.apple.Settings*",
    "*/Library/Containers/com.apple.SystemSettings*",
    "*/Library/Containers/com.apple.controlcenter*",
    "*/Library/Group Containers/com.apple.systempreferences*",
    "*/Library/Group Containers/com.apple.Settings*",
    "*/com.apple.sharedfilelist/*com.apple.Settings*",
    "*/com.apple.sharedfilelist/*com.apple.SystemSettings*",
    "*/com.apple.sharedfilelist/*systempreferences*",
    // OrbStack holds live container filesystem images
    "*/Library/Group Containers/*dev.orbstack",
    "*/Library/Group Containers/*dev.orbstack/*",
    "*/.orbstack",
    "*/.orbstack/*",
    // preferences and radio configuration
    "*/Library/Preferences/com.apple.dock.plist",
    "*/Library/Preferences/com.apple.finder.plist",
    "*/ByHost/com.apple.bluetooth.*",
    "*/ByHost/com.apple.wifi.*",
    "*/Library/Preferences/com.apple.networkextension*.plist",
    // Other tools' logs and AI-assistant state that live in cache-shaped paths
    "*/Library/Logs/mole",
    "*/Library/Logs/mole/*",
    "*/Library/Application Support/Codex",
    "*/Library/Application Support/Codex/*",
    "*/Library/Logs/com.openai.codex",
    "*/Library/Logs/com.openai.codex/*",
    "*/.codex/sessions",
    "*/.codex/sessions/*",
    "*/.codex/auth.json",
    "*/.codex/history.jsonl",
    "*/.codex/state_*.sqlite",
    "*/.codex/logs_*.sqlite",
    "*/.codex/session_index.jsonl",
    "*/.codex/cache/session_index.jsonl",
    "*/.codex/cache/codex_app_directory",
    "*/.codex/cache/codex_app_directory/*",
    // iCloud Drive
    "*/Library/Mobile Documents*",
    "*/Mobile Documents*",
    // accounts, keychains, mail and contacts
    "*/Library/Accounts",
    "*/Library/Accounts/*",
    "*/Library/Keychains",
    "*/Library/Keychains/*",
    "*/Library/Mail",
    "*/Library/Mail/*",
    "*/Library/Calendars",
    "*/Library/Contacts",
    "*/Library/Contacts/*",
    // audio plugins and license state
    "/Library/Audio/Plug-Ins/*",
    "*/Library/Application Support/iZotope",
    "*/Library/Application Support/iZotope/*",
    "*/Library/Preferences/com.native-instruments*",
    "*/Library/Preferences/com.avid.mediacomposer*.plist",
    "*/Library/Preferences/com.paceap.*.plist",
    // cache-shaped paths that hold license, account or service state
    "*/Library/Caches/ms-playwright",
    "*/Library/Caches/ms-playwright/*",
    "*/Library/Caches/app.cotypist.Cotypist*",
    "*/Library/Caches/com.displaylink.DisplayLinkUserAgent*",
    "*/Library/Caches/com.lasersoft-imaging.*",
    "*/Library/Caches/Adobe *",
    "*/Library/Caches/* Adobe*",
    "*/Library/Caches/com.apple.containermanagerd*",
    "*/Library/Caches/com.apple.homed*",
    "*/Library/Caches/com.apple.ap.adprivacyd*",
    "*/Library/Caches/FamilyCircle*",
    "*/Library/Caches/com.apple.HomeKit*",
    "*/Library/Caches/com.apple.WorkflowKit.BackgroundShortcutRunner.ShortcutsSandboxCache*",
    "*/Library/Caches/com.apple.siriactionsd.ShortcutsSandboxCache*",
    // wallpaper / aerial assets are user-selected content
    "*/Library/Application Support/com.apple.idleassetsd",
    "*/Library/Application Support/com.apple.idleassetsd/*",
    "*/Library/Application Support/com.apple.wallpaper",
    "*/Library/Application Support/com.apple.wallpaper/*",
    // cleaning audio subsystem caches can drop output on some Macs
    "*com.apple.coreaudio*",
    "*com.apple.audio.*",
    "*coreaudiod*",
];

fn protected_path_globs() -> &'static [Pattern] {
    static P: OnceLock<Vec<Pattern>> = OnceLock::new();
    P.get_or_init(|| {
        PROTECTED_PATH_GLOBS
            .iter()
            .map(|g| Pattern::new(g).unwrap_or_else(|e| panic!("bad path glob {g:?}: {e}")))
            .collect()
    })
}

/// Is the path under a macOS security/EDR agent's cache folder? Deleting inside
/// a sensor's container trips tamper detection.
fn is_endpoint_security_cache(path: &str) -> bool {
    if !(path.starts_with("/private/var/folders/") || path.starts_with("/var/folders/")) {
        return false;
    }
    let lower = path.to_lowercase();
    ENDPOINT_PREFIXES
        .lines()
        .filter(|l| !l.is_empty())
        .any(|p| lower.contains(&p.to_lowercase()))
}

/// How strict the protection rules are.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    /// Routine cleaning: apps' own data (password managers, IDEs, VPNs...) is protected.
    Cleanup,
    /// The user chose to remove one app, so its own data may go. System-critical
    /// components and the fixed path rules still apply.
    Uninstall,
}

/// Protection check for routine cleaning.
pub fn should_protect_path(path: &str) -> bool {
    should_protect_path_in(path, Mode::Cleanup)
}

pub fn should_protect_path_in(path: &str, mode: Mode) -> bool {
    if path.is_empty() {
        return false;
    }
    if protected_path_globs().iter().any(|p| p.matches(path)) {
        return true;
    }
    if is_endpoint_security_cache(path) {
        return true;
    }

    // Sandbox containers: protect by owning bundle id, except for the cache/tmp
    // directories inside them, which are regenerable by definition.
    let mut container_cache = false;
    for marker in ["/Library/Containers/", "/Library/Group Containers/"] {
        if let Some(i) = path.find(marker) {
            let rest = &path[i + marker.len()..];
            let bundle = rest.split('/').next().unwrap_or("");
            if path.contains("/Data/Library/Caches/") || path.contains("/Data/tmp/") {
                container_cache = true;
            } else if mode == Mode::Cleanup && should_protect_data(bundle) {
                return true;
            }
            break;
        }
    }

    // Codex's Chromium cache leaves are rebuildable; everything else it keeps is
    // protected above.
    let known_rebuildable = [
        "/Library/Caches/Codex/Default/Cache/",
        "/Library/Caches/Codex/Default/Code Cache/",
        "/Library/Caches/Codex/Default/Partitions/codex-browser-app/Cache/",
        "/Library/Caches/Codex/Default/Partitions/codex-browser-app/Code Cache/",
        "/Library/Caches/Codex/codex-browser-app/Cache/",
        "/Library/Caches/Codex/codex-browser-app/Code Cache/",
    ]
    .iter()
    .any(|p| path.contains(p));

    if !container_cache && !known_rebuildable {
        let data: &[Pattern] = if mode == Mode::Cleanup { data_protected() } else { &[] };
        if system_critical().iter().chain(data).any(|p| p.matches(path)) {
            return true;
        }
        // The filename check is cleanup-only: the user explicitly chose this app.
        let filename = path.rsplit('/').next().unwrap_or(path);
        if mode == Mode::Cleanup && should_protect_data(filename) {
            return true;
        }
    }
    false
}

/// Collapse `//` and `/./`, drop a trailing `/.` or `/`.
pub fn normalize(path: &str) -> String {
    let mut p = path.to_string();
    while p.contains("//") {
        p = p.replace("//", "/");
    }
    while p.contains("/./") {
        p = p.replace("/./", "/");
    }
    while p.ends_with("/.") {
        p.truncate(p.len() - 2);
        if p.is_empty() {
            p = "/".into();
        }
    }
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() { p } else { trimmed.to_string() }
}

fn under(p: &str, root: &str) -> bool {
    p == root || p.strip_prefix(root).is_some_and(|r| r.starts_with('/'))
}

/// System roots that must never be removed, whatever the caller thinks it is deleting.
pub fn is_critical_path(p: &str) -> bool {
    // User-installed software lives here; individual entries stay deletable but
    // the roots themselves are critical (fall through below).
    if p.starts_with("/usr/local/") || p.starts_with("/opt/homebrew/") {
        return false;
    }
    const TREES: &[&str] = &[
        "/bin", "/dev", "/sbin", "/usr", "/System", "/Library/Apple", "/Library/Extensions",
        "/Library/Keychains", "/Applications/Finder.app", "/Applications/Safari.app", "/etc",
        "/private/etc", "/var/db", "/var/audit", "/private/var/db", "/private/var/audit",
        "/Users/Guest",
    ];
    const EXACT: &[&str] = &[
        "/", "/Library", "/Library/Application Support", "/Applications", "/Volumes", "/opt",
        "/opt/homebrew", "/Users", "/Users/Shared", "/private", "/private/tmp", "/var",
        "/var/root", "/private/var", "/private/var/tmp", "/private/var/folders",
        "/private/var/root", "/Network", "/cores", "/home", "/net", "/tmp",
    ];
    if EXACT.contains(&p) || TREES.iter().any(|t| under(p, t)) {
        return true;
    }
    // A user home root (/Users/<name>) itself, while its children stay deletable.
    // Catches the empty-variable collapse "/Users/$u/$leaf" -> "/Users/<name>".
    if let Some(rest) = p.strip_prefix("/Users/")
        && !rest.is_empty() && !rest.contains('/') {
            return true;
        }
    // APFS is case-insensitive: /SYSTEM or /OPT/HOMEBREW are not symlinks, so the
    // string checks above miss them. Compare identity against the real roots.
    if EXACT.iter().any(|r| same_file(Path::new(p), Path::new(r))) {
        return true;
    }
    if TREES.iter().any(|t| within_existing(Path::new(p), Path::new(t))) {
        return true;
    }
    if let Some(parent) = Path::new(p).parent()
        && same_file(parent, Path::new("/Users")) {
            return true;
        }
    false
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Is `path` inside `root` once both are resolved on disk?
fn within_existing(path: &Path, root: &Path) -> bool {
    let (Ok(p), Ok(r)) = (path.canonicalize(), root.canonicalize()) else {
        return false;
    };
    p.starts_with(r)
}

/// The deletion gate for a user-level cleaner. `Ok(())` means
/// the path may be sized and deleted; `Err` carries the reason it was refused.
pub fn validate_for_deletion(path: &str, home: &Path) -> Result<(), String> {
    validate_in(path, home, Mode::Cleanup)
}

pub fn validate_in(path: &str, home: &Path, mode: Mode) -> Result<(), String> {
    if path.is_empty() {
        return Err("empty path".into());
    }
    if !path.starts_with('/') {
        return Err(format!("path must be absolute: {path}"));
    }
    if path.split('/').any(|c| c == "..") {
        return Err(format!("path traversal not allowed: {path}"));
    }
    if path.chars().any(|c| c.is_control()) {
        return Err("path contains control characters".into());
    }
    let policy = normalize(path);
    let home_s = home.to_string_lossy();
    let home_canon = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());

    // Leaf symlink: refuse when it points at a critical or protected location.
    // (The link itself is what gets removed, never the target.)
    if let Ok(meta) = std::fs::symlink_metadata(&policy)
        && meta.file_type().is_symlink()
            && let Ok(target) = std::fs::canonicalize(&policy) {
                let t = target.to_string_lossy().to_string();
                if is_critical_path(&t) {
                    return Err(format!("symlink points to protected system path: {t}"));
                }
            }

    // Ancestor-symlink guard. The checks match the LITERAL string, so a redirected
    // ~/Library/Caches would send a cache sweep into ~/Documents or a system tree.
    // Resolve the parent and re-run the deny predicates on the resolved leaf.
    let pol_path = Path::new(&policy);
    let leaf = pol_path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let resolved: PathBuf = match pol_path.parent().and_then(|p| p.canonicalize().ok()) {
        Some(parent) => parent.join(&leaf),
        None => pol_path.to_path_buf(),
    };
    let resolved_s = resolved.to_string_lossy().to_string();
    if resolved_s != policy {
        if is_critical_path(&resolved_s) {
            return Err(format!("resolves into a critical system path: {resolved_s}"));
        }
        if should_protect_path_in(&resolved_s, mode) {
            return Err(format!("resolves into a protected path: {resolved_s}"));
        }
    }

    // A guard that only re-runs the deny predicates on the resolved path is not
    // enough: ~/Library/Caches symlinked to ~/Documents resolves to a
    // location that is neither critical nor protected and the sweep would walk
    // in. Refuse any symlink among the ancestors below $HOME instead.
    let rel = pol_path
        .strip_prefix(home)
        .or_else(|_| pol_path.strip_prefix(&home_canon))
        .ok()
        .and_then(|r| r.parent());
    if let Some(parent_rel) = rel
        && let Ok(actual) = pol_path.parent().unwrap_or(pol_path).canonicalize() {
            let expected = home_canon.join(parent_rel);
            if actual != expected {
                return Err(format!("an ancestor is a symlink ({} -> {})", expected.display(), actual.display()));
            }
        }

    if is_critical_path(&policy) {
        return Err(format!("critical system path: {path}"));
    }
    if should_protect_path_in(&policy, mode) {
        return Err(format!("protected path: {policy}"));
    }

    // Only the user's home is ever cleaned: the literal AND the resolved
    // location must both be strictly below it, and never the home itself.
    let inside = |p: &Path, h: &Path| p.starts_with(h) && p != h;
    if !(inside(Path::new(&policy), Path::new(home_s.as_ref())) || inside(Path::new(&policy), &home_canon)) {
        return Err(format!("outside home directory: {policy}"));
    }
    if !inside(&resolved, &home_canon) {
        return Err(format!("resolves outside home directory: {resolved_s}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_pattern_files_compile_and_are_nonempty() {
        // compile() panics on a bad pattern; a count mismatch means a line was dropped.
        for (text, min) in [(PROTECTED_NAMES, 70), (DATA_PROTECTED, 300), (SYSTEM_CRITICAL, 100)] {
            let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
            assert!(lines >= min, "pattern file shrank: {lines}");
            assert_eq!(compile(text).len(), lines);
        }
        assert!(!protected_path_globs().is_empty());
    }

    #[test]
    fn critical_roots_are_refused() {
        for p in [
            "/", "/usr", "/usr/bin/ls", "/System/Library", "/Library", "/Applications", "/bin/sh",
            "/etc/hosts", "/private/etc/hosts", "/var/db/x", "/Users", "/Users/Shared",
            "/Users/someone", "/opt/homebrew", "/usr/local", "/private/var/folders",
            "/Library/Keychains/login.keychain-db", "/Applications/Safari.app/Contents",
        ] {
            assert!(is_critical_path(p), "{p} should be critical");
        }
    }

    #[test]
    fn user_children_are_not_critical() {
        for p in [
            "/Users/someone/Library/Caches/foo", "/opt/homebrew/Cellar/foo", "/usr/local/lib/foo",
            "/private/var/log/foo.log",
        ] {
            assert!(!is_critical_path(p), "{p} should not be critical");
        }
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize("/a//b/./c/"), "/a/b/c");
        assert_eq!(normalize("/a/b/."), "/a/b");
        assert_eq!(normalize("/"), "/");
    }

    #[test]
    fn rejects_malformed_and_escaping_paths() {
        let home = Path::new("/Users/someone");
        assert!(validate_for_deletion("", home).is_err());
        assert!(validate_for_deletion("relative/path", home).is_err());
        assert!(validate_for_deletion("/Users/someone/Library/../../x", home).is_err());
        assert!(validate_for_deletion("/Users/someone/a\nb", home).is_err());
        assert!(validate_for_deletion("/Users/someone", home).is_err()); // the home itself
        assert!(validate_for_deletion("/Users/other/Library/Caches/x", home).is_err());
        assert!(validate_for_deletion("/etc/hosts", home).is_err());
        // a name that merely contains ".." (e.g. Firefox's "name..files") is not traversal
        assert_eq!(validate_for_deletion("/Users/someone/x/name..files", home), Ok(()));
    }

    #[test]
    fn protects_known_sensitive_paths() {
        let h = "/Users/someone";
        for p in [
            "Library/Keychains/login.keychain-db",
            "Library/Mobile Documents/com~apple~CloudDocs",
            "Library/Caches/com.apple.finder",
            "Library/Caches/ms-playwright/chromium",
            "Library/Preferences/com.apple.dock.plist",
            "Library/Containers/com.apple.Settings.Extension/Data/foo",
            "Library/Caches/com.1password.1password",
            "Library/Caches/com.microsoft.VSCode",
        ] {
            assert!(should_protect_path(&format!("{h}/{p}")), "{p} should be protected");
        }
    }

    #[test]
    fn leaves_ordinary_caches_deletable() {
        let h = "/Users/someone";
        for p in [
            "Library/Caches/com.example.SomeApp",
            "Library/Caches/Homebrew/downloads/x",
            "Library/Containers/com.apple.AppStore/Data/Library/Caches/foo",
            ".cache/vite/deps",
        ] {
            assert!(!should_protect_path(&format!("{h}/{p}")), "{p} should be deletable");
        }
    }

    #[test]
    fn data_protection_by_bundle_id() {
        assert!(should_protect_data("com.apple.Safari"));
        assert!(should_protect_data("com.1password.7"));
        assert!(!should_protect_data("com.example.randomapp"));
    }

    #[test]
    fn uninstall_mode_releases_app_data_but_keeps_system_protection() {
        let h = "/Users/someone";
        let app_data = format!("{h}/Library/Application Support/com.1password.1password");
        assert!(should_protect_path_in(&app_data, Mode::Cleanup), "cleanup must keep password-manager data");
        assert!(!should_protect_path_in(&app_data, Mode::Uninstall), "uninstalling it must be able to remove its data");
        // Fixed path rules and system components never relax.
        for p in [
            format!("{h}/Library/Keychains/login.keychain-db"),
            format!("{h}/Library/Mobile Documents/x"),
            format!("{h}/Library/Preferences/com.apple.dock.plist"),
            format!("{h}/Library/Containers/com.apple.Settings.Extension/Data/foo"),
        ] {
            assert!(should_protect_path_in(&p, Mode::Uninstall), "{p} must stay protected when uninstalling");
        }
    }
}
