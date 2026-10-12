//! Interactive disk analyzer.
//!
//! Key bindings follow the ones recovered from the original analyzer binary:
//! Enter open folder, Space select, / filter, R refresh, O open, P preview,
//! F reveal in Finder, Del move to Trash, T large files, Esc back, Q quit.

use super::scan::{self, DirSize, Entry, LargeFiles, SizeMsg};
use crate::clean::safety::{Mode, validate_in};
use crate::status::human_bytes;
use crate::util::{move_to_trash, run};
use anyhow::Result;
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub enum Loc {
    Overview,
    Dir(PathBuf),
    /// Large files under this root.
    Large(PathBuf),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sort {
    Size,
    Name,
    Modified,
}

#[derive(Clone, Debug)]
pub enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Left,
    Right,
    Enter,
    Esc,
    Space,
    Backspace,
    Delete,
    Char(char),
    CtrlC,
}

enum Misc {
    OldDownloads(u64),
    Snapshots(Option<usize>),
    Large(LargeFiles),
}

pub struct App {
    pub home: PathBuf,
    pub stack: Vec<Loc>,
    pub entries: Vec<Entry>,
    pub visible: Vec<usize>,
    pub cursor: usize,
    pub selected: BTreeSet<PathBuf>,
    pub filter: String,
    pub filtering: bool,
    pub sort: Sort,
    pub pending: usize,
    pub confirm: Option<(Vec<PathBuf>, u64)>,
    pub message: String,
    pub note: String,
    pub quit: bool,
    pub min_large: u64,
    pub tick: usize,
    /// Has the user moved the cursor since this view opened? Until then it stays on the top row.
    moved: bool,
    /// (capacity, free) of the disk holding the home folder, for the overview.
    pub disk: Option<(u64, u64)>,
    cache: HashMap<PathBuf, DirSize>,
    size_rx: Option<Receiver<SizeMsg>>,
    misc_rx: Option<Receiver<Misc>>,
    cancel: Arc<AtomicBool>,
}

/// Home folders macOS and most users depend on: never offered for deletion.
const PROTECTED_HOME_DIRS: &[&str] =
    &["Desktop", "Documents", "Downloads", "Library", "Movies", "Music", "Pictures", "Public", "Applications", ".Trash"];

/// (capacity, free) bytes of the volume holding `path`.
fn disk_space(path: &Path) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path.to_str()?).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a zeroed statvfs.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| (st.f_blocks as u64 * st.f_frsize, st.f_bavail as u64 * st.f_frsize))
}

pub fn overview_rows(home: &Path) -> Vec<(String, PathBuf)> {
    let rows = [
        ("Home".to_string(), home.to_path_buf()),
        ("Applications".to_string(), PathBuf::from("/Applications")),
        ("Library".to_string(), home.join("Library")),
        ("System Library".to_string(), PathBuf::from("/Library")),
        ("Downloads".to_string(), home.join("Downloads")),
        ("Documents".to_string(), home.join("Documents")),
        ("Desktop".to_string(), home.join("Desktop")),
        ("Movies".to_string(), home.join("Movies")),
        ("Developer".to_string(), home.join("Developer")),
        ("Trash".to_string(), home.join(".Trash")),
        ("· Xcode DerivedData".to_string(), home.join("Library/Developer/Xcode/DerivedData")),
        ("· App caches".to_string(), home.join("Library/Caches")),
    ];
    rows.into_iter().filter(|(_, p)| p.is_dir()).collect()
}

