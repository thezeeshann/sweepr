//! What gets cleaned, grouped into Mole's section order. Every entry here comes
//! from Mole's own cleanup modules (`lib/clean/*.sh`); entries that Mole guards
//! behind conditions this port does not reproduce (Xcode/simulator process
//! state, compiled-model holds, vendor-specific rules) are deliberately left out.
//! Everything still passes the shared safety funnel in `Engine::consider`.

use super::engine::{Engine, Guard};
use super::orphans;
use std::path::{Path, PathBuf};

pub type SectionFn = fn(&mut Engine);

/// Section order follows Mole's `perform_cleanup`.
pub const SECTIONS: &[(&str, SectionFn)] = &[
    ("User essentials", user_essentials),
    ("App caches", app_caches),
    ("Browsers", browsers),
    ("Developer tools", developer_tools),
    ("Application Support", application_support),
    ("App leftovers", app_leftovers),
];

/// Retention for Application Support clutter (Mole: MOLE_SUPPORT_CACHE_AGE_DAYS).
const SUPPORT_AGE_DAYS: u64 = 30;

fn user_essentials(e: &mut Engine) {
    // Every child of ~/Library/Caches, each judged on its own.
    let caches = e.home.join("Library/Caches");
    let deno = deno_root(&e.home);
    if let Ok(rd) = std::fs::read_dir(&caches) {
        let mut kids: Vec<PathBuf> = rd.flatten().map(|d| d.path()).collect();
        kids.sort();
        for kid in kids {
            // `deno clean` would take the whole DENO_DIR (origin storage, runtime
            // payloads), so keep any candidate that is, holds, or sits in it.
            if deno.as_ref().is_some_and(|d| overlaps(&kid, d)) {
                continue;
            }
            e.consider("User app cache", &kid, Guard::OwnerRunning);
        }
    }
    e.glob("User app logs", "~/Library/Logs/*", Guard::None);

    let trash = e.home.join(".Trash");
    e.children("Trash", &trash, Guard::None);

    // Messages previews and stickers only; attachments are never touched.
    e.glob("Messages sticker cache", "~/Library/Messages/StickerCache/*", Guard::None);
    e.glob("Messages preview attachment cache", "~/Library/Messages/Caches/Previews/Attachments/*", Guard::None);
    e.glob("Messages preview sticker cache", "~/Library/Messages/Caches/Previews/StickerCache/*", Guard::None);
}

fn deno_root(home: &Path) -> Option<PathBuf> {
    match std::env::var("DENO_DIR") {
        Ok(d) if Path::new(&d).is_absolute() => Some(PathBuf::from(d)),
        _ => Some(home.join("Library/Caches/deno")),
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn app_caches(e: &mut Engine) {
    const TARGETS: &[(&str, &str)] = &[
        ("~/Library/Saved Application State/*", "Saved application states"),
        ("~/Library/Caches/com.apple.WebKit.Networking/*", "WebKit network cache"),
        ("~/Library/DiagnosticReports/*", "Diagnostic reports"),
        ("~/Library/Caches/Quick Look/*", "QuickLook cache"),
        ("~/Library/IdentityCaches/*", "Identity caches"),
        ("~/Library/Suggestions/*", "Siri suggestions cache"),
        ("~/Library/Calendars/Calendar Cache", "Calendar cache"),
        ("~/Library/Application Support/AddressBook/Sources/*/Photos.cache", "Address Book photo cache"),
        ("~/Library/Containers/com.apple.wallpaper.agent/Data/Library/Caches/*", "Wallpaper agent cache"),
        ("~/Library/Containers/com.apple.mediaanalysisd/Data/Library/Caches/*", "Media analysis cache"),
        ("~/Library/Containers/com.apple.mediaanalysisd/Data/tmp/*", "Media analysis temp files"),
        ("~/Library/Containers/com.apple.AppStore/Data/Library/Caches/*", "App Store cache"),
        ("~/Library/Containers/com.apple.configurator.xpc.InternetService/Data/tmp/*", "Apple Configurator temp files"),
        ("~/Library/Containers/com.apple.wallpaper.extension.aerials/Data/tmp/*", "Wallpaper aerials temp files"),
        ("~/Library/Containers/com.apple.geod/Data/tmp/*", "Geod temp files"),
        ("~/Library/Containers/com.apple.stocks/Data/Library/Caches/*", "Stocks cache"),
        ("~/Library/Caches/com.apple.helpd/*", "macOS Help system cache"),
        ("~/Library/Caches/GeoServices/*", "Maps geo tile cache"),
        ("~/Library/Containers/com.apple.AvatarUI.AvatarPickerMemojiPicker/Data/Library/Caches/*", "Memoji picker cache"),
        ("~/Library/Containers/com.apple.AMPArtworkAgent/Data/Library/Caches/*", "Music album art cache"),
        ("~/Library/Containers/com.apple.CoreDevice.CoreDeviceService/Data/Library/Caches/*", "CoreDevice service cache"),
        ("~/Library/Containers/com.apple.NeptuneOneExtension/Data/Library/Caches/*", "Apple Intelligence extension cache"),
        ("~/Library/Containers/com.apple.AppleMediaServicesUI.UtilityExtension/Data/tmp/*", "Apple Media Services temp files"),
        ("~/Library/Caches/com.apple.AppleMediaServices/*", "Apple Media Services cache"),
        ("~/Library/Caches/com.apple.duetexpertd/*", "Duet Expert cache"),
        ("~/Library/Caches/com.apple.parsecd/*", "Parsecd cache"),
        ("~/Library/Caches/com.apple.python/*", "Apple Python cache"),
    ];
    for (pattern, label) in TARGETS {
        e.glob(label, pattern, Guard::OwnerRunning);
    }
}

struct Chromium {
    name: &'static str,
    caches: &'static [&'static str],
    /// Roots that hold per-profile folders and a few browser-wide caches.
    support: &'static [&'static str],
    /// Process names that mean the browser is open.
    procs: &'static [&'static str],
}

