//! Finding installed apps and everything they left behind.
//!
//! Related files are found from the app's bundle id plus a handful of display-name
//! variants ("Maestro Studio" / "MaestroStudio" / "maestro-studio"). A bundle id
//! only counts when it is a well-formed reverse-DNS name, and directory scans
//! match on id *boundaries* ("com.x.App" matches "com.x.App.helper", never
//! "com.x.Application"), so one product's data cannot be mistaken for another's.

use crate::clean::safety::{Mode, is_system_critical_bundle, validate_in};
use crate::clean::{disk_usage, is_reverse_dns};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct App {
    pub name: String,
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    pub version: Option<String>,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub path: PathBuf,
    pub kind: &'static str,
    pub size: u64,
}

#[derive(Debug, Default)]
pub struct Related {
    pub files: Vec<Found>,
    /// Matches the safety checks refused (kept so `--verbose` can explain).
    pub refused: Vec<(PathBuf, String)>,
    /// LaunchAgent plists among `files`, to unload before removal.
    pub agents: Vec<PathBuf>,
    /// System-wide leftovers that need administrator rights; reported, never removed.
    pub system: Vec<PathBuf>,
}

fn app_roots(home: &Path) -> Vec<PathBuf> {
    vec![PathBuf::from("/Applications"), home.join("Applications")]
}

/// Installed, uninstallable apps (system components excluded), sorted by name.
pub fn installed_apps(home: &Path) -> Vec<App> {
    let mut paths = Vec::new();
    for root in app_roots(home).iter().filter(|r| r.is_dir()) {
        collect_apps(root, 1, 2, &mut paths);
    }
    let mut apps: Vec<App> = paths
        .into_iter()
        .filter_map(|p| {
            let (bundle_id, version) = read_plist(&p);
            if bundle_id.as_deref().is_some_and(is_system_critical_bundle) {
                return None;
            }
            let name = p.file_stem()?.to_string_lossy().to_string();
            // A link whose target is gone: leftover from an interrupted uninstall.
            let broken = p.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) && !p.exists();
            let version = if broken { Some("broken link".to_string()) } else { version };
            Some(App { name, path: p, bundle_id, version, size: 0 })
        })
        .collect();

    // Sizing every bundle is the slow part; spread it over a few threads.
    let chunk = apps.len().div_ceil(8).max(1);
    std::thread::scope(|s| {
        for part in apps.chunks_mut(chunk) {
            s.spawn(move || {
                for a in part {
                    a.size = disk_usage(&a.path).map(|u| u.0).unwrap_or(0);
                }
            });
        }
    });
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

fn collect_apps(dir: &Path, depth: usize, max: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if p.extension().is_some_and(|x| x == "app") && (ft.is_dir() || ft.is_symlink()) {
            out.push(p);
        } else if ft.is_dir() && depth < max {
            collect_apps(&p, depth + 1, max, out);
        }
    }
}

fn read_plist(app: &Path) -> (Option<String>, Option<String>) {
    let Ok(v) = plist::Value::from_file(app.join("Contents/Info.plist")) else {
        return (None, None);
    };
    let Some(d) = v.as_dictionary() else { return (None, None) };
    let get = |k: &str| d.get(k).and_then(|v| v.as_string()).filter(|s| !s.is_empty()).map(String::from);
    (get("CFBundleIdentifier"), get("CFBundleShortVersionString").or_else(|| get("CFBundleVersion")))
}

/// Homebrew cask token when the app is managed by Homebrew.
pub fn cask_token(app: &Path) -> Option<String> {
    let real = app.canonicalize().ok()?;
    let s = real.to_string_lossy();
    let i = s.find("/Caskroom/")?;
    s[i + "/Caskroom/".len()..].split('/').next().map(String::from)
}

/// Does `name` belong to bundle id `id` on an id boundary?
pub fn has_id_boundary(name: &str, id: &str) -> bool {
    is_reverse_dns(id)
        && (name == id
            || name.starts_with(&format!("{id}."))
            || name.ends_with(&format!(".{id}"))
            || name.contains(&format!(".{id}.")))
}

/// The same, but only at the start (`id` or `id.*`).
pub fn starts_with_id_boundary(name: &str, id: &str) -> bool {
    is_reverse_dns(id) && (name == id || name.starts_with(&format!("{id}.")))
}