impl App {
    pub fn new(start: Option<PathBuf>, home: PathBuf, min_large: u64) -> Self {
        let mut a = Self {
            home,
            stack: vec![start.map(Loc::Dir).unwrap_or(Loc::Overview)],
            entries: Vec::new(),
            visible: Vec::new(),
            cursor: 0,
            selected: BTreeSet::new(),
            filter: String::new(),
            filtering: false,
            sort: Sort::Size,
            pending: 0,
            confirm: None,
            message: String::new(),
            note: String::new(),
            quit: false,
            min_large,
            tick: 0,
            moved: false,
            disk: None,
            cache: HashMap::new(),
            size_rx: None,
            misc_rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        a.disk = disk_space(&a.home);
        a.load();
        a
    }

    pub fn loc(&self) -> &Loc {
        self.stack.last().expect("stack is never empty")
    }

    fn stop_scans(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.size_rx = None;
        self.misc_rx = None;
    }

    /// (Re)build the entry list for the current location and start measuring.
    pub fn load(&mut self) {
        self.stop_scans();
        self.entries.clear();
        self.selected.clear();
        self.filter.clear();
        self.filtering = false;
        self.cursor = 0;
        self.moved = false;
        self.note.clear();
        let (stx, srx) = channel();
        let (mtx, mrx) = channel();
        let mut to_measure = Vec::new();

        match self.loc().clone() {
            Loc::Overview => {
                for (name, path) in overview_rows(&self.home) {
                    self.entries.push(Entry { name, path: path.clone(), is_dir: true, size: None, files: 0, modified: None, partial: false });
                    to_measure.push(path);
                }
                let dl = self.home.join("Downloads");
                let tx = mtx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(Misc::OldDownloads(scan::old_files_size(&dl, 90)));
                });
                let tx = mtx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(Misc::Snapshots(scan::local_snapshot_count()));
                });
            }
            Loc::Dir(dir) => match scan::list_dir(&dir) {
                Ok(list) => {
                    for mut e in list {
                        if e.is_dir {
                            match self.cache.get(&e.path) {
                                Some(c) => {
                                    e.size = Some(c.bytes);
                                    e.files = c.files;
                                    e.partial = c.partial;
                                }
                                None => to_measure.push(e.path.clone()),
                            }
                        }
                        self.entries.push(e);
                    }
                }
                Err(e) => {
                    self.message = if e.kind() == std::io::ErrorKind::PermissionDenied {
                        format!("Cannot read {}: grant your terminal Full Disk Access in System Settings", dir.display())
                    } else {
                        format!("Cannot read {}: {e}", dir.display())
                    };
                }
            },
            Loc::Large(root) => {
                self.pending = 1;
                let (min, cancel) = (self.min_large, self.cancel.clone());
                std::thread::spawn(move || {
                    let _ = mtx.send(Misc::Large(scan::large_files(&root, min, 200, &cancel)));
                });
            }
        }
        if !to_measure.is_empty() {
            scan::spawn_sizes(to_measure, self.cancel.clone(), stx);
        }
        self.size_rx = Some(srx);
        self.misc_rx = Some(mrx);
        if !matches!(self.loc(), Loc::Large(_)) {
            self.pending = self.entries.iter().filter(|e| e.is_dir && e.size.is_none()).count();
        }
        self.recompute();
    }

    /// Absorb finished measurements. Returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        self.tick = self.tick.wrapping_add(1);
        let mut changed = false;
        if let Some(rx) = &self.size_rx {
            while let Ok(m) = rx.try_recv() {
                self.cache.insert(m.path.clone(), m.size);
                if let Some(e) = self.entries.iter_mut().find(|e| e.path == m.path) {
                    e.size = Some(m.size.bytes);
                    e.files = m.size.files;
                    e.partial = m.size.partial;
                }
                changed = true;
            }
        }
        let mut misc = Vec::new();
        if let Some(rx) = &self.misc_rx {
            while let Ok(m) = rx.try_recv() {
                misc.push(m);
            }
        }
        for m in misc {
            changed = true;
            match m {
                Misc::OldDownloads(b) if b > 0 => {
                    self.entries.push(Entry {
                        name: "· Old Downloads (90+ days)".into(),
                        path: self.home.join("Downloads"),
                        is_dir: true,
                        size: Some(b),
                        files: 0,
                        modified: None,
                        partial: false,
                    });
                }
                Misc::Snapshots(Some(n)) if n > 0 => {
                    self.note = format!("{n} Time Machine local snapshot(s) hold space that is not listed here");
                }
                Misc::Large(l) => {
                    self.entries = l
                        .files
                        .into_iter()
                        .map(|(p, s, m)| Entry {
                            name: p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                            path: p,
                            is_dir: false,
                            size: Some(s),
                            files: 1,
                            modified: m,
                            partial: false,
                        })
                        .collect();
                    self.pending = 0;
                    self.note = if l.via_spotlight {
                        format!("Files over {} found with Spotlight (unindexed files can be missed)", human_bytes(self.min_large))
                    } else {
                        format!("Files over {} found by scanning", human_bytes(self.min_large))
                    };
                }
                _ => {}
            }
        }
        if !matches!(self.loc(), Loc::Large(_)) {
            self.pending = self.entries.iter().filter(|e| e.is_dir && e.size.is_none()).count();
        }
        if changed {
            self.recompute();
        }
        changed
    }

    pub fn current(&self) -> Option<&Entry> {
        self.visible.get(self.cursor).and_then(|&i| self.entries.get(i))
    }

    /// Re-filter and re-sort, keeping the cursor on the same item.
    fn recompute(&mut self) {
        let keep = self.current().map(|e| e.path.clone());
        // `entries` may have just been replaced wholesale; drop indexes into the old list.
        self.visible.clear();
        let q = self.filter.to_lowercase();
        let mut v: Vec<usize> =
            (0..self.entries.len()).filter(|&i| q.is_empty() || self.entries[i].name.to_lowercase().contains(&q)).collect();
        let e = &self.entries;
        match self.sort {
            Sort::Size => v.sort_by(|&a, &b| e[b].size.unwrap_or(0).cmp(&e[a].size.unwrap_or(0)).then(e[a].name.cmp(&e[b].name))),
            Sort::Name => v.sort_by(|&a, &b| e[a].name.to_lowercase().cmp(&e[b].name.to_lowercase())),
            Sort::Modified => v.sort_by(|&a, &b| e[b].modified.cmp(&e[a].modified).then(e[a].name.cmp(&e[b].name))),
        }
        self.visible = v;
        self.cursor = if self.moved {
            keep.and_then(|p| self.visible.iter().position(|&i| self.entries[i].path == p))
                .unwrap_or_else(|| self.cursor.min(self.visible.len().saturating_sub(1)))
        } else {
            0
        };
    }

    pub fn total(&self) -> u64 {
        // The overview mixes nested folders, so a grand total would double-count.
        if matches!(self.loc(), Loc::Overview) { 0 } else { self.entries.iter().filter_map(|e| e.size).sum() }
    }

    fn push(&mut self, loc: Loc) {
        self.stack.push(loc);
        self.message.clear();
        self.load();
    }

    fn back(&mut self) {
        if self.stack.len() <= 1 {
            self.quit = true;
            return;
        }
        self.stack.pop();
        self.message.clear();
        self.load();
    }

    fn targets(&self) -> Vec<PathBuf> {
        if !self.selected.is_empty() {
            self.selected.iter().cloned().collect()
        } else {
            self.current().map(|e| vec![e.path.clone()]).unwrap_or_default()
        }
    }

    pub fn handle(&mut self, key: Key) {
        if let Some((paths, _)) = self.confirm.clone() {
            // Only an explicit "y" deletes; anything else backs out.
            if matches!(key, Key::Char('y') | Key::Char('Y')) {
                self.trash(&paths);
            } else {
                self.message = "Cancelled".into();
            }
            self.confirm = None;
            return;
        }
        if self.filtering {
            match key {
                Key::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                }
                Key::Enter => self.filtering = false,
                Key::Backspace => {
                    self.filter.pop();
                }
                Key::Char(c) => self.filter.push(c),
                Key::CtrlC => self.quit = true,
                _ => {}
            }
            self.recompute();
            return;
        }
        let last = self.visible.len().saturating_sub(1);
        if matches!(key, Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::Home | Key::End | Key::Space | Key::Char('j' | 'k' | 'g' | 'G')) {
            self.moved = true;
        }
        match key {
            Key::CtrlC | Key::Char('q') | Key::Char('Q') => self.quit = true,
            Key::Up | Key::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            Key::Down | Key::Char('j') => self.cursor = (self.cursor + 1).min(last),
            Key::PageUp => self.cursor = self.cursor.saturating_sub(10),
            Key::PageDown => self.cursor = (self.cursor + 10).min(last),
            Key::Home | Key::Char('g') => self.cursor = 0,
            Key::End | Key::Char('G') => self.cursor = last,
            Key::Enter | Key::Right | Key::Char('l') => {
                if let Some(e) = self.current().cloned() {
                    if e.is_dir {
                        self.push(Loc::Dir(e.path));
                    } else {
                        self.message = "Not a folder (O opens it, F shows it in Finder)".into();
                    }
                }
            }
            Key::Esc | Key::Left | Key::Char('h') | Key::Backspace => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.recompute();
                } else {
                    self.back();
                }
            }
            Key::Space => {
                if let Some(p) = self.current().map(|e| e.path.clone()) {
                    if !self.selected.remove(&p) {
                        self.selected.insert(p);
                    }
                    self.cursor = (self.cursor + 1).min(last);
                }
            }
            Key::Char('/') => self.filtering = true,
            Key::Char('r') | Key::Char('R') => {
                // Forget what we measured under here so it is measured again.
                if let Loc::Dir(p) | Loc::Large(p) = self.loc().clone() {
                    self.cache.retain(|k, _| !k.starts_with(&p));
                } else {
                    self.cache.clear();
                }
                self.load();
                self.message = "Refreshing…".into();
            }
            Key::Char('s') | Key::Char('S') => {
                self.sort = match self.sort {
                    Sort::Size => Sort::Name,
                    Sort::Name => Sort::Modified,
                    Sort::Modified => Sort::Size,
                };
                self.recompute();
            }
            Key::Char('t') | Key::Char('T') => {
                let root = match self.loc().clone() {
                    Loc::Overview => self.home.clone(),
                    Loc::Dir(p) | Loc::Large(p) => p,
                };
                if !matches!(self.loc(), Loc::Large(_)) {
                    self.push(Loc::Large(root));
                }
            }
            Key::Char('o') | Key::Char('O') => self.external(&["open"], "Opened"),
            Key::Char('f') | Key::Char('F') => self.external(&["open", "-R"], "Shown in Finder"),
            Key::Char('p') | Key::Char('P') => self.preview(),
            Key::Delete | Key::Char('d') | Key::Char('D') => self.start_delete(),
            _ => {}
        }
    }

    fn external(&mut self, cmd: &[&str], done: &str) {
        let Some(e) = self.current() else { return };
        let path = e.path.to_string_lossy().to_string();
        let mut args: Vec<&str> = cmd[1..].to_vec();
        args.push(&path);
        let out = run("/usr/bin/open", &args, Duration::from_secs(10));
        self.message = if out.ok { done.to_string() } else { "Could not open it".to_string() };
    }

    fn preview(&mut self) {
        let Some(e) = self.current() else { return };
        // Quick Look blocks until closed, so detach it.
        let started = std::process::Command::new("/usr/bin/qlmanage")
            .args(["-p", &e.path.to_string_lossy()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok();
        self.message = if started { "Quick Look opened (close its window to continue)".into() } else { "Could not start Quick Look".into() };
    }

    /// Why this path may not be moved to the Trash from here.
    pub fn deletable(&self, path: &Path) -> Result<(), String> {
        if let Some(name) = path.strip_prefix(&self.home).ok().and_then(|r| r.to_str())
            && PROTECTED_HOME_DIRS.contains(&name) {
                return Err(format!("{name} is a system folder; open it and choose items inside"));
            }
        validate_in(&path.to_string_lossy(), &self.home, Mode::Uninstall).map_err(|e| {
            if e.starts_with("outside home") {
                "only items inside your home folder can be removed from here".to_string()
            } else {
                e
            }
        })
    }

    fn start_delete(&mut self) {
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        if let Some((p, why)) = targets.iter().find_map(|p| self.deletable(p).err().map(|w| (p, w))) {
            self.message = format!("Not removed: {} ({why})", p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
            return;
        }
        let bytes: u64 = targets
            .iter()
            .map(|p| self.entries.iter().find(|e| &e.path == p).and_then(|e| e.size).unwrap_or(0))
            .sum();
        self.confirm = Some((targets, bytes));
    }

    fn trash(&mut self, paths: &[PathBuf]) {
        let (mut moved, mut freed, mut failed) = (0usize, 0u64, Vec::new());
        for p in paths {
            // Re-check at the moment of removal.
            if let Err(why) = self.deletable(p) {
                failed.push(why);
                continue;
            }
            let size = self.entries.iter().find(|e| &e.path == p).and_then(|e| e.size).unwrap_or(0);
            match move_to_trash(p, &self.home) {
                Ok(_) => {
                    moved += 1;
                    freed += size;
                    self.entries.retain(|e| &e.path != p);
                    // Cached sizes of this item and everything above it are stale now.
                    self.cache.retain(|k, _| !(p.starts_with(k) || k.starts_with(p)));
                    self.selected.remove(p);
                }
                Err(e) => failed.push(e.to_string()),
            }
        }
        self.message = if failed.is_empty() {
            format!("Moved {moved} item(s) to the Trash ({}). Empty it to reclaim the space.", human_bytes(freed))
        } else {
            format!("Moved {moved}; {} failed: {}", failed.len(), failed[0])
        };
        self.recompute();
    }
}

// ---------------------------------------------------------------- drawing

fn level(bytes: u64) -> Color {
    const GB: u64 = 1 << 30;
    if bytes >= 5 * GB {
        Color::Red
    } else if bytes >= GB {
        Color::Yellow
    } else {
        Color::Green
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    let [head, list, foot] = Layout::vertical([Constraint::Length(3), Constraint::Min(3), Constraint::Length(2)]).areas(f.area());

    let title = match app.loc() {
        Loc::Overview => "Disk overview".to_string(),
        Loc::Dir(p) => p.display().to_string(),
        Loc::Large(p) => format!("Large files in {}", p.display()),
    };
    let scanning = if app.pending > 0 { format!("  {} scanning…", ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][app.tick / 2 % 10]) } else { String::new() };
    let total = match (app.loc(), app.disk) {
        (Loc::Overview, Some((cap, free))) => {
            format!("  ·  disk: {} used of {} · {} free", human_bytes(cap.saturating_sub(free)), human_bytes(cap), human_bytes(free))
        }
        _ if app.total() > 0 => format!("  ·  {} total", human_bytes(app.total())),
        _ => String::new(),
    };
    let sel = if app.selected.is_empty() { String::new() } else { format!("  ·  {} selected", app.selected.len()) };
    let filt = if app.filtering || !app.filter.is_empty() { format!("  /{}{}", app.filter, if app.filtering { "▏" } else { "" }) } else { String::new() };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![Span::styled(title, Style::default().add_modifier(Modifier::BOLD).fg(Color::Magenta))]),
            Line::from(format!(
                "{} item(s){total}{sel}  ·  sort: {}{scanning}",
                app.visible.len(),
                match app.sort {
                    Sort::Size => "size",
                    Sort::Name => "name",
                    Sort::Modified => "modified",
                }
            )),
            Line::from(Span::styled(filt, Style::default().fg(Color::Yellow))),
        ]),
        head,
    );

    // Overview rows overlap (Downloads is inside Home), so their sum is not a meaningful
    // denominator; there each row is a share of the whole disk instead.
    let known_total: u64 = match (app.loc(), app.disk) {
        (Loc::Overview, Some((cap, _))) => cap.max(1),
        _ => app.entries.iter().filter_map(|e| e.size).sum::<u64>().max(1),
    };
    let max = app.entries.iter().filter_map(|e| e.size).max().unwrap_or(1).max(1);
    let name_w = (list.width as usize).saturating_sub(40).clamp(10, 60);
    let items: Vec<ListItem> = app
        .visible
        .iter()
        .map(|&i| {
            let e = &app.entries[i];
            let mark = if app.selected.contains(&e.path) { "[x]" } else { "[ ]" };
            let (bar, size_txt, pct) = match e.size {
                Some(s) => {
                    let filled = ((s as f64 / max as f64) * 10.0).round() as usize;
                    let lower = if e.partial { "≥" } else { " " };
                    (
                        Span::styled(format!("{}{}", "█".repeat(filled), "░".repeat(10 - filled.min(10))), Style::default().fg(level(s))),
                        format!("{lower}{:>9}", human_bytes(s)),
                        format!("{:>4.0}%", s as f64 / known_total as f64 * 100.0),
                    )
                }
                None => (Span::styled("··········", Style::default().fg(Color::DarkGray)), format!("{:>10}", "…"), "     ".to_string()),
            };
            let name = if e.is_dir { format!("{}/", e.name) } else { e.name.clone() };
            let name = if name.chars().count() > name_w { format!("{}…", name.chars().take(name_w - 1).collect::<String>()) } else { name };
            ListItem::new(Line::from(vec![
                Span::raw(format!("{mark} ")),
                bar,
                Span::raw(format!(" {size_txt} {pct}  ")),
                Span::styled(name, if e.is_dir { Style::default().fg(Color::Cyan) } else { Style::default() }),
                // Files from different folders often share a name; show where this one lives.
                if matches!(app.loc(), Loc::Large(_)) {
                    Span::styled(format!("  {}", parent_display(&e.path, &app.home)), Style::default().fg(Color::DarkGray))
                } else {
                    Span::raw("")
                },
                if e.partial { Span::styled("  ⚠ some folders unreadable", Style::default().fg(Color::Yellow)) } else { Span::raw("") },
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(if app.visible.is_empty() { None } else { Some(app.cursor) });
    let empty = if app.visible.is_empty() && app.pending == 0 { " (empty) " } else { "" };
    f.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::TOP).title(empty))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        list,
        &mut state,
    );

    let status = if !app.message.is_empty() { app.message.clone() } else { app.note.clone() };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(status, Style::default().fg(Color::Yellow))),
            Line::from(Span::styled(
                "↑↓ move · enter open · space select · / filter · s sort · t large files · o open · p preview · f finder · del trash · r refresh · esc back · q quit",
                Style::default().fg(Color::DarkGray),
            )),
        ]),
        foot,
    );

    if let Some((paths, bytes)) = &app.confirm {
        let area = centered(f.area(), 60, 7);
        f.render_widget(Clear, area);
        let what = if paths.len() == 1 {
            paths[0].file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
        } else {
            format!("{} items", paths.len())
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(format!("Move {what} to the Trash?")),
                Line::from(Span::styled(format!("about {}", human_bytes(*bytes)), Style::default().fg(Color::Cyan))),
                Line::from(""),
                Line::from(Span::styled("y  yes, move it     any other key  cancel", Style::default().fg(Color::DarkGray))),
            ])
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title(" Confirm ")),
            area,
        );
    }
}