const CHROMIUM: &[Chromium] = &[
    Chromium {
        name: "Chrome",
        caches: &["~/Library/Caches/Google/Chrome/*"],
        support: &["~/Library/Application Support/Google/Chrome"],
        procs: &["Google Chrome"],
    },
    Chromium {
        name: "Chromium",
        caches: &["~/Library/Caches/Chromium/*"],
        support: &["~/Library/Application Support/Chromium"],
        procs: &["Chromium"],
    },
    Chromium {
        name: "Edge",
        caches: &["~/Library/Caches/com.microsoft.edgemac/*"],
        support: &["~/Library/Application Support/Microsoft Edge"],
        procs: &["Microsoft Edge"],
    },
    Chromium {
        name: "Brave",
        caches: &["~/Library/Caches/BraveSoftware/Brave-Browser/*"],
        support: &["~/Library/Application Support/BraveSoftware/Brave-Browser"],
        procs: &["Brave Browser"],
    },
    Chromium {
        name: "Arc",
        caches: &["~/Library/Caches/company.thebrowser.Browser/*"],
        support: &["~/Library/Application Support/Arc", "~/Library/Application Support/Arc/User Data"],
        procs: &["Arc"],
    },
];

/// Rebuildable per-profile caches. Service Worker caches are deliberately absent:
/// removing MV3 extension bytecode can break extension service workers.
const PROFILE_CACHES: &[&str] = &[
    "Application Cache", "Code Cache", "GPUCache", "DawnCache", "GrShaderCache", "GraphiteDawnCache",
    "DawnGraphiteCache", "DawnWebGPUCache",
];
const BROWSER_WIDE_CACHES: &[&str] = &[
    "component_crx_cache", "ShaderCache", "GrShaderCache", "GraphiteDawnCache", "Crashpad/completed",
    "OptGuideOnDeviceModel", "OptGuideOnDeviceClassifierModel", "optimization_guide_model_store",
];

fn browsers(e: &mut Engine) {
    // Safari is not guarded: WebKit manages its cache directory itself.
    e.glob("Safari cache", "~/Library/Caches/com.apple.Safari/*", Guard::None);

    for b in CHROMIUM {
        // Nothing under a running Chromium browser is touched, caches included.
        let guard = Guard::ProcessRunning(b.procs);
        for pat in b.caches {
            e.glob(&format!("{} cache", b.name), pat, guard);
        }
        for root in b.support {
            for dir in PROFILE_CACHES {
                e.glob(&format!("{} profile caches", b.name), &format!("{root}/*/{dir}/*"), guard);
            }
            for dir in BROWSER_WIDE_CACHES {
                e.glob(&format!("{} profile caches", b.name), &format!("{root}/{dir}/*"), guard);
            }
        }
    }
    let chrome = Guard::ProcessRunning(&["Google Chrome"]);
    e.glob("GoogleUpdater CRX cache", "~/Library/Application Support/Google/GoogleUpdater/crx_cache/*", chrome);
    e.glob("GoogleUpdater old files", "~/Library/Application Support/Google/GoogleUpdater/*.old", chrome);

    let firefox = Guard::ProcessRunning(&["firefox"]);
    e.glob("Firefox cache", "~/Library/Caches/Firefox/*", firefox);
    e.glob("Firefox profile cache", "~/Library/Application Support/Firefox/Profiles/*/cache2/*", firefox);
}

