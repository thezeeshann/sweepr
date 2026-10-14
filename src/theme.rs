//! Color themes and the look shared by every command: one palette, dotted section
//! headings and block bars.
//!
//! The built-in `default` theme is the original 256-color look and paints no
//! background. The other themes are ports of Omarchy terminal themes: they bring
//! their own background and text color, so a full-screen view fills the terminal with
//! the theme and plain command output (`clean`, `uninstall`, `maintain`) uses its colors.
//!
//! The active theme is chosen by, in order: `--theme`, `$SWEEPR_THEME`, the saved
//! choice in `~/.config/sweepr/theme`, then `default`. `sw theme` manages the saved one.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Block,
};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
pub struct Palette {
    /// Headings, brand name, cursor, borders.
    pub accent: Color,
    pub good: Color,
    pub warn: Color,
    pub bad: Color,
    /// Secondary data such as sizes and rates.
    pub info: Color,
    /// Hints, labels, unknown values.
    pub dim: Color,
    /// The empty part of a bar and the dotted rules.
    pub track: Color,
    /// Background of the highlighted row.
    pub cursor_bg: Color,
    // The app picker keeps its own slots so the default theme looks exactly as before.
    pub title: Color,
    pub select: Color,
    pub search: Color,
    pub size: Color,
    pub faint: Color,
    /// Text and background of the theme; `None` keeps the terminal's own.
    pub text: Option<Color>,
    pub bg: Option<Color>,
}

pub struct Theme {
    pub id: &'static str,
    pub name: &'static str,
    pub about: &'static str,
    pub palette: Palette,
}

const fn hex(v: u32) -> Color {
    Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

#[allow(clippy::too_many_arguments)]
const fn themed(accent: u32, good: u32, warn: u32, bad: u32, info: u32, dim: u32, track: u32, cursor: u32, text: u32, bg: u32) -> Palette {
    Palette {
        accent: hex(accent),
        good: hex(good),
        warn: hex(warn),
        bad: hex(bad),
        info: hex(info),
        dim: hex(dim),
        track: hex(track),
        cursor_bg: hex(cursor),
        title: hex(accent),
        select: hex(good),
        search: hex(warn),
        size: hex(info),
        faint: hex(dim),
        text: Some(hex(text)),
        bg: Some(hex(bg)),
    }
}

/// Index 0 is the default theme.
pub const THEMES: [Theme; 5] = [
    Theme {
        id: "default",
        name: "Default",
        about: "The original sweepr look: 256-color purple on your terminal's own background",
        palette: Palette {
            accent: Color::Indexed(141),
            good: Color::Indexed(151),
            warn: Color::Indexed(221),
            bad: Color::Indexed(203),
            info: Color::Indexed(116),
            dim: Color::Indexed(244),
            track: Color::Indexed(238),
            cursor_bg: Color::Indexed(236),
            title: Color::Magenta,
            select: Color::Green,
            search: Color::Yellow,
            size: Color::Cyan,
            faint: Color::DarkGray,
            text: None,
            bg: None,
        },
    },
    Theme {
        id: "one-dark",
        name: "One Dark",
        about: "Atom's classic dark theme, with the brighter ANSI colors for bars and values",
        //                 accent    good     warn     bad      info     dim      track    cursor   text     bg
        palette: themed(0x61afef, 0xa9d98a, 0xf0d399, 0xff7b86, 0x66d9e8, 0x7f848e, 0x3e4451, 0x333842, 0xabb2bf, 0x282c34),
    },
    Theme {
        id: "one-dark-pro",
        name: "One Dark Pro",
        about: "The VS Code take on One Dark: same base, standard ANSI colors, softer surfaces",
        palette: themed(0x61afef, 0x98c379, 0xe5c07b, 0xe06c75, 0x56b6c2, 0x7a828e, 0x4b5263, 0x3e4451, 0xabb2bf, 0x282c34),
    },
    Theme {
        id: "arc-blueberry",
        name: "Arc Blueberry",
        about: "Deep blue-black with soft periwinkle accents and neon green and cyan",
        palette: themed(0x8eb0e6, 0x3cec85, 0xeacd61, 0xe35535, 0x69c3ff, 0x6f7594, 0x424761, 0x313b62, 0xbcc1dc, 0x111422),
    },
    Theme {
        id: "atelier",
        name: "Atelier",
        about: "Warm olive-black with earthy greens, ochre and clay",
        palette: themed(0x36a166, 0xa7c957, 0xc2b53c, 0xba6236, 0x5f9182, 0x7d7c6b, 0x3b3a2e, 0x302f27, 0xdcd9d6, 0x22221b),
    },
];

// ------------------------------------------------------------------ selection

static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static ADAPTED: OnceLock<Vec<Palette>> = OnceLock::new();

pub fn set_active(i: usize) {
    ACTIVE.store(i.min(THEMES.len() - 1), Ordering::Relaxed);
}

pub fn active_index() -> usize {
    ACTIVE.load(Ordering::Relaxed)
}

pub fn is_default() -> bool {
    active_index() == 0
}

/// Palette of theme `i`, converted to 256 colors when the terminal has no truecolor.
pub fn palette_of(i: usize) -> &'static Palette {
    &ADAPTED.get_or_init(|| THEMES.iter().map(|t| adapt(t.palette, truecolor())).collect())[i.min(THEMES.len() - 1)]
}

