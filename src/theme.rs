//! Look shared by the full-screen views (`status`, `analyze`): one palette, dotted
//! section headings and block bars.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

// 256-color palette (renders in Terminal.app as well as truecolor terminals).
pub const PURPLE: Color = Color::Indexed(141);
pub const GREEN: Color = Color::Indexed(151);
pub const YELLOW: Color = Color::Indexed(221);
pub const RED: Color = Color::Indexed(203);
pub const DIM: Color = Color::Indexed(244);
pub const TRACK: Color = Color::Indexed(238);

/// "▥ Title ┈┈┈┈┈┈" filling `w` cells.
pub fn heading(icon: &str, title: &str, w: usize) -> Line<'static> {
    let head = format!("{icon} {title} ");
    let rest = w.saturating_sub(head.chars().count());
    Line::from(vec![Span::styled(head, Style::default().fg(PURPLE).add_modifier(Modifier::BOLD)), Span::styled(rule(rest), Style::default().fg(TRACK))])
}

pub fn rule(n: usize) -> String {
    "┈".repeat(n)
}

/// Green below 60 %, yellow from 60 %, red from 85 %.
pub fn level(pct: f64) -> Color {
    if pct >= 85.0 {
        RED
    } else if pct >= 60.0 {
        YELLOW
    } else {
        GREEN
    }
}

pub fn bar_colored(pct: f64, cells: usize, color: Color) -> Vec<Span<'static>> {
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * cells as f64).round() as usize;
    vec![
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled("░".repeat(cells.saturating_sub(filled)), Style::default().fg(TRACK)),
    ]
}
