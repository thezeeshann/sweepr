//! The live dashboard: borderless "cards" in two columns, each a heading with a
//! dotted rule followed by label / bar / value rows.

use super::collect::{Collector, Snapshot, human_bytes};
use anyhow::Result;
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::time::{Duration, Instant};

use crate::theme::{DIM, GREEN, PURPLE, RED, TRACK, YELLOW, bar_colored, heading, level, rule};

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Cpu,
    Mem,
}

struct App {
    snap: Snapshot,
    sort: Sort,
    paused: bool,
}

pub fn run(top: usize, interval: Duration, now_playing: bool) -> Result<()> {
    let mut collector = Collector::new(top);
    collector.start_controls(now_playing);
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(300)));
    let mut app = App { snap: collector.collect(), sort: Sort::Cpu, paused: false };

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &mut collector, interval);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, collector: &mut Collector, interval: Duration) -> Result<()> {
    let mut next = Instant::now() + interval;
    loop {
        terminal.draw(|f| draw(f, app))?;
        let timeout = next.saturating_duration_since(Instant::now());
        if event::poll(timeout)?
            && let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                    KeyCode::Char('c') | KeyCode::Char('m') => {
                        let mem = k.code == KeyCode::Char('m');
                        app.sort = if mem { Sort::Mem } else { Sort::Cpu };
                        collector.sort_by_memory = mem;
                        // Re-rank immediately instead of waiting for the next tick.
                        app.snap = collector.collect();
                        next = Instant::now() + interval;
                    }
                    KeyCode::Char(' ') | KeyCode::Char('p') => app.paused = !app.paused,
                    _ => {}
                }
            }
        if Instant::now() >= next {
            if !app.paused {
                app.snap = collector.collect();
            }
            next = Instant::now() + interval;
        }
    }
}

// ------------------------------------------------------------------ layout

fn draw(f: &mut Frame, app: &App) {
    let [header, body, footer] = Layout::vertical([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).spacing(4).areas(body);
    let (lw, rw) = (left.width as usize, right.width as usize);
    let s = &app.snap;

    // Left: CPU, Disk, Network, then as many processes as fit.
    let mut l: Vec<Line> = Vec::new();
    l.extend(cpu_card(s, lw));
    l.push(Line::raw(""));
    l.extend(disk_card(s, lw));
    l.push(Line::raw(""));
    l.extend(network_card(s, lw));
    l.push(Line::raw(""));
    let proc_rows = (body.height as usize).saturating_sub(l.len() + 1).clamp(3, 15);
    l.extend(process_card(s, lw, proc_rows, app.sort));

    // Right: Memory, Power (if there is a battery), Controls.
    let mut r: Vec<Line> = Vec::new();
    r.extend(memory_card(s, rw));
    if let Some(b) = s.batteries.first() {
        r.push(Line::raw(""));
        r.extend(power_card(b, rw));
    }
    r.push(Line::raw(""));
    r.extend(controls_card(s, rw));

    f.render_widget(Paragraph::new(l), left);
    f.render_widget(Paragraph::new(r), right);

    f.render_widget(Paragraph::new(header_lines(s)), header);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("q quit · c/m sort · space pause{}", if app.paused { "   · PAUSED" } else { "" }),
            Style::default().fg(DIM),
        ))),
        footer,
    );
}

/// Who and what this Mac is: model, chip, memory, storage, macOS, uptime.
fn header_lines(s: &Snapshot) -> Vec<Line<'static>> {
    let h = &s.hardware;
    let cores = if s.cpu.p_core_count > 0 {
        format!("{} cores ({}P+{}E)", s.cpu.logical_cpu, s.cpu.p_core_count, s.cpu.e_core_count)
    } else {
        format!("{} cores", s.cpu.logical_cpu)
    };
    let model = if h.model_id.is_empty() || h.model_id == h.model { h.model.clone() } else { format!("{} ({})", h.model, h.model_id) };
    let sep = || Span::styled(" · ", Style::default().fg(DIM));
    let mut first = vec![Span::styled("sweepr", Style::default().fg(PURPLE).add_modifier(Modifier::BOLD)), Span::raw("  ")];
    let facts = [model, h.cpu_model.clone(), cores, format!("{} RAM", h.total_ram), format!("{} disk", h.disk_size)];
    for (i, f) in facts.into_iter().filter(|f| !f.trim().is_empty() && !f.starts_with(' ')).enumerate() {
        if i > 0 {
            first.push(sep());
        }
        first.push(Span::raw(f));
    }
    let os = if h.os_build.is_empty() { h.os_version.clone() } else { format!("{} ({})", h.os_version, h.os_build) };
    let second = format!("{} · {} · up {} · {} processes", s.host, os, uptime(s.uptime_seconds), s.procs);
    vec![Line::from(first), Line::from(Span::styled(second, Style::default().fg(DIM))), Line::raw("")]
}

