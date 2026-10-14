//! Searchable multi-select list of installed apps.

use super::discover::App;
use crate::status::human_bytes;
use anyhow::Result;
use crate::theme::pal;
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::collections::BTreeSet;

pub struct Picker<'a> {
    apps: &'a [App],
    pub query: String,
    pub searching: bool,
    /// Indices into `apps` that match the query, in display order.
    pub visible: Vec<usize>,
    pub cursor: usize,
    pub selected: BTreeSet<usize>,
    pub done: Option<bool>,
}

pub enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Space,
    Enter,
    Esc,
    Char(char),
    Backspace,
    CtrlC,
}

impl<'a> Picker<'a> {
    pub fn new(apps: &'a [App]) -> Self {
        let mut p = Self { apps, query: String::new(), searching: false, visible: Vec::new(), cursor: 0, selected: BTreeSet::new(), done: None };
        p.refilter();
        p
    }

    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.visible = (0..self.apps.len())
            .filter(|&i| {
                q.is_empty()
                    || self.apps[i].name.to_lowercase().contains(&q)
                    || self.apps[i].bundle_id.as_deref().is_some_and(|b| b.to_lowercase().contains(&q))
            })
            .collect();
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
    }

    pub fn current(&self) -> Option<usize> {
        self.visible.get(self.cursor).copied()
    }

    pub fn selected_bytes(&self) -> u64 {
        self.selected.iter().map(|&i| self.apps[i].size).sum()
    }

    /// `done == Some(true)`: confirmed; `Some(false)`: cancelled.
    pub fn key(&mut self, k: Key) {
        let last = self.visible.len().saturating_sub(1);
        match k {
            Key::CtrlC => self.done = Some(false),
            Key::Esc if self.searching => {
                self.searching = false;
                self.query.clear();
                self.refilter();
            }
            Key::Esc => self.done = Some(false),
            Key::Enter if self.searching => self.searching = false,
            Key::Backspace if self.searching => {
                self.query.pop();
                self.refilter();
            }
            Key::Char(c) if self.searching => {
                self.query.push(c);
                self.refilter();
            }
            Key::Char('/') => self.searching = true,
            Key::Char('q') => self.done = Some(false),
            Key::Up | Key::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            Key::Down | Key::Char('j') => self.cursor = (self.cursor + 1).min(last),
            Key::PageUp => self.cursor = self.cursor.saturating_sub(10),
            Key::PageDown => self.cursor = (self.cursor + 10).min(last),
            Key::Space => {
                if let Some(i) = self.current() {
                    if !self.selected.remove(&i) {
                        self.selected.insert(i);
                    }
                    self.cursor = (self.cursor + 1).min(last);
                }
            }
            Key::Char('c') => self.selected.clear(),
            Key::Enter => {
                // Enter on nothing selected means "this one".
                if self.selected.is_empty()
                    && let Some(i) = self.current() {
                        self.selected.insert(i);
                    }
                self.done = Some(!self.selected.is_empty());
            }
            _ => {}
        }
    }
}

