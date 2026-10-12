//! Smart uninstaller: removes an app together with its preferences, caches,
//! containers and LaunchAgents.
//!
//! Everything is moved to the Trash rather than deleted, so a wrong guess is
//! recoverable from Finder. Files that need administrator rights are listed, not
//! touched.

mod discover;
mod picker;

use crate::clean::safety::{Mode, is_critical_path, is_system_critical_bundle, validate_in};
use crate::clean::{Running, Ui};
use crate::status::human_bytes;
use crate::util::{move_to_trash, run};
use anyhow::{Result, bail};
use clap::Args as ClapArgs;
use discover::{App, Found, Related};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Apps to remove, by name or bundle id (omit to pick from a list)
    pub apps: Vec<String>,
    /// List installed apps and exit
    #[arg(long)]
    pub list: bool,
    /// Show what would be removed without moving anything
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Do not ask for confirmation
    #[arg(short, long)]
    pub yes: bool,
    /// Also show matches the safety checks refused
    #[arg(short, long)]
    pub verbose: bool,
}

struct Plan {
    app: App,
    related: Related,
    cask: Option<String>,
}

impl Plan {
    fn total(&self) -> u64 {
        self.app.size + self.related.files.iter().map(|f| f.size).sum::<u64>()
    }
}

pub fn run_cmd(args: Args) -> Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        bail!("run as your normal user, not root: apps in /Applications are moved to your own Trash");
    }
    let home = dirs::home_dir().filter(|h| h.is_absolute()).ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let ui = Ui::detect();

    ui.progress("Scanning installed apps…");
    let apps = discover::installed_apps(&home);
    ui.clear_progress();
    if apps.is_empty() {
        bail!("no apps found in /Applications or ~/Applications");
    }

    if args.list {
        for a in &apps {
            println!("{:<36} {:>10}  {:<12} {}", a.name, human_bytes(a.size), a.version.as_deref().unwrap_or(""), a.bundle_id.as_deref().unwrap_or(""));
        }
        return Ok(());
    }

    let chosen: Vec<usize> = if args.apps.is_empty() {
        if !std::io::stdout().is_terminal() {
            bail!("name the apps to remove (or use --list); the picker needs a terminal");
        }
        picker::pick(&apps)?
    } else {
        args.apps.iter().map(|q| resolve(&apps, q)).collect::<Result<_>>()?
    };
    if chosen.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }

    let plans: Vec<Plan> = chosen
        .iter()
        .map(|&i| {
            let app = apps[i].clone();
            let related = discover::related_files(&app, &home);
            let cask = discover::cask_token(&app.path);
            Plan { app, related, cask }
        })
        .collect();

    println!();
    if args.dry_run {
        println!("{}", ui.yellow("Dry run: nothing will be moved"));
    }
    for p in &plans {
        print_plan(&ui, p, args.verbose);
    }
    let removable: Vec<&Plan> = plans.iter().filter(|p| p.cask.is_none()).collect();
    let total: u64 = removable.iter().map(|p| p.total()).sum();
    println!("\nTotal to move to the Trash: {}", ui.green(&human_bytes(total)));
    if args.dry_run || removable.is_empty() {
        return Ok(());
    }
    if !args.yes && !confirm(removable.len())? {
        println!("Cancelled. Nothing was moved.");
        return Ok(());
    }

    let running = Running::detect();
    let mut failures = 0;
    for p in removable {
        println!();
        let o = remove(p, &home, &running);
        println!("{} {}", if o.failed.is_empty() && o.skipped.is_none() { ui.green("✓") } else { ui.yellow("⚠") }, ui.bold(&p.app.name));
        if let Some(why) = &o.skipped {
            println!("    skipped: {why}");
            failures += 1;
            continue;
        }
        println!("    moved {} item(s), {}", o.moved, human_bytes(o.bytes));
        if o.unloaded > 0 {
            println!("    unloaded {} launch agent(s)", o.unloaded);
        }
        for (path, why) in &o.failed {
            println!("    could not move {}: {why}", path.display());
            failures += 1;
        }
    }
    println!("\nMoved to the Trash. Empty the Trash to reclaim the space.");
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Match a name or bundle id exactly (case-insensitive); explain near misses.
fn resolve(apps: &[App], q: &str) -> Result<usize> {
    let ql = q.to_lowercase();
    let exact: Vec<usize> = (0..apps.len())
        .filter(|&i| apps[i].name.to_lowercase() == ql || apps[i].bundle_id.as_deref().is_some_and(|b| b.to_lowercase() == ql))
        .collect();
    match exact.as_slice() {
        [one] => Ok(*one),
        [] => {
            let near: Vec<&str> = apps.iter().filter(|a| a.name.to_lowercase().contains(&ql)).map(|a| a.name.as_str()).take(5).collect();
            if near.is_empty() {
                bail!("no installed app matches {q:?} (see `sweepr uninstall --list`)")
            } else {
                bail!("no installed app is named {q:?}; did you mean: {}", near.join(", "))
            }
        }
        many => bail!("{q:?} matches {} apps: {}", many.len(), many.iter().map(|&i| apps[i].path.display().to_string()).collect::<Vec<_>>().join(", ")),
    }
}