// ------------------------------------------------------------------ cards

fn cpu_card(s: &Snapshot, w: usize) -> Vec<Line<'static>> {
    let bw = bar_width(w);
    let mut v = vec![heading("◉", "CPU", w), meter_row("Total", s.cpu.usage as f64, bw, String::new())];
    // The busiest cores, labelled by kind (efficiency cores enumerate first).
    let n = s.cpu.per_core.len();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| s.cpu.per_core[b].partial_cmp(&s.cpu.per_core[a]).unwrap_or(std::cmp::Ordering::Equal));
    for &i in order.iter().take(3) {
        v.push(meter_row(&format!("{}{i}", core_kind(i, s.cpu.e_core_count as usize, n)), s.cpu.per_core[i] as f64, bw, String::new()));
    }
    let topo = if s.cpu.p_core_count > 0 { format!(", {}P+{}E", s.cpu.p_core_count, s.cpu.e_core_count) } else { String::new() };
    v.push(text_row("Load", format!("{:.2} / {:.2} / {:.2}{topo}", s.cpu.load1, s.cpu.load5, s.cpu.load15)));
    v
}

/// On Apple Silicon the efficiency cores enumerate first.
fn core_kind(i: usize, e_cores: usize, total: usize) -> &'static str {
    if e_cores == 0 || e_cores >= total {
        "C"
    } else if i < e_cores {
        "E"
    } else {
        "P"
    }
}

fn memory_card(s: &Snapshot, w: usize) -> Vec<Line<'static>> {
    let m = &s.memory;
    let bw = bar_width(w);
    let swap_pct = if m.swap_total == 0 { 0.0 } else { m.swap_used as f64 / m.swap_total as f64 * 100.0 };
    vec![
        heading("◫", "Memory", w),
        meter_row("Used", m.used_percent, bw, String::new()),
        meter_row("Free", (100.0 - m.used_percent).max(0.0), bw, String::new()).with_calm(),
        meter_row("Swap", swap_pct, bw, format!(" {}/{}", short_bytes(m.swap_used), short_bytes(m.swap_total))),
        text_row("Total", format!("{} / {} · Avail {}", human_bytes(m.used), human_bytes(m.total), human_bytes(m.available))),
    ]
}

fn disk_card(s: &Snapshot, w: usize) -> Vec<Line<'static>> {
    let bw = bar_width(w);
    let mut v = vec![heading("▥", "Disk", w)];
    for (i, d) in s.disks.iter().take(3).enumerate() {
        let label = if d.external { "EXT".to_string() } else if i == 0 { "INTR".to_string() } else { short_mount(&d.mount) };
        v.push(meter_row(&label, d.used_percent, bw, format!(" {} used, {} free", short_bytes(d.used), short_bytes(d.total.saturating_sub(d.used)))));
        if i == 0 {
            v.push(text_row("Total", format!("{} · {}", short_bytes(d.total), d.fstype.to_uppercase())));
        }
    }
    v.push(Line::from(vec![
        label_span("I/O"),
        Span::styled(io_cells(s.disk_io.read_rate), Style::default().fg(GREEN)),
        Span::raw(format!(" R {} · ", rate(s.disk_io.read_rate))),
        Span::styled(io_cells(s.disk_io.write_rate), Style::default().fg(YELLOW)),
        Span::raw(format!(" W {}", rate(s.disk_io.write_rate))),
    ]));
    v
}

