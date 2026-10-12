//! Battery level, state and health. Absent on Macs without a battery.

use crate::util::run;
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Battery {
    pub percent: u8,
    /// "charging", "discharging", "charged", ...
    pub status: String,
    /// "5:26" while an estimate exists.
    pub time_left: Option<String>,
    /// Current full capacity as a share of the design capacity.
    pub capacity: Option<u8>,
    pub health: String,
    pub cycle_count: Option<u32>,
}

pub fn read() -> Option<Battery> {
    let pm = run("/usr/bin/pmset", &["-g", "batt"], Duration::from_secs(4));
    if !pm.ok {
        return None;
    }
    let (percent, status, time_left) = parse_pmset(&pm.stdout)?;
    let io = run("/usr/sbin/ioreg", &["-rn", "AppleSmartBattery", "-w0"], Duration::from_secs(4));
    let (cycle_count, capacity) = if io.ok { parse_ioreg(&io.stdout) } else { (None, None) };
    Some(Battery { percent, status, time_left, capacity, health: health_label(capacity), cycle_count })
}

fn health_label(capacity: Option<u8>) -> String {
    match capacity {
        Some(c) if c >= 80 => "Healthy",
        Some(c) if c >= 60 => "Fair",
        Some(_) => "Service soon",
        None => "Unknown",
    }
    .to_string()
}

/// "... 32%; discharging; 5:26 remaining present: true"
pub fn parse_pmset(text: &str) -> Option<(u8, String, Option<String>)> {
    let line = text.lines().find(|l| l.contains("InternalBattery"))?;
    let detail = line.split('\t').nth(1)?;
    let mut parts = detail.split(';').map(str::trim);
    let percent: u8 = parts.next()?.trim_end_matches('%').parse().ok()?;
    let status = parts.next()?.to_string();
    let time_left = parts
        .next()
        .and_then(|t| t.split_whitespace().next())
        .filter(|t| t.contains(':') && !t.starts_with("0:00"))
        .map(String::from);
    Some((percent.min(100), status, time_left))
}

/// (cycle count, health %) from the AppleSmartBattery registry entry.
pub fn parse_ioreg(text: &str) -> (Option<u32>, Option<u8>) {
    let num = |key: &str| -> Option<u64> {
        let pat = format!("\"{key}\"=");
        let compact = text.replace(" = ", "=");
        let i = compact.find(&pat)?;
        compact[i + pat.len()..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
    };
    let cycles = num("CycleCount").map(|c| c as u32);
    let design = num("DesignCapacity").filter(|d| *d > 0);
    // Apple Silicon reports the usable capacity as NominalChargeCapacity.
    let now = num("NominalChargeCapacity").or_else(|| num("AppleRawMaxCapacity"));
    let capacity = design.zip(now).map(|(d, n)| ((n as f64 / d as f64) * 100.0).round().min(100.0) as u8);
    (cycles, capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pmset_output() {
        let t = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=23003235)\t32%; discharging; 5:26 remaining present: true\n";
        assert_eq!(parse_pmset(t), Some((32, "discharging".into(), Some("5:26".into()))));
        let c = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1)\t100%; charged; 0:00 remaining present: true\n";
        assert_eq!(parse_pmset(c), Some((100, "charged".into(), None)));
        let est = " -InternalBattery-0 (id=1)\t57%; discharging; (no estimate) present: true\n";
        assert_eq!(parse_pmset(est).unwrap().2, None);
        assert_eq!(parse_pmset("Now drawing from 'AC Power'\n"), None, "a Mac without a battery");
    }

    #[test]
    fn parses_cycles_and_health() {
        let t = r#"| "CycleCount" = 153
| "BatteryData" = {"FullChargeCapacity"=4225,"NominalChargeCapacity"=4352,"DesignCapacity"=4629,"CurrentCapacity"=32}"#;
        assert_eq!(parse_ioreg(t), (Some(153), Some(94)));
        assert_eq!(parse_ioreg("nothing"), (None, None));
    }

    #[test]
    fn health_labels() {
        assert_eq!(health_label(Some(94)), "Healthy");
        assert_eq!(health_label(Some(70)), "Fair");
        assert_eq!(health_label(Some(40)), "Service soon");
        assert_eq!(health_label(None), "Unknown");
    }
}