pub fn pal() -> &'static Palette {
    palette_of(active_index())
}

pub fn accent() -> Color {
    pal().accent
}
pub fn good() -> Color {
    pal().good
}
pub fn warn() -> Color {
    pal().warn
}
pub fn bad() -> Color {
    pal().bad
}
pub fn muted() -> Color {
    pal().dim
}
pub fn track() -> Color {
    pal().track
}
pub fn cursor_bg() -> Color {
    pal().cursor_bg
}

/// Lowercase, `_`/space to `-`, and drop the Omarchy `omarchy-` prefix and `-theme`
/// suffix so `omarchy-one-dark-theme`, `One Dark` and `one-dark` all match.
pub fn normalize(name: &str) -> String {
    let mut s = name.trim().to_lowercase().replace(['_', ' '], "-");
    if let Some(r) = s.strip_prefix("omarchy-") {
        s = r.to_string();
    }
    if let Some(r) = s.strip_suffix("-theme") {
        s = r.to_string();
    }
    s
}

pub fn find(name: &str) -> Option<usize> {
    let n = normalize(name);
    THEMES.iter().position(|t| t.id == n)
}

pub fn ids() -> String {
    THEMES.iter().map(|t| t.id).collect::<Vec<_>>().join(", ")
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Source {
    Flag,
    Env,
    Saved,
    Default,
}

impl Source {
    pub fn describe(self) -> &'static str {
        match self {
            Source::Flag => "from --theme",
            Source::Env => "from $SWEEPR_THEME",
            Source::Saved => "saved with `sw theme`",
            Source::Default => "the default",
        }
    }
}

#[derive(Debug)]
pub struct Choice {
    pub index: usize,
    pub source: Source,
    /// Set when a lower-priority setting named a theme that does not exist.
    pub warning: Option<String>,
}

/// Pure resolution of the four sources, in priority order. An unknown `--theme` is an
/// error; a stale env var or saved name falls through with a warning.
pub fn resolve(flag: Option<&str>, env: Option<&str>, saved: Option<&str>) -> Result<Choice, String> {
    if let Some(f) = flag {
        return match find(f) {
            Some(index) => Ok(Choice { index, source: Source::Flag, warning: None }),
            None => Err(format!("unknown theme `{f}`; choose one of: {}", ids())),
        };
    }
    let mut warning = None;
    if let Some(e) = env.filter(|e| !e.trim().is_empty()) {
        match find(e) {
            Some(index) => return Ok(Choice { index, source: Source::Env, warning: None }),
            None => warning = Some(format!("ignoring $SWEEPR_THEME=`{e}` (not a theme; choose one of: {})", ids())),
        }
    }
    if let Some(s) = saved {
        match find(s) {
            Some(index) => return Ok(Choice { index, source: Source::Saved, warning }),
            None => warning = Some(format!("ignoring saved theme `{}` (not a theme; run `sw theme`)", s.trim())),
        }
    }
    Ok(Choice { index: 0, source: Source::Default, warning })
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join(".config/sweepr/theme")
}