fn network_card(s: &Snapshot, w: usize) -> Vec<Line<'static>> {
    let h = &s.network_history;
    let sw = bar_width(w);
    let (rx, tx) = (h.rx_history.last().copied().unwrap_or(0.0), h.tx_history.last().copied().unwrap_or(0.0));
    let iface = s.network.first().map(|i| format!("{} · {}", i.name, if i.ip.is_empty() { "no address".to_string() } else { i.ip.clone() }));
    vec![
        heading("⇅", "Network", w),
        Line::from(vec![label_span("Down"), Span::styled(sparkline(&h.rx_history, sw), Style::default().fg(GREEN)), Span::raw(format!("  {}", rate(rx)))]),
        Line::from(vec![label_span("Up"), Span::styled(sparkline(&h.tx_history, sw), Style::default().fg(YELLOW)), Span::raw(format!("  {}", rate(tx)))]),
        text_row("Link", iface.unwrap_or_else(|| "no interface".into())),
    ]
}

fn process_card(s: &Snapshot, w: usize, rows: usize, sort: Sort) -> Vec<Line<'static>> {
    let bw = bar_width(w).min(20);
    let by = if sort == Sort::Cpu { "CPU" } else { "memory" };
    let mut v = vec![Line::from(vec![
        Span::styled("❋ Processes ", Style::default().fg(PURPLE).add_modifier(Modifier::BOLD)),
        Span::styled(format!("by {by} "), Style::default().fg(DIM)),
        Span::styled(rule(w.saturating_sub(14 + by.len())), Style::default().fg(TRACK)),
    ])];
    for (i, p) in s.top_processes.iter().take(rows.saturating_sub(1)).enumerate() {
        let mut spans = vec![label_span(&format!("#{}", i + 1))];
        spans.extend(bar(p.cpu as f64, bw));
        // label + bar + " 100.0%  " + "  512M  " leave the rest of the column for the name.
        let room = w.saturating_sub(LABEL_W + bw + 9 + 8).max(4);
        spans.push(Span::raw(format!(" {:>5.1}%  {:>6}  {}", p.cpu, short_bytes(p.memory_bytes), ellipsize(&p.name, room))));
        v.push(Line::from(spans));
    }
    v
}

fn power_card(b: &super::battery::Battery, w: usize) -> Vec<Line<'static>> {
    let bw = bar_width(w);
    let level_color = |p: f64| if p < 20.0 { RED } else if p < 50.0 { YELLOW } else { GREEN };
    let mut level = vec![label_span("Level")];
    level.extend(bar_colored(b.percent as f64, bw, level_color(b.percent as f64)));
    level.push(Span::raw(format!(" {:>5.1}%", b.percent as f64)));

    let mut v = vec![heading("◪", "Power", w), Line::from(level)];
    if let Some(c) = b.capacity {
        let mut health = vec![label_span("Health")];
        health.extend(bar_colored(c as f64, bw, level_color(c as f64)));
        health.push(Span::raw(format!(" {c:>5}%")));
        v.push(Line::from(health));
    }
    let mut status = capitalize(&b.status);
    if let Some(t) = &b.time_left {
        status.push_str(&format!(" · {t}"));
    }
    status.push_str(&format!(" · {}", b.health));
    if let Some(c) = b.cycle_count {
        status.push_str(&format!(" · {c} cycles"));
    }
    v.push(Line::from(Span::styled(status, Style::default().fg(DIM))));
    v
}