/// "Zed Nightly" → "Zed"; unchanged when there is no channel suffix.
pub fn base_name(name: &str) -> String {
    const SUFFIXES: &[&str] = &[
        "Nightly", "Beta", "Alpha", "Dev", "Canary", "Preview", "Insider", "Edge", "Stable", "Release", "RC", "LTS",
        "Developer Edition", "Technology Preview",
    ];
    for suf in SUFFIXES {
        if let Some(head) = name.strip_suffix(suf).and_then(|h| h.strip_suffix(' '))
            && !head.trim().is_empty() {
                return head.trim_end().to_string();
            }
    }
    name.to_string()
}

/// Display-name spellings apps use for their folders.
pub fn name_variants(name: &str) -> Vec<String> {
    let mut v = vec![name.to_string()];
    if name.chars().count() > 3 && name.contains(' ') {
        v.push(name.replace(' ', ""));
        v.push(name.replace(' ', "_"));
        v.push(name.replace(' ', "-"));
    }
    let base = base_name(name);
    if base != name && base.chars().count() > 2 {
        v.push(base.clone());
        if base.contains(' ') {
            v.push(base.replace(' ', ""));
        }
    }
    let mut seen = HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
    v
}

fn lib(home: &Path, rel: &str) -> PathBuf {
    home.join("Library").join(rel)
}

