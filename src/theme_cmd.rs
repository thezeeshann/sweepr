//! `sw theme`: see which theme is active, preview the others live and save a choice.
//!
//! The choice only changes how sweepr itself looks; it never touches the terminal's own
//! settings. It is saved in `~/.config/sweepr/theme`.

use crate::theme::{self, THEMES, Source, bar_colored, bg_sgr, heading, level, pal, palette_of};
use anyhow::{Result, bail};
use clap::{Args as ClapArgs, Subcommand};
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(subcommand)]
    action: Option<Action>,
}

#[derive(Subcommand, Debug)]
enum Action {
    /// List the themes with a color swatch and mark the active one
    List,
    /// Show the active theme and where the choice comes from
    Current,
    /// Save a theme (e.g. `sw theme set atelier`)
    Set {
        /// One of the listed theme names; `omarchy-atelier-theme` style names work too
        name: String,
    },
    /// Forget the saved theme and go back to the default
    Reset,
}

/// What `init` decided; `sw theme` reports on it.
pub fn run_cmd(args: Args, dry_run: bool, choice: theme::Choice) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot find your home folder"))?;
    match args.action {
        Some(Action::List) => list(&choice),
        Some(Action::Current) => current(&choice),
        Some(Action::Set { name }) => set(&home, &name, dry_run, &choice),
        Some(Action::Reset) => reset(&home, dry_run),
        None if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() => pick(home, dry_run, &choice),
        None => list(&choice),
    }
}

fn colors_on() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Seven colored cells showing a theme's accent, severity colors, values and surfaces.
fn swatch(i: usize, on: bool) -> String {
    let p = palette_of(i);
    let cells = [p.accent, p.good, p.warn, p.bad, p.info, p.dim, p.track];
    if !on {
        return String::new();
    }
    let mut s: String = cells.iter().map(|&c| format!("\x1b[{}m  \x1b[0m", bg_sgr(c))).collect();
    s.push(' ');
    s
}

fn list(choice: &theme::Choice) -> Result<()> {
    let on = colors_on();
    println!("Themes (ports of Omarchy terminal themes):\n");
    for (i, t) in THEMES.iter().enumerate() {
        let mark = if i == choice.index { "●" } else { " " };
        println!("  {mark} {:<14} {}{}", t.id, swatch(i, on), t.about);
    }
    println!();
    current(choice)?;
    println!("\nSave one:    sw theme set <name>");
    println!("Try one:     sw --theme <name> status      (or SWEEPR_THEME=<name>)");
    println!("Pick live:   sw theme                      (interactive preview)");
    Ok(())
}

fn current(choice: &theme::Choice) -> Result<()> {
    let t = &THEMES[choice.index];
    println!("Active theme: {} ({}, {})", t.name, t.id, choice.source.describe());
    if choice.source != Source::Saved
        && let Some(saved) = dirs::home_dir().and_then(|h| theme::read_saved(&h))
    {
        println!("Saved theme:  {saved}");
    }
    Ok(())
}

fn set(home: &std::path::Path, name: &str, dry_run: bool, choice: &theme::Choice) -> Result<()> {
    let Some(i) = theme::find(name) else {
        bail!("unknown theme `{name}`; choose one of: {}", theme::ids());
    };
    let t = &THEMES[i];
    if dry_run {
        println!("Dry run: would save theme {} to {} (nothing written)", t.id, theme::config_path(home).display());
        return Ok(());
    }
    theme::save(home, t.id)?;
    println!("✓ Theme set to {} ({}). It applies to every sw command.", t.name, t.id);
    override_note(choice);
    Ok(())
}

fn override_note(choice: &theme::Choice) {
    if matches!(choice.source, Source::Flag | Source::Env) {
        println!("  Note: this shell uses {} ({}), which takes priority over the saved theme.", THEMES[choice.index].id, choice.source.describe());
    }
}