fn controls_card(s: &Snapshot, w: usize) -> Vec<Line<'static>> {
    let c = &s.controls;
    let bw = bar_width(w);
    let wifi = match c.wifi.on {
        None => "unknown".to_string(),
        Some(false) => "Off".to_string(),
        Some(true) => {
            let mut p = vec![match (&c.wifi.network, c.wifi.connected) {
                (Some(n), _) => format!("Connected to {n}"),
                (None, Some(true)) => "Connected".to_string(),
                _ => "Not connected".to_string(),
            }];
            if let Some(ip) = &c.wifi.ip {
                p.push(ip.clone());
            }
            p.join(" · ")
        }
    };
    let bt = match (c.bluetooth.on, c.bluetooth.connected.len()) {
        (None, _) => "loading…".to_string(),
        (Some(false), _) => "Off".to_string(),
        (Some(true), 0) => "On · no devices".to_string(),
        (Some(true), n) => {
            let shown: Vec<_> = c.bluetooth.connected.iter().take(2).cloned().collect();
            format!("On · {}{}", shown.join(", "), if n > 2 { format!(" +{}", n - 2) } else { String::new() })
        }
    };
    let focus = match c.focus_active {
        Some(true) => "On".to_string(),
        Some(false) => "Off".to_string(),
        None => "unknown (needs Full Disk Access)".to_string(),
    };
    let display = match c.display_brightness {
        Some(b) => meter_row("Display", b as f64, bw, String::new()),
        None => text_row("Display", "unavailable".into()),
    };
    let sound = match &c.sound {
        Some(snd) if snd.muted => text_row("Sound", format!("muted ({}%)", snd.volume)),
        Some(snd) => meter_row("Sound", snd.volume as f64, bw, String::new()),
        None => text_row("Sound", "no volume control on this output".into()),
    };
    let playing = match &c.now_playing {
        Some(n) => format!("{} — {} ({})", n.title, n.artist, n.app),
        None => "— (use --now-playing for Music/Spotify)".to_string(),
    };
    vec![
        heading("⚙", "Controls", w),
        state_row("Wi-Fi", c.wifi.on, wifi),
        state_row("Bluetooth", c.bluetooth.on, bt),
        state_row("AirDrop", c.airdrop.as_ref().map(|a| a != "Off"), c.airdrop.clone().unwrap_or_else(|| "unknown".into())),
        state_row("Focus", c.focus_active, focus),
        display,
        sound,
        text_row("Playing", playing),
    ]
}

// ------------------------------------------------------------------ pieces

const LABEL_W: usize = 10;

fn label_span(l: &str) -> Span<'static> {
    Span::raw(format!("{l:<LABEL_W$}"))
}

/// Bar width that leaves room for the label and the value text.
fn bar_width(w: usize) -> usize {
    w.saturating_sub(LABEL_W + 26).clamp(6, 22)
}

fn bar(pct: f64, cells: usize) -> Vec<Span<'static>> {
    bar_colored(pct, cells, level(pct))
}

/// label · bar · percent · optional extra text
fn meter_row(label: &str, pct: f64, cells: usize, extra: String) -> Line<'static> {
    let mut spans = vec![label_span(label)];
    spans.extend(bar(pct, cells));
    spans.push(Span::raw(format!(" {:>5.1}%{extra}", pct.clamp(0.0, 999.0))));
    Line::from(spans)
}

trait Calm {
    /// "Free" style rows: a high value is good, so never colour it as a warning.
    fn with_calm(self) -> Self;
}
impl Calm for Line<'static> {
    fn with_calm(mut self) -> Self {
        for sp in self.spans.iter_mut() {
            if sp.style.fg == Some(RED) || sp.style.fg == Some(YELLOW) {
                sp.style = sp.style.fg(GREEN);
            }
        }
        self
    }
}

fn text_row(label: &str, text: String) -> Line<'static> {
    Line::from(vec![label_span(label), Span::raw(text)])
}

/// On/off marker followed by text; an unknown state is shown with no marker at all.
fn state_row(label: &str, on: Option<bool>, text: String) -> Line<'static> {
    let (glyph, color) = match on {
        Some(true) => ("● ", GREEN),
        Some(false) => ("○ ", DIM),
        None => ("  ", DIM),
    };
    let text_style = if on.is_none() { Style::default().fg(DIM) } else { Style::default() };
    Line::from(vec![label_span(label), Span::styled(glyph, Style::default().fg(color)), Span::styled(text, text_style)])
}

fn sparkline(values: &[f64], width: usize) -> String {
    const G: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let tail = &values[values.len().saturating_sub(width)..];
    // A floor keeps an idle link flat instead of blowing up noise.
    let max = tail.iter().cloned().fold(0.05_f64, f64::max);
    let mut s: String = " ".repeat(width.saturating_sub(tail.len()));
    for v in tail {
        s.push(G[((v / max) * 7.0).round().clamp(0.0, 7.0) as usize]);
    }
    s
}

/// Five-cell activity meter on a log-ish scale.
fn io_cells(mbs: f64) -> String {
    let n = [0.01, 0.1, 1.0, 10.0, 100.0].iter().filter(|t| mbs >= **t).count();
    format!("{}{}", "▮".repeat(n), "▯".repeat(5 - n))
}

