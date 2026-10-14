//! Which apps are running right now. Cleaning an app's cache while it runs can
//! corrupt open databases or force costly rebuilds, so those caches are deferred;
//! running apps are found from process executables and their Info.plist.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Default)]
pub struct Running {
    /// Bundle identifiers of every running `.app` (helpers included).
    pub bundle_ids: HashSet<String>,
    /// Lower-cased process names.
    names: HashSet<String>,
}

impl Running {
    pub fn detect() -> Self {
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
        );
        let mut me = Running::default();
        let mut plist_cache: HashMap<String, Option<String>> = HashMap::new();
        for p in sys.processes().values() {
            me.names.insert(p.name().to_string_lossy().to_lowercase());
            let Some(exe) = p.exe() else { continue };
            for app in app_bundles_of(exe) {
                let id = plist_cache
                    .entry(app.clone())
                    .or_insert_with(|| bundle_id_of(Path::new(&app)))
                    .clone();
                if let Some(id) = id {
                    me.bundle_ids.insert(id);
                }
            }
        }
        me
    }

    #[cfg(test)]
    pub fn with(ids: &[&str], names: &[&str]) -> Self {
        Self {
            bundle_ids: ids.iter().map(|s| s.to_string()).collect(),
            names: names.iter().map(|s| s.to_lowercase()).collect(),
        }
    }

    /// Is a process with this name (or one of its helpers, "Name Helper") running?
    pub fn process_running(&self, name: &str) -> bool {
        let n = name.to_lowercase();
        let prefix = format!("{n} ");
        self.names.contains(&n) || self.names.iter().any(|x| x.starts_with(&prefix))
    }

    /// Does the app that owns this `~/Library/...` path appear to be running?
    pub fn owner_running(&self, path: &str) -> bool {
        let Some(owner) = library_owner(path) else {
            return false;
        };
        let owner_l = owner.to_lowercase();
        self.bundle_ids.iter().any(|id| {
            let id_l = id.to_lowercase();
            id_l == owner_l
                || id_l.starts_with(&format!("{owner_l}.")) // cache "com.x.App", helper "com.x.App.helper"
                || owner_l.starts_with(&format!("{id_l}.")) // cache "com.x.App.ShipIt", app "com.x.App"
        }) || self.process_running(&owner)
    }
}

/// First path component below a per-app Library folder.
fn library_owner(path: &str) -> Option<String> {
    const ROOTS: &[&str] = &[
        "/Library/Caches/",
        "/Library/Logs/",
        "/Library/Application Support/",
        "/Library/Containers/",
        "/Library/Saved Application State/",
        "/Library/HTTPStorages/",
        "/Library/WebKit/",
    ];
    let home_lib = path.find("/Library/")?;
    let tail = &path[home_lib..];
    let root = ROOTS.iter().find(|r| tail.starts_with(**r))?;
    let owner = tail[root.len()..].split('/').next()?;
    let owner = owner.strip_suffix(".savedState").unwrap_or(owner);
    (!owner.is_empty()).then(|| owner.to_string())
}

/// Every enclosing `*.app` of an executable path, innermost first.
fn app_bundles_of(exe: &Path) -> Vec<String> {
    exe.ancestors()
        .filter(|a| a.extension().is_some_and(|e| e == "app"))
        .map(|a| a.to_string_lossy().to_string())
        .collect()
}

pub fn bundle_id_of(app: &Path) -> Option<String> {
    let plist = app.join("Contents/Info.plist");
    let v = plist::Value::from_file(plist).ok()?;
    v.as_dictionary()?.get("CFBundleIdentifier")?.as_string().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_extraction() {
        let h = "/Users/u/Library";
        assert_eq!(library_owner(&format!("{h}/Caches/com.x.App/sub")).as_deref(), Some("com.x.App"));
        assert_eq!(library_owner(&format!("{h}/Saved Application State/com.x.App.savedState")).as_deref(), Some("com.x.App"));
        assert_eq!(library_owner(&format!("{h}/Containers/com.x.App/Data/tmp")).as_deref(), Some("com.x.App"));
        assert_eq!(library_owner("/Users/u/.cache/vite"), None);
    }

    #[test]
    fn defers_caches_of_running_apps_only() {
        let r = Running::with(&["com.google.Chrome", "com.google.Chrome.helper"], &["google chrome", "slack helper"]);
        assert!(r.owner_running("/Users/u/Library/Caches/com.google.Chrome"));
        assert!(r.owner_running("/Users/u/Library/Caches/com.google.Chrome.ShipIt"));
        assert!(r.owner_running("/Users/u/Library/Caches/Slack")); // via "Slack Helper"
        assert!(!r.owner_running("/Users/u/Library/Caches/com.example.Idle"));
        assert!(!r.owner_running("/Users/u/.cache/anything"));
    }

    #[test]
    fn process_name_matching() {
        let r = Running::with(&[], &["google chrome helper (renderer)"]);
        assert!(r.process_running("Google Chrome"));
        assert!(!r.process_running("Firefox"));
    }

    #[test]
    fn nested_app_bundles() {
        let exe = Path::new("/Applications/A.app/Contents/Frameworks/H.app/Contents/MacOS/H");
        assert_eq!(app_bundles_of(exe), vec!["/Applications/A.app/Contents/Frameworks/H.app", "/Applications/A.app"]);
    }
}
