//! User-protected paths. Mirrors Mole's `DEFAULT_WHITELIST_PATTERNS`,
//! `SAFETY_WHITELIST_PATTERNS` and `is_path_whitelisted` semantics.
//!
//! The user's own list lives in `~/.config/sweepr/whitelist` (one path or glob
//! per line, `~` allowed), kept separate from Mole's file so the two tools never
//! overwrite each other's settings.

use super::safety::normalize;
use anyhow::{Context, Result, bail};
use glob::Pattern;
use std::path::{Path, PathBuf};

/// Convenience protections that a user list may replace.
const DEFAULTS: &[&str] = &[
    "~/Library/Caches/ms-playwright*",
    "~/.gradle/caches/*",
    "~/.gradle/daemon/*",
    "~/.ollama/models/*",
    "~/Library/Caches/com.nssurge.surge-mac/*",
    "~/Library/Application Support/com.nssurge.surge-mac/*",
    "~/Library/Caches/org.R-project.R/R/renv/*",
    "~/Library/Caches/JetBrains*",
    "~/Library/Caches/com.jetbrains.toolbox*",
    "~/Library/Caches/tealdeer/tldr-pages",
    "~/Library/Application Support/JetBrains*",
    "~/Library/Caches/com.apple.finder",
    "~/Library/Mobile Documents*",
];

/// Hard safety: always merged, even when the user supplies their own list.
/// These break search, fonts or iCloud sync, or hold live interpreters.
const SAFETY: &[&str] = &[
    "~/Library/Caches/com.apple.FontRegistry*",
    "~/Library/Caches/com.apple.spotlight*",
    "~/Library/Caches/com.apple.Spotlight*",
    "~/Library/Caches/CloudKit*",
    "~/Library/Caches/pypoetry/virtualenvs*",
];

pub struct Whitelist {
    entries: Vec<Entry>,
    user_count: usize,
    pub file: PathBuf,
}

struct Entry {
    pattern: String,
    glob: Option<Pattern>,
    has_glob: bool,
}

impl Entry {
    fn new(raw: &str, home: &Path) -> Self {
        let expanded = expand_tilde(raw, home);
        let pattern = normalize(&expanded);
        let has_glob = pattern.contains(['*', '?', '[']);
        let glob = Pattern::new(&pattern).ok();
        Self { pattern, glob, has_glob }
    }
}

pub fn expand_tilde(s: &str, home: &Path) -> String {
    if s == "~" {
        home.to_string_lossy().to_string()
    } else if let Some(rest) = s.strip_prefix("~/") {
        format!("{}/{}", home.to_string_lossy(), rest)
    } else {
        s.to_string()
    }
}

pub fn config_file(home: &Path) -> PathBuf {
    home.join(".config/sweepr/whitelist")
}

fn read_lines(file: &Path) -> Vec<String> {
    std::fs::read_to_string(file)
        .map(|t| {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

impl Whitelist {
    pub fn load(home: &Path) -> Self {
        let file = config_file(home);
        let user = read_lines(&file);
        // A saved user list replaces the convenience defaults, but the safety
        // patterns always apply.
        let base: Vec<String> = if file.exists() {
            user.clone()
        } else {
            DEFAULTS.iter().map(|s| s.to_string()).collect()
        };
        let user_count = user.len();
        let entries = base
            .iter()
            .map(String::as_str)
            .chain(SAFETY.iter().copied())
            .map(|p| Entry::new(p, home))
            .collect();
        Self { entries, user_count, file }
    }

    /// (always-on core patterns, user-supplied patterns)
    pub fn counts(&self) -> (usize, usize) {
        (SAFETY.len() + if self.user_count == 0 { DEFAULTS.len() } else { 0 }, self.user_count)
    }

    pub fn patterns(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.pattern.as_str())
    }

    /// Port of `is_path_whitelisted`.
    pub fn is_protected(&self, target: &str) -> bool {
        if target.is_empty() {
            return false;
        }
        let t = normalize(target);
        self.entries.iter().any(|e| {
            let exact_or_glob = t == e.pattern || e.glob.as_ref().is_some_and(|g| g.matches(&t));
            // target is a parent of a whitelisted path: keep it so its protected children survive
            let parent_of_protected = e.pattern.strip_prefix(t.as_str()).is_some_and(|r| r.starts_with('/'));
            // target is inside a whitelisted (non-glob) directory
            let child_of_protected = !e.has_glob && t.strip_prefix(e.pattern.as_str()).is_some_and(|r| r.starts_with('/'));
            exact_or_glob || parent_of_protected || child_of_protected
        })
    }
}

/// Append `path` to the user's whitelist file (creating it, seeded with the
/// defaults so adding one entry does not silently drop them).
pub fn add(home: &Path, raw: &str) -> Result<()> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty path");
    }
    let file = config_file(home);
    let mut lines = if file.exists() {
        read_lines(&file)
    } else {
        DEFAULTS.iter().map(|s| s.to_string()).collect()
    };
    if lines.iter().any(|l| l == raw) {
        return Ok(());
    }
    lines.push(raw.to_string());
    write(&file, &lines)
}