/// "~/Library/Caches/x" for the folder containing `path`.
fn parent_display(path: &Path, home: &Path) -> String {
    let parent = path.parent().unwrap_or(path);
    match parent.strip_prefix(home) {
        Ok(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => parent.display().to_string(),
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

pub fn run_tui(start: Option<PathBuf>, home: PathBuf, min_large: u64) -> Result<()> {
    let mut app = App::new(start, home, min_large);
    let mut term = ratatui::init();
    let result = (|| -> Result<()> {
        while !app.quit {
            app.poll();
            term.draw(|f| draw(f, &app))?;
            if event::poll(Duration::from_millis(80))?
                && let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    let key = match k.code {
                        KeyCode::Up => Key::Up,
                        KeyCode::Down => Key::Down,
                        KeyCode::PageUp => Key::PageUp,
                        KeyCode::PageDown => Key::PageDown,
                        KeyCode::Home => Key::Home,
                        KeyCode::End => Key::End,
                        KeyCode::Left => Key::Left,
                        KeyCode::Right => Key::Right,
                        KeyCode::Enter => Key::Enter,
                        KeyCode::Esc => Key::Esc,
                        KeyCode::Backspace => Key::Backspace,
                        KeyCode::Delete => Key::Delete,
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Key::CtrlC,
                        KeyCode::Char(' ') => Key::Space,
                        KeyCode::Char(c) => Key::Char(c),
                        _ => continue,
                    };
                    app.handle(key);
                }
        }
        Ok(())
    })();
    app.stop_scans();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Instant;

    /// A throwaway "home" with a known layout.
    fn rig(tag: &str) -> (PathBuf, PathBuf) {
        let home = std::env::temp_dir().join(format!("sweepr-tui-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let home = {
            fs::create_dir_all(&home).unwrap();
            home.canonicalize().unwrap()
        };
        let w = |p: &str, n: usize| {
            let p = home.join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, vec![b'x'; n]).unwrap();
        };
        w("proj/big/a.bin", 900_000);
        w("proj/small/b.bin", 50_000);
        w("proj/loose.bin", 300_000);
        w("proj/.hidden/c.bin", 10_000);
        (home.clone(), home.join("proj"))
    }

    fn settle(a: &mut App) {
        let t = Instant::now();
        while a.pending > 0 && t.elapsed() < Duration::from_secs(10) {
            a.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        a.poll();
    }

    fn names(a: &App) -> Vec<String> {
        a.visible.iter().map(|&i| a.entries[i].name.clone()).collect()
    }

    #[test]
    fn lists_biggest_first_once_measured() {
        let (home, proj) = rig("order");
        let mut a = App::new(Some(proj), home.clone(), 1 << 20);
        settle(&mut a);
        assert_eq!(names(&a), vec!["big", "loose.bin", "small", ".hidden"]);
        assert_eq!(a.pending, 0);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn navigates_into_folders_and_back_then_quits_at_the_top() {
        let (home, proj) = rig("nav");
        let mut a = App::new(Some(proj.clone()), home.clone(), 1 << 20);
        settle(&mut a);
        a.handle(Key::Enter); // "big"
        settle(&mut a);
        assert_eq!(a.loc(), &Loc::Dir(proj.join("big")));
        assert_eq!(names(&a), vec!["a.bin"]);
        a.handle(Key::Enter); // a file: stays put, explains
        assert!(a.message.contains("Not a folder"));
        a.handle(Key::Esc);
        assert_eq!(a.loc(), &Loc::Dir(proj.clone()));
        assert!(!a.quit);
        a.handle(Key::Esc);
        assert!(a.quit, "Esc at the first screen quits");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn cursor_follows_its_item_while_sizes_arrive() {
        let (home, proj) = rig("cursor");
        let mut a = App::new(Some(proj), home.clone(), 1 << 20);
        settle(&mut a);
        assert_eq!(a.cursor, 0, "an untouched view stays on the top row while sizes arrive");
        a.handle(Key::Down); // on "loose.bin"
        let before = a.current().unwrap().name.clone();
        a.recompute();
        assert_eq!(a.current().unwrap().name, before, "once moved, the cursor follows its item");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn filter_narrows_and_esc_clears() {
        let (home, proj) = rig("filter");
        let mut a = App::new(Some(proj), home.clone(), 1 << 20);
        settle(&mut a);
        a.handle(Key::Char('/'));
        for c in "SMA".chars() {
            a.handle(Key::Char(c));
        }
        assert_eq!(names(&a), vec!["small"]);
        a.handle(Key::Enter);
        assert!(!a.filtering && a.filter == "SMA");
        a.handle(Key::Esc); // clears filter first, does not navigate
        assert_eq!(a.visible.len(), 4);
        assert!(!a.quit);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn typing_in_the_filter_never_triggers_commands() {
        let (home, proj) = rig("typing");
        let mut a = App::new(Some(proj), home.clone(), 1 << 20);
        settle(&mut a);
        a.handle(Key::Char('/'));
        for c in "qdt".chars() {
            a.handle(Key::Char(c)); // quit, delete and large-files keys: all just text here
        }
        assert!(!a.quit && a.confirm.is_none());
        assert_eq!(a.filter, "qdt");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn sort_cycles_and_reorders() {
        let (home, proj) = rig("sort");
        let mut a = App::new(Some(proj), home.clone(), 1 << 20);
        settle(&mut a);
        a.handle(Key::Char('s'));
        assert_eq!(a.sort, Sort::Name);
        assert_eq!(names(&a)[0], ".hidden");
        a.handle(Key::Char('s'));
        a.handle(Key::Char('s'));
        assert_eq!(a.sort, Sort::Size);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn delete_needs_explicit_confirmation_and_goes_to_trash() {
        let (home, proj) = rig("trash");
        let mut a = App::new(Some(proj.clone()), home.clone(), 1 << 20);
        settle(&mut a);
        a.handle(Key::Down); // loose.bin
        a.handle(Key::Delete);
        assert!(a.confirm.is_some());
        a.handle(Key::Enter); // not "y": cancels
        assert!(a.confirm.is_none() && proj.join("loose.bin").exists());
        a.handle(Key::Delete);
        a.handle(Key::Char('y'));
        assert!(!proj.join("loose.bin").exists());
        assert!(home.join(".Trash/loose.bin").exists(), "must be recoverable from the Trash");
        assert!(!names(&a).contains(&"loose.bin".to_string()));
        assert!(a.message.contains("Trash"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn multi_select_deletes_all_selected() {
        let (home, proj) = rig("multi");
        let mut a = App::new(Some(proj.clone()), home.clone(), 1 << 20);
        settle(&mut a);
        a.handle(Key::Space); // big (moves to loose.bin)
        a.handle(Key::Space); // loose.bin
        assert_eq!(a.selected.len(), 2);
        a.handle(Key::Char('d'));
        assert_eq!(a.confirm.as_ref().unwrap().0.len(), 2);
        a.handle(Key::Char('y'));
        assert!(!proj.join("big").exists() && !proj.join("loose.bin").exists());
        assert!(proj.join("small/b.bin").exists(), "unselected items stay");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn protected_and_outside_paths_are_refused() {
        let (home, _) = rig("refuse");
        fs::create_dir_all(home.join("Library")).unwrap();
        fs::create_dir_all(home.join("Documents")).unwrap();
        let a = App::new(Some(home.clone()), home.clone(), 1 << 20);
        assert!(a.deletable(&home.join("Library")).is_err(), "system folder");
        assert!(a.deletable(&home.join("Documents")).is_err());
        assert!(a.deletable(&home).is_err(), "the home itself");
        assert!(a.deletable(Path::new("/etc/hosts")).is_err());
        assert!(a.deletable(Path::new("/Applications/Safari.app")).is_err());
        assert!(a.deletable(&home.join("proj")).is_ok());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn deleting_a_protected_folder_never_even_asks() {
        let (home, _) = rig("noprompt");
        fs::create_dir_all(home.join("Documents/x")).unwrap();
        fs::write(home.join("Documents/x/f"), "data").unwrap();
        let mut a = App::new(Some(home.clone()), home.clone(), 1 << 20);
        settle(&mut a);
        let i = a.visible.iter().position(|&i| a.entries[i].name == "Documents").unwrap();
        a.cursor = i;
        a.handle(Key::Delete);
        assert!(a.confirm.is_none());
        assert!(a.message.contains("Not removed"));
        assert!(home.join("Documents/x/f").exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn large_files_view_lists_files_over_the_threshold() {
        let (home, proj) = rig("large");
        let mut a = App::new(Some(proj), home.clone(), 200_000);
        settle(&mut a);
        a.handle(Key::Char('t'));
        assert!(matches!(a.loc(), Loc::Large(_)));
        let t = Instant::now();
        while a.pending > 0 && t.elapsed() < Duration::from_secs(30) {
            a.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        // Spotlight may not have indexed a fresh temp dir; the fallback walk must still work,
        // and anything listed must really be over the threshold.
        assert_eq!(a.pending, 0);
        assert!(a.entries.iter().all(|e| !e.is_dir && e.size.unwrap() >= 100_000));
        assert!(!a.note.is_empty());
        a.handle(Key::Esc);
        assert!(matches!(a.loc(), Loc::Dir(_)));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn overview_lists_existing_home_folders() {
        let (home, _) = rig("overview");
        fs::create_dir_all(home.join("Downloads")).unwrap();
        fs::write(home.join("Downloads/a"), vec![0u8; 100_000]).unwrap();
        let mut a = App::new(None, home.clone(), 1 << 20);
        settle(&mut a);
        assert_eq!(a.loc(), &Loc::Overview);
        let n = names(&a);
        assert!(n.contains(&"Home".to_string()) && n.contains(&"Downloads".to_string()));
        assert!(!n.contains(&"Movies".to_string()), "missing folders are not listed");
        assert_eq!(a.total(), 0, "overview rows overlap, so no grand total is shown");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn renders_at_many_sizes_without_panicking() {
        use ratatui::{Terminal, backend::TestBackend};
        let (home, proj) = rig("render");
        let mut a = App::new(Some(proj), home.clone(), 1 << 20);
        settle(&mut a);
        a.handle(Key::Space);
        for (w, h) in [(120, 40), (80, 24), (40, 8), (12, 4), (1, 1), (0, 0)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &a)).unwrap();
        }
        a.handle(Key::Down);
        a.handle(Key::Delete); // confirm modal
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        t.draw(|f| draw(f, &a)).unwrap();
        let text: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("big/") && text.contains("Confirm") && text.contains("Trash"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn parent_folder_is_shown_relative_to_home() {
        let h = Path::new("/Users/u");
        assert_eq!(parent_display(Path::new("/Users/u/a/b/f.bin"), h), "~/a/b");
        assert_eq!(parent_display(Path::new("/Users/u/f.bin"), h), "~");
        assert_eq!(parent_display(Path::new("/opt/x/f.bin"), h), "/opt/x");
    }
}