fn print_plan(ui: &Ui, p: &Plan, verbose: bool) {
    println!(
        "{} {}{}  {}",
        ui.bold("➤"),
        ui.bold(&p.app.name),
        p.app.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default(),
        ui.gray(&p.app.bundle_id.clone().unwrap_or_default())
    );
    if let Some(token) = &p.cask {
        println!("  {} managed by Homebrew. Run: brew uninstall --cask --zap {token}", ui.yellow("⚠"));
        return;
    }
    println!("  {} {} · {}", ui.green("✓"), p.app.path.display(), human_bytes(p.app.size));
    for f in &p.related.files {
        println!("  {} {} · {} {}", ui.green("✓"), tilde(&f.path), human_bytes(f.size), ui.gray(&format!("({})", f.kind)));
    }
    if !p.related.system.is_empty() {
        println!("  {} needs administrator rights (not touched):", ui.yellow("⚠"));
        for s in &p.related.system {
            println!("      {}", s.display());
        }
    }
    if verbose {
        for (path, why) in &p.related.refused {
            println!("  {} refused {}: {why}", ui.gray("·"), tilde(path));
        }
    }
}

fn tilde(p: &Path) -> String {
    match dirs::home_dir().and_then(|h| p.strip_prefix(&h).ok().map(|r| r.to_path_buf())) {
        Some(r) => format!("~/{}", r.display()),
        None => p.display().to_string(),
    }
}