fn children(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// Everything under `~/Library` (and a few siblings) that belongs to `app`.
pub fn related_files(app: &App, home: &Path) -> Related {
    let mut cand: Vec<(PathBuf, &'static str)> = Vec::new();
    let name_ok = app.name.chars().count() >= 3;

    if name_ok {
        for n in name_variants(&app.name) {
            for (dir, kind) in [
                ("Application Support", "Application Support"),
                ("Caches", "Cache"),
                ("Logs", "Logs"),
                ("Preferences", "Preferences"),
            ] {
                cand.push((lib(home, &format!("{dir}/{n}")), kind));
            }
            cand.push((lib(home, &format!("Preferences/{n}.plist")), "Preferences"));
            cand.push((lib(home, &format!("Saved Application State/{n}.savedState")), "Saved state"));
        }
        // Plug-ins and extensions named after the app.
        for (dir, ext) in [
            ("Services", "workflow"),
            ("QuickLook", "qlgenerator"),
            ("Internet Plug-Ins", "plugin"),
            ("Audio/Plug-Ins/Components", "component"),
            ("Audio/Plug-Ins/VST", "vst"),
            ("Audio/Plug-Ins/VST3", "vst3"),
            ("PreferencePanes", "prefPane"),
            ("Input Methods", "app"),
            ("Screen Savers", "saver"),
            ("Frameworks", "framework"),
            ("Contextual Menu Items", "plugin"),
            ("Spotlight", "mdimporter"),
            ("ColorPickers", "colorPicker"),
            ("Workflows", "workflow"),
        ] {
            cand.push((lib(home, &format!("{dir}/{}.{ext}", app.name)), "Plug-in"));
        }
    }

    if let Some(id) = app.bundle_id.as_deref().filter(|i| is_reverse_dns(i)) {
        for (rel, kind) in [
            (format!("Application Support/{id}"), "Application Support"),
            (format!("Caches/{id}"), "Cache"),
            (format!("Logs/{id}"), "Logs"),
            (format!("Saved Application State/{id}.savedState"), "Saved state"),
            (format!("Containers/{id}"), "Container"),
            (format!("WebKit/{id}"), "WebKit data"),
            (format!("WebKit/com.apple.WebKit.WebContent/{id}"), "WebKit data"),
            (format!("HTTPStorages/{id}"), "HTTP storage"),
            (format!("HTTPStorages/{id}.binarycookies"), "Cookies"),
            (format!("Cookies/{id}.binarycookies"), "Cookies"),
            (format!("Application Scripts/{id}"), "Scripts"),
            (format!("Input Methods/{id}.app"), "Plug-in"),
            (format!("Autosave Information/{id}"), "Autosave"),
            (format!("SyncedPreferences/{id}.plist"), "Preferences"),
            (format!("Preferences/{id}.plist"), "Preferences"),
            (format!("Preferences/{id}"), "Preferences"),
            (format!("Caches/com.apple.nsurlsessiond/Downloads/{id}"), "Cache"),
        ] {
            cand.push((lib(home, &rel), kind));
        }

        // Per-host preferences: "<id>.<uuid>.plist".
        for p in children(&lib(home, "Preferences/ByHost")) {
            if starts_with_id_boundary(&file_name(&p), id) {
                cand.push((p, "Preferences"));
            }
        }
        // LaunchAgents: "<id>.plist" and helpers "<id>.*.plist".
        for p in children(&lib(home, "LaunchAgents")) {
            let n = file_name(&p);
            if n.ends_with(".plist") && starts_with_id_boundary(n.trim_end_matches(".plist"), id) {
                cand.push((p, "LaunchAgent"));
            }
        }
        // Folders named after the id (team-prefixed group containers, extensions, ...).
        for dir in ["Group Containers", "Application Scripts", "Containers", "Application Support/FileProvider"] {
            for p in children(&lib(home, dir)) {
                if p.is_dir() && has_id_boundary(&file_name(&p), id) {
                    cand.push((p, if dir == "Group Containers" { "Group container" } else { "Extension data" }));
                }
            }
        }
        // Recent-documents lists.
        let recents = lib(home, "Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.ApplicationRecentDocuments");
        for ext in ["sfl2", "sfl3", "sfl4"] {
            cand.push((recents.join(format!("{id}.{ext}")), "Recent documents"));
        }
    }

    // De-duplicate, keep existing, apply the safety checks.
    let mut out = Related::default();
    let mut seen = HashSet::new();
    let library_roots: HashSet<PathBuf> = [
        "Application Support", "Caches", "Logs", "Preferences", "Preferences/ByHost", "Containers", "WebKit",
        "HTTPStorages", "Application Scripts", "Autosave Information", "Group Containers", "LaunchAgents",
    ]
    .iter()
    .map(|r| lib(home, r))
    .collect();
    for (path, kind) in cand {
        if !seen.insert(path.clone()) || path.symlink_metadata().is_err() {
            continue;
        }
        // An empty name must never collapse a pattern into a whole Library folder.
        if library_roots.contains(&path) || path == home {
            continue;
        }
        if let Err(why) = validate_in(&path.to_string_lossy(), home, Mode::Uninstall) {
            out.refused.push((path, why));
            continue;
        }
        let size = disk_usage(&path).map(|u| u.0).unwrap_or(0);
        if kind == "LaunchAgent" {
            out.agents.push(path.clone());
        }
        out.files.push(Found { path, kind, size });
    }
    // Drop entries nested inside another entry so nothing is counted or moved twice.
    let paths: Vec<PathBuf> = out.files.iter().map(|f| f.path.clone()).collect();
    out.files.retain(|f| !paths.iter().any(|o| o != &f.path && f.path.starts_with(o)));

    out.system = system_leftovers(app);
    out
}

/// Administrator-owned leftovers: reported so the user can remove them by hand.
pub fn system_leftovers(app: &App) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let Some(id) = app.bundle_id.as_deref().filter(|i| is_reverse_dns(i)) else {
        return v;
    };
    for dir in ["/Library/LaunchAgents", "/Library/LaunchDaemons", "/Library/PrivilegedHelperTools"] {
        for p in children(Path::new(dir)) {
            let n = file_name(&p);
            if starts_with_id_boundary(n.trim_end_matches(".plist"), id) {
                v.push(p);
            }
        }
    }
    for dir in ["/Library/Application Support", "/Library/Caches", "/Library/Preferences"] {
        for p in children(Path::new(dir)) {
            if starts_with_id_boundary(file_name(&p).trim_end_matches(".plist"), id) {
                v.push(p);
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn id_boundaries_do_not_leak_between_products() {
        let id = "com.x.App";
        assert!(has_id_boundary("com.x.App", id));
        assert!(has_id_boundary("com.x.App.helper", id));
        assert!(has_id_boundary("TEAMID.com.x.App", id));
        assert!(has_id_boundary("group.com.x.App.shared", id));
        assert!(!has_id_boundary("com.x.Application", id), "prefix of a different id");
        assert!(!has_id_boundary("com.x.AppStore", id));
        assert!(!has_id_boundary("com.x.App", "not a bundle id"));
        assert!(starts_with_id_boundary("com.x.App.ABC.plist", id));
        assert!(!starts_with_id_boundary("TEAM.com.x.App", id));
    }

    #[test]
    fn name_variants_cover_common_spellings() {
        assert_eq!(name_variants("Maestro Studio"), vec!["Maestro Studio", "MaestroStudio", "Maestro_Studio", "Maestro-Studio"]);
        assert_eq!(name_variants("Slack"), vec!["Slack"]);
        let z = name_variants("Zed Nightly");
        assert!(z.contains(&"Zed".to_string()) && z.contains(&"ZedNightly".to_string()));
        assert_eq!(base_name("Firefox Developer Edition"), "Firefox");
        assert_eq!(base_name("Beta"), "Beta"); // nothing left of the name
    }

    fn fake_home(tag: &str) -> PathBuf {
        let h = std::env::temp_dir().join(format!("sweepr-un-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        h.canonicalize().unwrap()
    }
    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![b'x'; 5000]).unwrap();
    }

    #[test]
    fn finds_all_leftovers_and_nothing_else() {
        let h = fake_home("related");
        let l = h.join("Library");
        // belongs to "Fake App" / com.fake.app
        for p in [
            "Application Support/Fake App/db", "Caches/com.fake.app/c", "Logs/Fake App/l", "Preferences/com.fake.app.plist",
            "Preferences/ByHost/com.fake.app.1234.plist", "Containers/com.fake.app/Data/x", "Group Containers/TEAM.com.fake.app/g",
            "LaunchAgents/com.fake.app.helper.plist", "Saved Application State/com.fake.app.savedState/w",
            "Application Support/FakeApp/x",
        ] {
            touch(&l.join(p));
        }
        // belongs to someone else, including near-miss names
        for p in [
            "Application Support/Other/x", "Caches/com.fake.application/c", "Preferences/com.fake.apple.plist",
            "Group Containers/TEAM.com.fake.application/g", "LaunchAgents/com.fake.appx.plist", "Application Support/Fake Application/x",
        ] {
            touch(&l.join(p));
        }
        let app = App { name: "Fake App".into(), path: h.join("Applications/Fake App.app"), bundle_id: Some("com.fake.app".into()), version: None, size: 0 };
        let r = related_files(&app, &h);
        let mut got: Vec<String> = r.files.iter().map(|f| f.path.strip_prefix(&l).unwrap().to_string_lossy().to_string()).collect();
        got.sort();
        let mut want = vec![
            "Application Support/Fake App", "Application Support/FakeApp", "Caches/com.fake.app", "Containers/com.fake.app",
            "Group Containers/TEAM.com.fake.app", "LaunchAgents/com.fake.app.helper.plist", "Logs/Fake App",
            "Preferences/ByHost/com.fake.app.1234.plist", "Preferences/com.fake.app.plist",
            "Saved Application State/com.fake.app.savedState",
        ];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(r.agents.len(), 1);
        assert!(r.files.iter().all(|f| f.size > 0));
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn short_or_missing_names_never_expand_to_whole_folders() {
        let h = fake_home("short");
        touch(&h.join("Library/Application Support/X/data"));
        touch(&h.join("Library/Caches/unrelated/data"));
        let app = App { name: "X".into(), path: h.join("Applications/X.app"), bundle_id: None, version: None, size: 0 };
        assert!(related_files(&app, &h).files.is_empty(), "a 1-char name must not match anything");
        let empty = App { name: "".into(), path: h.join("Applications/.app"), bundle_id: Some("bogus id!".into()), version: None, size: 0 };
        assert!(related_files(&empty, &h).files.is_empty());
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn protected_system_data_is_refused_even_if_it_matches() {
        let h = fake_home("protected");
        touch(&h.join("Library/Keychains/com.fake.app/k"));
        touch(&h.join("Library/Application Support/com.fake.app/d"));
        let app = App { name: "Fake App".into(), path: h.join("Applications/Fake App.app"), bundle_id: Some("com.fake.app".into()), version: None, size: 0 };
        let r = related_files(&app, &h);
        assert!(r.files.iter().all(|f| !f.path.to_string_lossy().contains("Keychains")));
        assert_eq!(r.files.len(), 1);
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn nested_matches_are_collapsed() {
        let h = fake_home("nested");
        touch(&h.join("Library/Application Support/com.fake.app/deep/f"));
        touch(&h.join("Library/Containers/com.fake.app/Data/Library/Application Support/Fake App/f"));
        let app = App { name: "Fake App".into(), path: h.join("Applications/Fake App.app"), bundle_id: Some("com.fake.app".into()), version: None, size: 0 };
        let r = related_files(&app, &h);
        assert_eq!(r.files.len(), 2, "{:?}", r.files);
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn cask_detection() {
        assert_eq!(cask_token(Path::new("/definitely/not/a/cask.app")), None);
    }
}