pub fn read_saved(home: &Path) -> Option<String> {
    let s = std::fs::read_to_string(config_path(home)).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

pub fn save(home: &Path, id: &str) -> std::io::Result<()> {
    let p = config_path(home);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(p, format!("{id}\n"))
}

/// Remove the saved choice. Returns whether there was one.
pub fn clear_saved(home: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(config_path(home)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Work out the theme from the flag, environment and saved choice and make it active.
pub fn init(flag: Option<&str>) -> anyhow::Result<Choice> {
    let home = dirs::home_dir();
    let saved = home.as_deref().and_then(read_saved);
    let env = std::env::var("SWEEPR_THEME").ok();
    let choice = resolve(flag, env.as_deref(), saved.as_deref()).map_err(anyhow::Error::msg)?;
    set_active(choice.index);
    if let Some(w) = &choice.warning {
        eprintln!("sw: {w}");
    }
    Ok(choice)
}

// ------------------------------------------------------------------ color depth

/// Terminal.app is the one common macOS terminal without truecolor.
fn truecolor() -> bool {
    let ct = std::env::var("COLORTERM").unwrap_or_default().to_lowercase();
    if ct.contains("truecolor") || ct.contains("24bit") {
        return true;
    }
    std::env::var("TERM_PROGRAM").map(|t| t != "Apple_Terminal").unwrap_or(true)
}

fn adapt(p: Palette, truecolor: bool) -> Palette {
    if truecolor {
        return p;
    }
    let c = |c: Color| match c {
        Color::Rgb(r, g, b) => Color::Indexed(rgb_to_256(r, g, b)),
        other => other,
    };
    let o = |c2: Option<Color>| c2.map(c);
    Palette {
        accent: c(p.accent),
        good: c(p.good),
        warn: c(p.warn),
        bad: c(p.bad),
        info: c(p.info),
        dim: c(p.dim),
        track: c(p.track),
        cursor_bg: c(p.cursor_bg),
        title: c(p.title),
        select: c(p.select),
        search: c(p.search),
        size: c(p.size),
        faint: c(p.faint),
        text: o(p.text),
        bg: o(p.bg),
    }
}

/// Nearest xterm-256 color: the better of the 6x6x6 cube and the 24-step gray ramp.
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let near = |v: u8| (0..6).min_by_key(|&i| (LEVELS[i] - v as i32).abs()).unwrap();
    let (ri, gi, bi) = (near(r), near(g), near(b));
    let cube = (16 + 36 * ri + 6 * gi + bi) as u8;
    let (cr, cg, cb) = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);

    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gi2 = ((avg - 8).max(0) / 10).min(23);
    let gv = 8 + 10 * gi2;
    let gray = (232 + gi2) as u8;

    let dist = |x: i32, y: i32, z: i32| (x - r as i32).pow(2) + (y - g as i32).pow(2) + (z - b as i32).pow(2);
    if dist(gv, gv, gv) < dist(cr, cg, cb) { gray } else { cube }
}

// ------------------------------------------------------------------ painting

/// Fill `area` with the theme's background and text color. A no-op for the default
/// theme, which uses the terminal's own.
pub fn fill(f: &mut Frame, area: Rect) {
    let p = pal();
    if p.bg.is_none() && p.text.is_none() {
        return;
    }
    let mut s = Style::default();
    if let Some(bg) = p.bg {
        s = s.bg(bg);
    }
    if let Some(fg) = p.text {
        s = s.fg(fg);
    }
    f.render_widget(Block::new().style(s), area);
}

/// Paint the whole screen; call first in every full-screen `draw`.
pub fn backdrop(f: &mut Frame) {
    fill(f, f.area());
}

/// "▥ Title ┈┈┈┈┈┈" filling `w` cells.
pub fn heading(icon: &str, title: &str, w: usize) -> Line<'static> {
    let head = format!("{icon} {title} ");
    let rest = w.saturating_sub(head.chars().count());
    Line::from(vec![Span::styled(head, Style::default().fg(accent()).add_modifier(Modifier::BOLD)), Span::styled(rule(rest), Style::default().fg(track()))])
}