pub fn remove(home: &Path, raw: &str) -> Result<bool> {
    let file = config_file(home);
    let mut lines = if file.exists() {
        read_lines(&file)
    } else {
        DEFAULTS.iter().map(|s| s.to_string()).collect()
    };
    let before = lines.len();
    lines.retain(|l| l != raw.trim());
    if lines.len() == before {
        return Ok(false);
    }
    write(&file, &lines)?;
    Ok(true)
}

fn write(file: &Path, lines: &[String]) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Write-then-rename so an interrupted run cannot leave a truncated list.
    let tmp = file.with_extension("tmp");
    let mut body = String::from("# sweepr whitelist: one path or glob per line (~ allowed)\n");
    for l in lines {
        body.push_str(l);
        body.push('\n');
    }
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, file).with_context(|| format!("replacing {}", file.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wl(patterns: &[&str]) -> Whitelist {
        let home = Path::new("/Users/u");
        Whitelist {
            entries: patterns.iter().map(|p| Entry::new(p, home)).collect(),
            user_count: patterns.len(),
            file: PathBuf::from("/nonexistent"),
        }
    }

    #[test]
    fn exact_and_glob_matches() {
        let w = wl(&["~/Library/Caches/JetBrains*", "~/.gradle/caches/*"]);
        assert!(w.is_protected("/Users/u/Library/Caches/JetBrains"));
        assert!(w.is_protected("/Users/u/Library/Caches/JetBrainsClient/x"));
        assert!(w.is_protected("/Users/u/.gradle/caches/modules-2"));
        assert!(!w.is_protected("/Users/u/Library/Caches/Other"));
    }

    #[test]
    fn parent_and_child_of_protected_path() {
        let w = wl(&["~/Library/Caches/tealdeer/tldr-pages"]);
        assert!(w.is_protected("/Users/u/Library/Caches/tealdeer")); // parent kept
        assert!(w.is_protected("/Users/u/Library/Caches/tealdeer/tldr-pages/sub")); // child
        assert!(!w.is_protected("/Users/u/Library/Caches/tealdeer2"));
    }

    #[test]
    fn glob_pattern_does_not_protect_arbitrary_children() {
        // children are only inherited from non-glob directory entries
        let w = wl(&["~/a/b*"]);
        assert!(w.is_protected("/Users/u/a/bcd"));
        assert!(!w.is_protected("/Users/u/a/other/x"));
    }

    #[test]
    fn normalizes_double_slashes_and_trailing_slash() {
        let w = wl(&["~/x/y"]);
        assert!(w.is_protected("/Users/u//x//y/"));
    }

    #[test]
    fn safety_patterns_always_apply() {
        let w = Whitelist::load(Path::new("/definitely/not/a/real/home"));
        assert!(w.is_protected("/definitely/not/a/real/home/Library/Caches/CloudKit"));
        assert!(w.is_protected("/definitely/not/a/real/home/Library/Caches/pypoetry/virtualenvs/x"));
    }
}