pub fn pick(apps: &[App]) -> Result<Vec<usize>> {
    let mut p = Picker::new(apps);
    let mut term = ratatui::init();
    let result = (|| -> Result<()> {
        while p.done.is_none() {
            term.draw(|f| draw(f, &p))?;
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                let key = match k.code {
                    KeyCode::Up => Key::Up,
                    KeyCode::Down => Key::Down,
                    KeyCode::PageUp => Key::PageUp,
                    KeyCode::PageDown => Key::PageDown,
                    KeyCode::Enter => Key::Enter,
                    KeyCode::Esc => Key::Esc,
                    KeyCode::Backspace => Key::Backspace,
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Key::CtrlC,
                    KeyCode::Char(' ') => Key::Space,
                    KeyCode::Char(c) => Key::Char(c),
                    _ => continue,
                };
                p.key(key);
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result?;
    Ok(if p.done == Some(true) { p.selected.into_iter().collect() } else { Vec::new() })
}

pub fn draw(f: &mut Frame, p: &Picker) {
    crate::theme::backdrop(f);
    let [head, list, foot] = Layout::vertical([Constraint::Length(2), Constraint::Min(3), Constraint::Length(1)]).areas(f.area());

    let search = if p.searching || !p.query.is_empty() {
        format!("  search: {}{}", p.query, if p.searching { "▏" } else { "" })
    } else {
        String::new()
    };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Uninstall apps", Style::default().add_modifier(Modifier::BOLD).fg(pal().title)),
                Span::raw(format!("  {} shown · {} selected ({})", p.visible.len(), p.selected.len(), human_bytes(p.selected_bytes()))),
            ]),
            Line::from(Span::styled(search, Style::default().fg(pal().search))),
        ]),
        head,
    );

    let items: Vec<ListItem> = p
        .visible
        .iter()
        .map(|&i| {
            let a = &p.apps[i];
            let mark = if p.selected.contains(&i) { "[x]" } else { "[ ]" };
            let style = if p.selected.contains(&i) { Style::default().fg(pal().select) } else { Style::default() };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{mark} "), style),
                Span::styled(format!("{:<34}", truncate(&a.name, 34)), style),
                Span::styled(format!("{:>10}  ", human_bytes(a.size)), Style::default().fg(pal().size)),
                Span::styled(format!("{:<12}", truncate(a.version.as_deref().unwrap_or(""), 12)), Style::default().fg(pal().faint)),
                Span::styled(a.bundle_id.clone().unwrap_or_default(), Style::default().fg(pal().faint)),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(if p.visible.is_empty() { None } else { Some(p.cursor) });
    f.render_stateful_widget(
        List::new(items).block(Block::default().borders(Borders::TOP)).highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        list,
        &mut state,
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            "↑↓ move · space select · / search · c clear · enter review · q quit",
            Style::default().fg(pal().faint),
        )),
        foot,
    );
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn app(name: &str, id: &str, size: u64) -> App {
        App { name: name.into(), path: PathBuf::from(format!("/Applications/{name}.app")), bundle_id: Some(id.into()), version: Some("1.0".into()), size }
    }
    fn apps() -> Vec<App> {
        vec![app("Alpha", "com.a.alpha", 100), app("Beta", "com.b.beta", 200), app("Gamma", "com.g.gamma", 400)]
    }

    #[test]
    fn selecting_and_confirming() {
        let a = apps();
        let mut p = Picker::new(&a);
        p.key(Key::Space); // selects Alpha, moves to Beta
        p.key(Key::Down);
        p.key(Key::Space); // selects Gamma
        assert_eq!(p.selected.iter().copied().collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(p.selected_bytes(), 500);
        p.key(Key::Enter);
        assert_eq!(p.done, Some(true));
    }

    #[test]
    fn enter_on_nothing_selected_picks_the_highlighted_app() {
        let a = apps();
        let mut p = Picker::new(&a);
        p.key(Key::Down);
        p.key(Key::Enter);
        assert_eq!(p.selected.iter().copied().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn search_filters_by_name_and_bundle_id_and_selection_survives() {
        let a = apps();
        let mut p = Picker::new(&a);
        p.key(Key::Space); // select Alpha
        p.key(Key::Char('/'));
        for c in "GAM".chars() {
            p.key(Key::Char(c));
        }
        assert_eq!(p.visible, vec![2]);
        p.key(Key::Enter); // leave search, keep filter
        p.key(Key::Char('/'));
        p.key(Key::Esc); // clears the filter
        assert_eq!(p.visible.len(), 3);
        assert!(p.selected.contains(&0), "selection kept across filtering");
        p.key(Key::Char('/'));
        for c in "com.b".chars() {
            p.key(Key::Char(c));
        }
        assert_eq!(p.visible, vec![1]);
    }

    #[test]
    fn typing_in_search_does_not_trigger_commands() {
        let a = apps();
        let mut p = Picker::new(&a);
        p.key(Key::Char('/'));
        p.key(Key::Char('q')); // must be text, not quit
        assert_eq!(p.done, None);
        assert_eq!(p.query, "q");
    }

    #[test]
    fn quit_and_empty_confirm_cancel() {
        let a = apps();
        let mut p = Picker::new(&a);
        p.key(Key::Char('q'));
        assert_eq!(p.done, Some(false));
        let empty: Vec<App> = Vec::new();
        let mut p = Picker::new(&empty);
        p.key(Key::Enter);
        assert_eq!(p.done, Some(false));
    }

    #[test]
    fn cursor_stays_in_bounds() {
        let a = apps();
        let mut p = Picker::new(&a);
        for _ in 0..20 {
            p.key(Key::Down);
        }
        assert_eq!(p.cursor, 2);
        for _ in 0..20 {
            p.key(Key::Up);
        }
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn renders_without_panicking() {
        use ratatui::{Terminal, backend::TestBackend};
        let a = apps();
        let mut p = Picker::new(&a);
        p.key(Key::Space);
        for (w, h) in [(100, 30), (40, 8), (10, 3), (0, 0)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &p)).unwrap();
        }
        let mut t = Terminal::new(TestBackend::new(100, 20)).unwrap();
        t.draw(|f| draw(f, &p)).unwrap();
        let text: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Alpha") && text.contains("[x]") && text.contains("1 selected"));
    }
}