fn reset(home: &std::path::Path, dry_run: bool) -> Result<()> {
    if dry_run {
        println!("Dry run: would forget the saved theme (nothing changed)");
        return Ok(());
    }
    if theme::clear_saved(home)? {
        println!("✓ Saved theme removed. sw uses the default theme again.");
    } else {
        println!("No saved theme; sw already uses the default.");
    }
    Ok(())
}

// ------------------------------------------------------------ interactive picker

struct Picker {
    cursor: usize,
    /// What was active when we started; restored on cancel.
    start: usize,
    dry_run: bool,
    /// `Some(true)` = save, `Some(false)` = cancel.
    done: Option<bool>,
}

fn pick(home: PathBuf, dry_run: bool, choice: &theme::Choice) -> Result<()> {
    let mut p = Picker { cursor: choice.index, start: choice.index, dry_run, done: None };
    let mut term = ratatui::init();
    let result = (|| -> Result<()> {
        while p.done.is_none() {
            theme::set_active(p.cursor);
            term.draw(|f| draw(f, &p))?;
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => p.cursor = (p.cursor + THEMES.len() - 1) % THEMES.len(),
                    KeyCode::Down | KeyCode::Char('j') => p.cursor = (p.cursor + 1) % THEMES.len(),
                    KeyCode::Home => p.cursor = 0,
                    KeyCode::End => p.cursor = THEMES.len() - 1,
                    KeyCode::Enter => p.done = Some(true),
                    KeyCode::Esc | KeyCode::Char('q') => p.done = Some(false),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => p.done = Some(false),
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result?;

    theme::set_active(p.start);
    if p.done != Some(true) {
        println!("Cancelled: theme unchanged.");
        return Ok(());
    }
    let t = &THEMES[p.cursor];
    if p.dry_run {
        println!("Dry run: would save theme {} to {} (nothing written)", t.id, theme::config_path(&home).display());
        return Ok(());
    }
    theme::save(&home, t.id)?;
    theme::set_active(p.cursor);
    println!("✓ Theme set to {} ({}). It applies to every sw command.", t.name, t.id);
    override_note(choice);
    Ok(())
}

fn draw(f: &mut Frame, p: &Picker) {
    theme::backdrop(f);
    let [head, body, foot] = Layout::vertical([Constraint::Length(3), Constraint::Min(8), Constraint::Length(2)]).areas(f.area());
    let [left, right] = Layout::horizontal([Constraint::Length(30), Constraint::Min(30)]).spacing(3).areas(body);
    let dim = Style::default().fg(pal().dim);

    let mut top = vec![Span::styled("sweepr", Style::default().fg(pal().accent).add_modifier(Modifier::BOLD)), Span::styled("  theme", Style::default().add_modifier(Modifier::BOLD))];
    if p.dry_run {
        top.push(Span::styled("  DRY RUN · nothing will be saved", Style::default().fg(pal().warn)));
    }
    let saved = dirs::home_dir().and_then(|h| theme::read_saved(&h)).unwrap_or_else(|| "none (default)".into());
    f.render_widget(
        Paragraph::new(vec![Line::from(top), Line::from(Span::styled(format!("Saved: {saved}   ·   Previewing: {}", THEMES[p.cursor].id), dim))]),
        head,
    );

    // Theme list; the highlighted row is the one being previewed.
    let w = left.width as usize;
    let mut rows: Vec<Line> = vec![heading("◆", "Themes", w)];
    for (i, t) in THEMES.iter().enumerate() {
        let here = i == p.cursor;
        let mut spans = vec![Span::styled(if here { "▸ " } else { "  " }, Style::default().fg(pal().accent).add_modifier(Modifier::BOLD))];
        let pad = format!("{:<w$}", t.name, w = w.saturating_sub(2));
        spans.push(Span::styled(pad, if here { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() }));
        let line = Line::from(spans);
        rows.push(if here { line.style(Style::default().bg(pal().cursor_bg)) } else { line });
    }
    rows.push(Line::raw(""));
    rows.push(Line::from(Span::styled(THEMES[p.cursor].about, dim)));
    f.render_widget(Paragraph::new(rows).wrap(ratatui::widgets::Wrap { trim: true }), left);

    f.render_widget(Paragraph::new(preview(right.width as usize)).block(Block::default().borders(Borders::NONE)), right);
    f.render_widget(Paragraph::new(vec![Line::raw(""), Line::from(Span::styled("↑↓ choose · enter save · q cancel", dim))]), foot);
}

/// A miniature dashboard drawn with the same helpers the real views use.
fn preview(w: usize) -> Vec<Line<'static>> {
    let q = pal();
    let dim = Style::default().fg(q.dim);
    let label = |s: &str| Span::styled(format!("{s:<8}"), dim);
    let bar = |label_: &str, pct: f64, extra: &str| {
        let mut v = vec![label(label_)];
        v.extend(bar_colored(pct, 18, level(pct)));
        v.push(Span::raw(format!("  {pct:>3.0}%{extra}")));
        Line::from(v)
    };
    let mut v = vec![
        heading("▥", "CPU", w),
        bar("Total", 34.0, ""),
        bar("Core 1", 72.0, ""),
        Line::raw(""),
        heading("▤", "Disk", w),
        bar("Macintosh", 91.0, "  of 460 GB"),
        Line::from(vec![label("Read"), Span::styled("▁▂▃▅▆▇▅▃▂", Style::default().fg(q.good)), Span::styled("  1.2 MB/s", dim)]),
        Line::from(vec![label("Write"), Span::styled("▁▁▂▂▃▂▁▁▂", Style::default().fg(q.warn)), Span::styled("  340 KB/s", dim)]),
        Line::raw(""),
        heading("❋", "Processes", w),
    ];
    for (i, (name, cpu)) in [("WindowServer", "12.4%"), ("Safari", "8.1%"), ("node", "3.0%")].into_iter().enumerate() {
        let line = Line::from(vec![Span::raw(format!("{name:<16}")), Span::styled(format!("{cpu:>7}"), Style::default().fg(q.info))]);
        v.push(if i == 1 { line.style(Style::default().bg(q.cursor_bg)) } else { line });
    }
    v.push(Line::raw(""));
    v.push(Line::from(vec![
        Span::styled("✓ ok  ", Style::default().fg(q.good)),
        Span::styled("⚠ warning  ", Style::default().fg(q.warn)),
        Span::styled("✗ critical  ", Style::default().fg(q.bad)),
        Span::styled("hint", dim),
    ]));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(i: usize, w: u16, h: u16) -> String {
        theme::set_active(i);
        let p = Picker { cursor: i, start: 0, dry_run: false, done: None };
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, &p)).unwrap();
        let out = t.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>();
        theme::set_active(0);
        out
    }

    #[test]
    fn every_theme_renders_at_many_sizes_without_panicking() {
        for i in 0..THEMES.len() {
            for (w, h) in [(120, 40), (80, 24), (50, 12), (20, 5), (1, 1)] {
                render(i, w, h);
            }
        }
    }

    #[test]
    fn the_picker_lists_every_theme_and_the_preview() {
        let out = render(0, 120, 40);
        for t in &THEMES {
            assert!(out.contains(t.name), "{}", t.name);
        }
        assert!(out.contains("Processes") && out.contains("Previewing"));
    }

    #[test]
    fn themed_screens_fill_the_background_and_default_does_not() {
        for (i, th) in THEMES.iter().enumerate() {
            theme::set_active(i);
            let mut t = Terminal::new(TestBackend::new(40, 10)).unwrap();
            t.draw(theme::backdrop).unwrap();
            let want = palette_of(i).bg.unwrap_or(ratatui::style::Color::Reset);
            let all = t.backend().buffer().content().iter().all(|c| c.bg == want);
            theme::set_active(0);
            assert!(all, "{}: every cell should carry the theme background", th.id);
        }
    }

}