fn confirm(n: usize) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("refusing to continue without confirmation in a non-interactive session; pass --yes");
    }
    print!("Move {n} app(s) and their leftovers to the Trash? [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

#[derive(Default)]
struct Outcome {
    moved: usize,
    bytes: u64,
    unloaded: usize,
    failed: Vec<(PathBuf, String)>,
    /// Set when the app was not touched at all.
    skipped: Option<String>,
}

/// The app bundle itself must live directly in an Applications folder and must
/// not be a protected macOS component.
fn validate_app_bundle(path: &Path, home: &Path, bundle_id: Option<&str>) -> Result<(), String> {
    let s = path.to_string_lossy();
    if path.extension().is_none_or(|e| e != "app") || s.split('/').any(|c| c == "..") {
        return Err("not an .app bundle".into());
    }
    let roots = [PathBuf::from("/Applications"), home.join("Applications")];
    if !roots.iter().any(|r| path.starts_with(r) && path != r) {
        return Err("not inside an Applications folder".into());
    }
    if is_critical_path(&s) || bundle_id.is_some_and(is_system_critical_bundle) {
        return Err("a protected macOS component".into());
    }
    Ok(())
}

fn remove(p: &Plan, home: &Path, running: &Running) -> Outcome {
    let mut o = Outcome::default();
    if let Some(id) = &p.app.bundle_id
        && running.bundle_ids.contains(id) {
            o.skipped = Some(format!("{} is running; quit it and try again", p.app.name));
            return o;
        }
    if let Err(why) = validate_app_bundle(&p.app.path, home, p.app.bundle_id.as_deref()) {
        o.skipped = Some(format!("refusing to remove {}: {why}", p.app.path.display()));
        return o;
    }

    // Stop background helpers first so they do not respawn files after removal.
    let uid = unsafe { libc::getuid() }.to_string();
    for plist in &p.related.agents {
        let domain = format!("gui/{uid}");
        if run("/bin/launchctl", &["bootout", &domain, &plist.to_string_lossy()], Duration::from_secs(10)).ok {
            o.unloaded += 1;
        }
    }

    // The app first: if that fails there is nothing half-removed to explain.
    match move_to_trash(&p.app.path, home) {
        Ok(_) => {
            o.moved += 1;
            o.bytes += p.app.size;
        }
        Err(e) => {
            let why = if e.kind() == std::io::ErrorKind::PermissionDenied {
                "permission denied (installed by an administrator; remove it in Finder)".to_string()
            } else {
                e.to_string()
            };
            o.failed.push((p.app.path.clone(), why));
            return o;
        }
    }
    for Found { path, size, .. } in &p.related.files {
        // Re-check at the moment of removal.
        if let Err(why) = validate_in(&path.to_string_lossy(), home, Mode::Uninstall) {
            o.failed.push((path.clone(), format!("no longer safe to move ({why})")));
            continue;
        }
        match move_to_trash(path, home) {
            Ok(_) => {
                o.moved += 1;
                o.bytes += size;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => o.failed.push((path.clone(), e.to_string())),
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn home(tag: &str) -> PathBuf {
        let h = std::env::temp_dir().join(format!("sweepr-un2-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        h.canonicalize().unwrap()
    }

    fn make_app(home: &Path, name: &str, id: &str) -> App {
        let path = home.join(format!("Applications/{name}.app"));
        fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
        fs::write(path.join("Contents/MacOS/bin"), vec![b'x'; 20_000]).unwrap();
        fs::write(
            path.join("Contents/Info.plist"),
            format!(r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>{id}</string><key>CFBundleShortVersionString</key><string>2.1</string></dict></plist>"#),
        )
        .unwrap();
        App { name: name.into(), path, bundle_id: Some(id.into()), version: Some("2.1".into()), size: 20_000 }
    }

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![b'x'; 5000]).unwrap();
    }

    #[test]
    fn end_to_end_removes_app_and_leftovers_to_trash_and_spares_others() {
        let h = home("e2e");
        let app = make_app(&h, "Fake App", "com.fake.app");
        touch(&h.join("Library/Application Support/Fake App/db"));
        touch(&h.join("Library/Preferences/com.fake.app.plist"));
        touch(&h.join("Library/Caches/com.fake.app/c"));
        touch(&h.join("Library/Application Support/Other App/keep"));
        touch(&h.join("Library/Caches/com.fake.application/keep"));
        touch(&h.join("Documents/precious.txt"));

        let related = discover::related_files(&app, &h);
        let plan = Plan { app, related, cask: None };
        let o = remove(&plan, &h, &Running::default());
        assert!(o.skipped.is_none() && o.failed.is_empty(), "{:?} {:?}", o.skipped, o.failed);
        assert_eq!(o.moved, 4); // bundle + 3 leftovers

        assert!(!h.join("Applications/Fake App.app").exists());
        assert!(!h.join("Library/Application Support/Fake App").exists());
        assert!(!h.join("Library/Preferences/com.fake.app.plist").exists());
        // recoverable, not destroyed
        assert!(h.join(".Trash/Fake App.app/Contents/Info.plist").exists());
        assert!(h.join(".Trash/com.fake.app.plist").exists());
        // everything else untouched
        assert!(h.join("Library/Application Support/Other App/keep").exists());
        assert!(h.join("Library/Caches/com.fake.application/keep").exists());
        assert!(h.join("Documents/precious.txt").exists());
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn running_app_is_not_touched() {
        let h = home("running");
        let app = make_app(&h, "Busy App", "com.busy.app");
        touch(&h.join("Library/Caches/com.busy.app/c"));
        let related = discover::related_files(&app, &h);
        let plan = Plan { app, related, cask: None };
        let o = remove(&plan, &h, &Running::with(&["com.busy.app"], &[]));
        assert!(o.skipped.is_some());
        assert!(h.join("Applications/Busy App.app").exists());
        assert!(h.join("Library/Caches/com.busy.app/c").exists());
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn protected_and_misplaced_bundles_are_refused() {
        let h = home("refuse");
        assert!(validate_app_bundle(Path::new("/Applications/Safari.app"), &h, Some("com.apple.Safari")).is_err());
        assert!(validate_app_bundle(Path::new("/Applications/Finder.app"), &h, None).is_err());
        assert!(validate_app_bundle(Path::new("/Applications"), &h, None).is_err());
        assert!(validate_app_bundle(Path::new("/usr/bin/thing.app"), &h, None).is_err());
        assert!(validate_app_bundle(Path::new("/Applications/../etc/x.app"), &h, None).is_err());
        assert!(validate_app_bundle(Path::new("/Applications/Notes"), &h, None).is_err());
        assert!(validate_app_bundle(&h.join("Applications/Mine.app"), &h, Some("com.me.mine")).is_ok());
        assert!(validate_app_bundle(Path::new("/Applications/Some Tool.app"), &h, Some("com.x.tool")).is_ok());
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn name_resolution_is_exact_and_explains_misses() {
        let mk = |n: &str, id: &str| App { name: n.into(), path: PathBuf::from(format!("/Applications/{n}.app")), bundle_id: Some(id.into()), version: None, size: 0 };
        let apps = vec![mk("Slack", "com.tinyspeck.slackmacgap"), mk("Slack Helper Tool", "com.x.slackhelper")];
        assert_eq!(resolve(&apps, "slack").unwrap(), 0);
        assert_eq!(resolve(&apps, "com.x.SlackHelper").unwrap(), 1);
        let e = resolve(&apps, "Sla").unwrap_err().to_string();
        assert!(e.contains("did you mean") && e.contains("Slack"), "{e}");
        assert!(resolve(&apps, "nothing").unwrap_err().to_string().contains("no installed app matches"));
    }

    #[test]
    fn installed_apps_excludes_system_components() {
        // The real machine: Safari/Finder-class bundles must never be offered.
        let h = dirs::home_dir().unwrap();
        for a in discover::installed_apps(&h) {
            if let Some(id) = &a.bundle_id {
                assert!(!is_system_critical_bundle(id), "{} offered for removal", a.name);
            }
        }
    }
}