pub fn rule(n: usize) -> String {
    "┈".repeat(n)
}

/// Good below 60 %, warning from 60 %, bad from 85 %.
pub fn level(pct: f64) -> Color {
    if pct >= 85.0 {
        bad()
    } else if pct >= 60.0 {
        warn()
    } else {
        good()
    }
}

pub fn bar_colored(pct: f64, cells: usize, color: Color) -> Vec<Span<'static>> {
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * cells as f64).round() as usize;
    vec![
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled("░".repeat(cells.saturating_sub(filled)), Style::default().fg(track())),
    ]
}

// ------------------------------------------------------------------ plain output

#[derive(Clone, Copy)]
pub enum Role {
    Good,
    Warn,
    Info,
    Dim,
}

/// SGR parameters for plain (non-TUI) output. The default theme keeps the original
/// 16-color codes so it follows the terminal's own scheme.
pub fn sgr(role: Role) -> String {
    if is_default() {
        return match role {
            Role::Good => "32",
            Role::Warn => "33",
            Role::Info => "34",
            Role::Dim => "90",
        }
        .to_string();
    }
    let p = pal();
    let c = match role {
        Role::Good => p.good,
        Role::Warn => p.warn,
        Role::Info => p.info,
        Role::Dim => p.dim,
    };
    fg_sgr(c)
}

pub fn fg_sgr(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("38;2;{r};{g};{b}"),
        Color::Indexed(n) => format!("38;5;{n}"),
        _ => "39".to_string(),
    }
}

