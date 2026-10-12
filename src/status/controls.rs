//! Control Center style state: Wi-Fi, Bluetooth, AirDrop, Focus, display
//! brightness, sound volume and (optionally) now playing.
//!
//! Read-only. Every field is an `Option` because macOS hides some of this from
//! unprivileged processes, and "unknown" must never be shown as "off":
//! - the Wi-Fi network name is redacted unless the app has Location access;
//! - Focus state lives in a TCC-protected folder (needs Full Disk Access);
//! - brightness uses the private DisplayServices framework and may vanish in
//!   a future macOS release.

use crate::util::run;
use serde::Serialize;
use std::ffi::CString;
use std::sync::OnceLock;
use std::time::Duration;

const T: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Controls {
    pub wifi: Wifi,
    pub bluetooth: Bluetooth,
    /// "Off", "Contacts Only" or "Everyone".
    pub airdrop: Option<String>,
    /// `None` when macOS will not let us read it.
    pub focus_active: Option<bool>,
    /// 0-100.
    pub display_brightness: Option<f32>,
    pub sound: Option<Sound>,
    pub now_playing: Option<NowPlaying>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Wifi {
    pub on: Option<bool>,
    pub connected: Option<bool>,
    /// `None` when redacted by macOS privacy.
    pub network: Option<String>,
    pub ip: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Bluetooth {
    pub on: Option<bool>,
    pub connected: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Sound {
    /// 0-100.
    pub volume: u8,
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NowPlaying {
    pub app: String,
    pub title: String,
    pub artist: String,
}

impl Controls {
    /// Quick items (each a few ms to ~100 ms).
    pub fn collect_fast(&mut self) {
        self.wifi = wifi();
        self.airdrop = airdrop();
        self.focus_active = focus_active();
        self.display_brightness = brightness();
        self.sound = sound();
    }

    /// Bluetooth needs `system_profiler`, which takes around a second.
    pub fn collect_slow(&mut self) {
        self.bluetooth = bluetooth();
    }

    pub fn collect_now_playing(&mut self) {
        self.now_playing = now_playing();
    }
}

// ---------- Wi-Fi ----------

fn wifi_device() -> Option<String> {
    let out = run("/usr/sbin/networksetup", &["-listallhardwareports"], T);
    parse_wifi_device(&out.stdout)
}

fn parse_wifi_device(text: &str) -> Option<String> {
    let mut lines = text.lines();
    while let Some(l) = lines.next() {
        if l.trim() == "Hardware Port: Wi-Fi" {
            return lines.next()?.trim().strip_prefix("Device: ").map(String::from);
        }
    }
    None
}

fn wifi() -> Wifi {
    let Some(dev) = wifi_device() else {
        return Wifi::default();
    };
    let power = run("/usr/sbin/networksetup", &["-getairportpower", &dev], T);
    let on = power.ok.then(|| power.stdout.trim_end().ends_with("On"));
    let ip = run("/usr/sbin/ipconfig", &["getifaddr", &dev], T);
    let ip = (ip.ok && !ip.stdout.trim().is_empty()).then(|| ip.stdout.trim().to_string());
    let net = run("/usr/sbin/networksetup", &["-getairportnetwork", &dev], T);
    Wifi {
        on,
        connected: on.map(|o| o && ip.is_some()),
        network: parse_network_name(&net.stdout),
        ip,
    }
}

/// "Current Wi-Fi Network: Name" → Some("Name"). Anything else, including macOS's
/// "not associated" reply when it withholds the SSID, is unknown.
fn parse_network_name(text: &str) -> Option<String> {
    let name = text.trim().strip_prefix("Current Wi-Fi Network: ")?.trim();
    (!name.is_empty() && name != "<redacted>").then(|| name.to_string())
}

// ---------- Bluetooth ----------

fn bluetooth() -> Bluetooth {
    let out = run("/usr/sbin/system_profiler", &["SPBluetoothDataType", "-json"], Duration::from_secs(15));
    if !out.ok {
        return Bluetooth::default();
    }
    parse_bluetooth(&out.stdout)
}

fn parse_bluetooth(json: &str) -> Bluetooth {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Bluetooth::default();
    };
    let Some(root) = v.get("SPBluetoothDataType").and_then(|a| a.get(0)) else {
        return Bluetooth::default();
    };
    let on = root
        .get("controller_properties")
        .and_then(|p| p.get("controller_state"))
        .and_then(|s| s.as_str())
        .map(|s| s == "attrib_on");
    let connected = root
        .get("device_connected")
        .and_then(|d| d.as_array())
        .map(|devs| {
            devs.iter()
                .filter_map(|d| d.as_object().and_then(|o| o.keys().next().cloned()))
                .collect()
        })
        .unwrap_or_default();
    Bluetooth { on, connected }
}

// ---------- AirDrop / Focus ----------

fn airdrop() -> Option<String> {
    let out = run("/usr/bin/defaults", &["read", "com.apple.sharingd", "DiscoverableMode"], T);
    // The key is absent until AirDrop's visibility was changed once; macOS's default is Contacts Only.
    Some(if out.ok { out.stdout.trim().to_string() } else { "Contacts Only".to_string() }).filter(|s| !s.is_empty())
}

fn focus_active() -> Option<bool> {
    let home = dirs::home_dir()?;
    let text = std::fs::read_to_string(home.join("Library/DoNotDisturb/DB/Assertions.json")).ok()?;
    parse_focus(&text)
}

fn parse_focus(json: &str) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let data = v.get("data")?.as_array()?;
    Some(data.iter().any(|d| d.get("storeAssertionRecords").and_then(|r| r.as_array()).is_some_and(|r| !r.is_empty())))
}

// ---------- Sound ----------

fn sound() -> Option<Sound> {
    let out = run("/usr/bin/osascript", &["-e", "get volume settings"], T);
    if !out.ok {
        return None;
    }
    parse_sound(&out.stdout)
}

fn parse_sound(text: &str) -> Option<Sound> {
    let field = |key: &str| text.split(',').find_map(|p| p.trim().strip_prefix(key)).map(str::trim);
    // "missing value" means the output device has no software volume (e.g. HDMI).
    let volume: u8 = field("output volume:")?.parse().ok()?;
    let muted = field("output muted:").is_some_and(|m| m == "true");
    Some(Sound { volume: volume.min(100), muted })
}

// ---------- Display brightness (private DisplayServices) ----------

type MainDisplayFn = unsafe extern "C" fn() -> u32;
type GetBrightnessFn = unsafe extern "C" fn(u32, *mut f32) -> i32;

fn brightness_fns() -> Option<(MainDisplayFn, GetBrightnessFn)> {
    static FNS: OnceLock<Option<(usize, usize)>> = OnceLock::new();
    let (a, b) = (*FNS.get_or_init(|| unsafe {
        let load = |path: &str, sym: &str| -> Option<usize> {
            let p = CString::new(path).ok()?;
            let h = libc::dlopen(p.as_ptr(), libc::RTLD_LAZY);
            if h.is_null() {
                return None;
            }
            let s = CString::new(sym).ok()?;
            let f = libc::dlsym(h, s.as_ptr());
            (!f.is_null()).then_some(f as usize)
        };
        Some((
            load("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics", "CGMainDisplayID")?,
            load("/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices", "DisplayServicesGetBrightness")?,
        ))
    }))?;
    // SAFETY: both symbols were resolved from the named frameworks with the
    // signatures documented/observed for them.
    unsafe { Some((std::mem::transmute::<usize, MainDisplayFn>(a), std::mem::transmute::<usize, GetBrightnessFn>(b))) }
}

fn brightness() -> Option<f32> {
    let (main_display, get) = brightness_fns()?;
    let mut v: f32 = -1.0;
    // SAFETY: `v` is a valid f32 out-pointer for the call.
    let rc = unsafe { get(main_display(), &mut v) };
    (rc == 0 && (0.0..=1.0).contains(&v)).then_some((v * 100.0).round())
}

// ---------- Now playing (Music / Spotify only) ----------

/// Queries Music and Spotify through AppleScript, only if they are running.
/// macOS asks once for permission to control them, which is why this is opt-in.
fn now_playing() -> Option<NowPlaying> {
    for app in ["Music", "Spotify"] {
        if !run("/usr/bin/pgrep", &["-x", app], T).ok {
            continue;
        }
        let script = format!(
            "tell application \"{app}\" to if player state is playing then return (name of current track) & \"\\n\" & (artist of current track)"
        );
        let out = run("/usr/bin/osascript", &["-e", &script], T);
        if out.ok {
            let mut it = out.stdout.lines();
            if let (Some(title), artist) = (it.next(), it.next().unwrap_or(""))
                && !title.trim().is_empty() {
                    return Some(NowPlaying { app: app.into(), title: title.trim().into(), artist: artist.trim().into() });
                }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_wifi_device() {
        let t = "Hardware Port: Ethernet\nDevice: en5\n\nHardware Port: Wi-Fi\nDevice: en0\nEthernet Address: aa\n";
        assert_eq!(parse_wifi_device(t).as_deref(), Some("en0"));
        assert_eq!(parse_wifi_device("Hardware Port: Ethernet\nDevice: en5\n"), None);
    }

    #[test]
    fn network_name_is_unknown_when_macos_withholds_it() {
        assert_eq!(parse_network_name("Current Wi-Fi Network: Home\n").as_deref(), Some("Home"));
        assert_eq!(parse_network_name("You are not associated with an AirPort network.\n"), None);
        assert_eq!(parse_network_name("Current Wi-Fi Network: <redacted>"), None);
    }

    #[test]
    fn parses_bluetooth_state_and_connected_devices() {
        let j = r#"{"SPBluetoothDataType":[{"controller_properties":{"controller_state":"attrib_on"},
            "device_connected":[{"Keychron K2":{"device_address":"x"}},{"MX Master 2S":{}}]}]}"#;
        let b = parse_bluetooth(j);
        assert_eq!(b.on, Some(true));
        assert_eq!(b.connected, vec!["Keychron K2", "MX Master 2S"]);
        let off = parse_bluetooth(r#"{"SPBluetoothDataType":[{"controller_properties":{"controller_state":"attrib_off"}}]}"#);
        assert_eq!((off.on, off.connected.len()), (Some(false), 0));
        assert_eq!(parse_bluetooth("garbage"), Bluetooth::default());
    }

    #[test]
    fn parses_sound_settings() {
        let s = parse_sound("output volume:46, input volume:93, alert volume:100, output muted:false").unwrap();
        assert_eq!((s.volume, s.muted), (46, false));
        assert!(parse_sound("output volume:0, input volume:93, alert volume:100, output muted:true").unwrap().muted);
        assert_eq!(parse_sound("output volume:missing value, output muted:missing value"), None);
    }

    #[test]
    fn focus_is_on_only_when_assertions_exist() {
        assert_eq!(parse_focus(r#"{"data":[{"storeAssertionRecords":[{"a":1}]}]}"#), Some(true));
        assert_eq!(parse_focus(r#"{"data":[{"storeAssertionRecords":[]}]}"#), Some(false));
        assert_eq!(parse_focus(r#"{"data":[{}]}"#), Some(false));
        assert_eq!(parse_focus("not json"), None);
    }

    /// Hits the real machine: whatever is available must be sane, never invented.
    #[test]
    fn live_values_are_in_range() {
        let mut c = Controls::default();
        c.collect_fast();
        if let Some(b) = c.display_brightness {
            assert!((0.0..=100.0).contains(&b));
        }
        if let Some(s) = c.sound {
            assert!(s.volume <= 100);
        }
        assert!(c.airdrop.is_some());
    }
}