fn developer_tools(e: &mut Engine) {
    const TARGETS: &[(&str, &str)] = &[
        ("~/.npm/_cacache/*", "npm cache"),
        ("~/.npm/_npx/*", "npx cache"),
        ("~/.npm/_logs/*", "npm logs"),
        ("~/.yarn/cache/*", "Yarn cache"),
        ("~/Library/Caches/Yarn/*", "Yarn v1 cache"),
        ("~/.cache/poetry/*", "Poetry cache"),
        ("~/.cache/ruff/*", "Ruff cache"),
        ("~/.cache/mypy/*", "MyPy cache"),
        ("~/.bundle/cache/*", "Ruby Bundler cache"),
        ("~/.docker/buildx/cache/*", "Docker BuildX cache"),
        ("~/.cache/typescript/*", "TypeScript cache"),
        ("~/.cache/electron/*", "Electron cache"),
        ("~/.cache/node-gyp/*", "node-gyp cache"),
        ("~/.cache/vite/*", "Vite global cache"),
        ("~/.cache/webpack/*", "Webpack cache"),
        ("~/.cache/eslint/*", "ESLint cache"),
        ("~/.cache/prettier/*", "Prettier cache"),
        ("~/.cache/swift-package-manager/*", "Swift package manager cache"),
        ("~/.composer/cache/*", "PHP Composer cache (legacy)"),
        ("~/.cache/bazel/*", "Bazel cache"),
        ("~/.cache/zig/*", "Zig cache"),
        ("~/.cache/terraform/*", "Terraform cache"),
        ("~/.cache/gitlab-runner/*", "GitLab Runner cache"),
        ("~/.cache/prisma/*", "Prisma cache"),
        ("~/.cache/pre-commit/*", "pre-commit cache"),
        ("~/.cache/puppeteer/*", "Puppeteer browser cache"),
        ("~/.cache/curl/*", "curl cache"),
        ("~/.cache/wget/*", "wget cache"),
        ("~/Library/Caches/Homebrew/downloads/*", "Homebrew cache"),
    ];
    for (pattern, label) in TARGETS {
        e.glob(label, pattern, Guard::None);
    }
}

fn application_support(e: &mut Engine) {
    let dir = e.home.join("Library/Application Support/CrashReporter");
    // A symlinked CrashReporter folder could point anywhere: leave it alone.
    if dir.is_dir() && !dir.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        e.aged_files("Crash reporter data", &dir, "*", SUPPORT_AGE_DAYS, 3);
    }
}

fn app_leftovers(e: &mut Engine) {
    if let Err(reason) = orphans::scan(e) {
        e.notes.push(format!("App leftovers skipped: {reason}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_pattern_is_a_valid_glob_under_home() {
        let mut all: Vec<String> = Vec::new();
        for b in CHROMIUM {
            all.extend(b.caches.iter().map(|s| s.to_string()));
            for root in b.support {
                for d in PROFILE_CACHES {
                    all.push(format!("{root}/*/{d}/*"));
                }
                for d in BROWSER_WIDE_CACHES {
                    all.push(format!("{root}/{d}/*"));
                }
            }
        }
        for p in &all {
            assert!(p.starts_with("~/"), "{p} must be home-relative");
            glob::Pattern::new(p.trim_start_matches("~/")).unwrap_or_else(|e| panic!("{p}: {e}"));
        }
        assert!(all.len() > 50);
    }

    #[test]
    fn service_worker_caches_are_never_targeted() {
        for d in PROFILE_CACHES.iter().chain(BROWSER_WIDE_CACHES) {
            assert!(!d.to_lowercase().contains("service worker"), "{d}");
        }
    }

    #[test]
    fn deno_root_overlap_rules() {
        let root = Path::new("/h/Library/Caches/deno");
        assert!(overlaps(Path::new("/h/Library/Caches/deno"), root));
        assert!(overlaps(Path::new("/h/Library/Caches"), root)); // would contain it
        assert!(overlaps(Path::new("/h/Library/Caches/deno/npm"), root));
        assert!(!overlaps(Path::new("/h/Library/Caches/other"), root));
    }
}