pub fn bg_sgr(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("48;2;{r};{g};{b}"),
        Color::Indexed(n) => format!("48;5;{n}"),
        _ => "49".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(c: Color) -> (f64, f64, f64) {
        match c {
            Color::Rgb(r, g, b) => (r as f64, g as f64, b as f64),
            other => panic!("expected rgb, got {other:?}"),
        }
    }

    fn luminance(c: Color) -> f64 {
        let (r, g, b) = rgb(c);
        let f = |v: f64| {
            let v = v / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn ids_are_unique_and_default_is_first() {
        assert_eq!(THEMES[0].id, "default");
        for (i, t) in THEMES.iter().enumerate() {
            assert_eq!(THEMES.iter().position(|o| o.id == t.id), Some(i), "{}", t.id);
        }
    }

    #[test]
    fn default_theme_keeps_the_terminal_background() {
        assert!(THEMES[0].palette.bg.is_none() && THEMES[0].palette.text.is_none());
        assert!(THEMES[1..].iter().all(|t| t.palette.bg.is_some() && t.palette.text.is_some()));
    }

    #[test]
    fn omarchy_names_resolve() {
        for (name, id) in [
            ("omarchy-one-dark-theme", "one-dark"),
            ("omarchy-one-dark-pro-theme", "one-dark-pro"),
            ("omarchy-arc-blueberry", "arc-blueberry"),
            ("omarchy-atelier-theme", "atelier"),
            ("One Dark Pro", "one-dark-pro"),
            ("ARC_BLUEBERRY", "arc-blueberry"),
            ("  atelier  ", "atelier"),
            ("default", "default"),
        ] {
            assert_eq!(find(name).map(|i| THEMES[i].id), Some(id), "{name}");
        }
        assert_eq!(find("nope"), None);
        assert_eq!(find(""), None);
    }

    #[test]
    fn resolution_order_is_flag_env_saved_default() {
        let r = |f, e, s| resolve(f, e, s).unwrap();
        assert_eq!(r(Some("arc-blueberry"), Some("atelier"), Some("one-dark")).source, Source::Flag);
        assert_eq!(r(None, Some("atelier"), Some("one-dark")).index, find("atelier").unwrap());
        assert_eq!(r(None, None, Some("one-dark")).source, Source::Saved);
        assert_eq!(r(None, None, None).source, Source::Default);
        assert_eq!(r(None, Some("  "), None).source, Source::Default);
    }

    #[test]
    fn bad_names_error_for_the_flag_and_fall_through_otherwise() {
        assert!(resolve(Some("nope"), None, None).unwrap_err().contains("one-dark"));
        let c = resolve(None, Some("nope"), Some("atelier")).unwrap();
        assert_eq!((c.source, c.index), (Source::Saved, find("atelier").unwrap()));
        assert!(c.warning.unwrap().contains("SWEEPR_THEME"));
        let c = resolve(None, None, Some("gone")).unwrap();
        assert_eq!(c.source, Source::Default);
        assert!(c.warning.unwrap().contains("gone"));
    }

    #[test]
    fn saved_theme_round_trips_in_a_temp_home() {
        let home = std::env::temp_dir().join(format!("sweepr-theme-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(read_saved(&home), None);
        assert!(!clear_saved(&home).unwrap());
        save(&home, "arc-blueberry").unwrap();
        assert_eq!(read_saved(&home).as_deref(), Some("arc-blueberry"));
        save(&home, "atelier").unwrap();
        assert_eq!(read_saved(&home).as_deref(), Some("atelier"));
        assert!(clear_saved(&home).unwrap());
        assert_eq!(read_saved(&home), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn every_theme_is_readable_on_its_own_background() {
        for t in &THEMES[1..] {
            let p = &t.palette;
            let bg = p.bg.unwrap();
            assert!(contrast(p.text.unwrap(), bg) >= 6.0, "{} text", t.id);
            for (what, c) in [("accent", p.accent), ("good", p.good), ("warn", p.warn), ("bad", p.bad), ("info", p.info), ("dim", p.dim)] {
                assert!(contrast(c, bg) >= 3.0, "{} {what} {:.2}", t.id, contrast(c, bg));
            }
            // The empty bar and the highlighted row must show, yet stay below text.
            assert!(contrast(p.track, bg) >= 1.3, "{} track", t.id);
            assert!(contrast(p.cursor_bg, bg) >= 1.1, "{} cursor", t.id);
            assert!(contrast(p.text.unwrap(), p.cursor_bg) >= 4.5, "{} text on cursor row", t.id);
        }
    }

    #[test]
    fn severity_stays_distinguishable_in_every_theme() {
        for t in &THEMES {
            let p = &t.palette;
            assert!(p.good != p.warn && p.warn != p.bad && p.good != p.bad, "{}", t.id);
        }
    }

    #[test]
    fn rgb_maps_onto_the_256_palette() {
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 255, 255), 231);
        assert_eq!(rgb_to_256(255, 0, 0), 196);
        // Neutral grays use the gray ramp, not a tinted cube cell.
        assert!((232..=255).contains(&rgb_to_256(0x80, 0x80, 0x80)));
        assert!((232..=255).contains(&rgb_to_256(0x12, 0x12, 0x12)));
    }

    #[test]
    fn without_truecolor_every_color_becomes_indexed() {
        for t in &THEMES {
            let p = adapt(t.palette, false);
            for c in [p.accent, p.good, p.warn, p.bad, p.info, p.dim, p.track, p.cursor_bg, p.text.unwrap_or(Color::Reset), p.bg.unwrap_or(Color::Reset)] {
                assert!(!matches!(c, Color::Rgb(..)), "{}", t.id);
            }
        }
    }

    #[test]
    fn sgr_codes_are_well_formed() {
        assert_eq!(fg_sgr(Color::Rgb(1, 2, 3)), "38;2;1;2;3");
        assert_eq!(bg_sgr(Color::Rgb(1, 2, 3)), "48;2;1;2;3");
        assert_eq!(fg_sgr(Color::Indexed(141)), "38;5;141");
    }
}