fn rate(mbs: f64) -> String {
    if mbs >= 1.0 {
        format!("{mbs:.1} MB/s")
    } else if mbs >= 0.01 {
        format!("{:.0} KB/s", mbs * 1024.0)
    } else {
        "0 MB/s".to_string()
    }
}

/// "323G", "4.7G", "258M": compact sizes for tight rows.
pub fn short_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 || v >= 100.0 { format!("{v:.0}{}", U[i]) } else { format!("{v:.1}{}", U[i]) }
}

fn short_mount(m: &str) -> String {
    if m == "/" { "/".into() } else { m.rsplit('/').next().unwrap_or(m).chars().take(8).collect::<String>().to_uppercase() }
}

/// Shorten to `max` characters with a trailing ellipsis.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>()) }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if d > 0 { format!("{d}d {h}h") } else { format!("{h}h {m}m") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::battery::Battery;
    use super::super::collect::{DiskInfo, NetIf, Proc};
    use super::super::controls::{Bluetooth, Sound, Wifi};
    use ratatui::{Terminal, backend::TestBackend};

    fn sample() -> App {
        let mut snap = Snapshot::default();
        snap.host = "host".into();
        snap.hardware.cpu_model = "Apple M4".into();
        snap.hardware.model = "MacBook Air".into();
        snap.hardware.model_id = "Mac16,12".into();
        snap.hardware.total_ram = "16.0 GB".into();
        snap.hardware.disk_size = "460.4 GB".into();
        snap.hardware.os_version = "macOS 27.0".into();
        snap.hardware.os_build = "26A428".into();
        snap.uptime_seconds = 45450;
        snap.procs = 675;
        snap.cpu.usage = 6.9;
        snap.cpu.per_core = vec![10.0, 55.0, 90.0, 100.0, 0.0, 33.3, 12.0, 75.0, 20.0, 5.0];
        snap.cpu.logical_cpu = 10;
        snap.cpu.p_core_count = 4;
        snap.cpu.e_core_count = 6;
        snap.memory.total = 16 << 30;
        snap.memory.used = 11 << 30;
        snap.memory.used_percent = 68.7;
        snap.memory.swap_total = 1 << 30;
        snap.memory.swap_used = 250 << 20;
        snap.disks = vec![DiskInfo { mount: "/".into(), total: 460 << 30, used: 323 << 30, used_percent: 70.0, fstype: "apfs".into(), ..Default::default() }];
        snap.network = vec![NetIf { name: "en0".into(), ip: "10.0.0.2".into(), ..Default::default() }];
        snap.network_history.rx_history = vec![0.0, 0.5, 2.0, 0.1];
        snap.network_history.tx_history = vec![0.0, 0.0, 0.2, 0.0];
        snap.batteries = vec![Battery { percent: 36, status: "discharging".into(), time_left: Some("3:50".into()), capacity: Some(94), health: "Healthy".into(), cycle_count: Some(153) }];
        snap.controls.wifi = Wifi { on: Some(true), connected: Some(true), network: None, ip: Some("10.0.0.2".into()) };
        snap.controls.bluetooth = Bluetooth { on: Some(true), connected: vec!["Keychron K2".into(), "MX".into(), "Buds".into()] };
        snap.controls.airdrop = Some("Off".into());
        snap.controls.display_brightness = Some(35.0);
        snap.controls.sound = Some(Sound { volume: 46, muted: false });
        snap.top_processes = (0..30).map(|i| Proc { pid: i, name: format!("proc{i}"), cpu: (30 - i) as f32 * 3.0, memory_bytes: (30 - i) as u64 * 1024 * 1024, ..Default::default() }).collect();
        App { snap, sort: Sort::Cpu, paused: false }
    }

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        t.backend().buffer().content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn renders_every_card_in_the_original_layout() {
        let text = render(&sample(), 140, 36);
        for needle in [
            "◉ CPU", "◫ Memory", "▥ Disk", "⇅ Network", "❋ Processes", "◪ Power", "⚙ Controls", "Total", "Load", "INTR", "APFS", "Swap",
            "Level", "Health", "Discharging", "153 cycles", "Healthy", "Down", "Up", "en0 · 10.0.0.2", "Wi-Fi", "Connected", "Bluetooth",
            "Keychron K2", "AirDrop", "Focus", "Display", "Sound", "#1", "proc0", "I/O",
        ] {
            assert!(text.contains(needle), "missing {needle:?}");
        }
        assert!(!text.contains('┌') && !text.contains('│'), "the original design has no boxes");
    }

    #[test]
    fn header_shows_the_mac_details() {
        let text = render(&sample(), 140, 36);
        for needle in [
            "sweepr", "MacBook Air (Mac16,12)", "Apple M4", "10 cores (4P+6E)", "16.0 GB RAM", "460.4 GB disk", "host",
            "macOS 27.0 (26A428)", "up 12h 37m", "675 processes",
        ] {
            assert!(text.contains(needle), "header is missing {needle:?}");
        }
        // keys stay in the footer
        assert!(text.contains("q quit"));
    }

    #[test]
    fn header_copes_with_missing_hardware_info() {
        let mut app = sample();
        app.snap.hardware = Default::default();
        let text = render(&app, 140, 36);
        assert!(text.contains("sweepr") && !text.contains("()"));
    }

    #[test]
    fn renders_at_every_size_without_panicking() {
        let app = sample();
        for (w, h) in [(200, 60), (140, 40), (100, 30), (80, 24), (40, 10), (20, 5), (10, 3), (1, 1), (0, 0)] {
            render(&app, w, h);
        }
    }

    #[test]
    fn power_card_is_hidden_on_a_mac_without_a_battery() {
        let mut app = sample();
        app.snap.batteries.clear();
        let text = render(&app, 140, 36);
        assert!(!text.contains("Power") && text.contains("Controls"));
    }

    #[test]
    fn unknown_values_are_not_shown_as_off() {
        let mut app = sample();
        app.snap.controls.focus_active = None;
        app.snap.controls.wifi.on = None;
        let text = render(&app, 140, 36);
        assert!(text.contains("needs Full Disk Access") && text.contains("unknown"));
    }

    #[test]
    fn more_processes_show_when_the_terminal_is_taller() {
        let count = |h| render(&sample(), 140, h).matches("proc").count();
        assert!(count(60) > count(32));
    }

    #[test]
    fn sparkline_scales_and_pads() {
        assert_eq!(sparkline(&[], 4), "    ");
        assert_eq!(sparkline(&[0.0, 0.0], 4), "  ▁▁");
        let s = sparkline(&[0.0, 1.0, 2.0], 3);
        assert_eq!(s.chars().last(), Some('█'));
        assert_eq!(sparkline(&[1.0; 10], 3).chars().count(), 3);
    }

    #[test]
    fn long_process_names_get_an_ellipsis_not_a_cut() {
        assert_eq!(ellipsize("Google Chrome Helper (Renderer)", 12), "Google Chro…");
        assert_eq!(ellipsize("short", 12), "short");
        let mut app = sample();
        app.snap.top_processes[0].name = "A very long process name that cannot fit in any column".into();
        assert!(render(&app, 100, 36).contains('…'));
    }

    #[test]
    fn compact_formatting() {
        assert_eq!(short_bytes(323 << 30), "323G");
        assert_eq!(short_bytes(5 << 29), "2.5G");
        assert_eq!(short_bytes(258 << 20), "258M");
        assert_eq!(rate(0.0), "0 MB/s");
        assert_eq!(rate(0.5), "512 KB/s");
        assert_eq!(rate(2.0), "2.0 MB/s");
        assert_eq!(io_cells(0.0), "▯▯▯▯▯");
        assert_eq!(io_cells(5.0), "▮▮▮▯▯");
        assert_eq!(uptime(45450), "12h 37m");
        assert_eq!(core_kind(0, 6, 10), "E");
        assert_eq!(core_kind(6, 6, 10), "P");
    }

    #[test]
    fn free_memory_is_never_coloured_as_a_warning() {
        // 95% free is good news; the generic bar colouring would paint it red.
        let line = meter_row("Free", 95.0, 10, String::new()).with_calm();
        assert!(line.spans.iter().all(|s| s.style.fg != Some(RED)));
    }
}
