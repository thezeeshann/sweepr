use super::battery::{self, Battery};
use super::controls::Controls;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{Disks, Networks, ProcessesToUpdate, System};

const HISTORY_LEN: usize = 120;
const MB: f64 = 1024.0 * 1024.0;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Snapshot {
    pub collected_at_unix: u64,
    pub host: String,
    pub platform: String,
    pub uptime_seconds: u64,
    pub procs: usize,
    pub hardware: Hardware,
    pub cpu: Cpu,
    pub memory: Memory,
    pub disks: Vec<DiskInfo>,
    pub disk_io: DiskIo,
    pub network: Vec<NetIf>,
    pub network_history: NetHistory,
    pub batteries: Vec<Battery>,
    pub controls: Controls,
    pub top_processes: Vec<Proc>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Hardware {
    /// Marketing name, e.g. "MacBook Air".
    pub model: String,
    /// Identifier, e.g. "Mac16,12".
    pub model_id: String,
    pub cpu_model: String,
    pub total_ram: String,
    pub disk_size: String,
    pub os_version: String,
    /// e.g. "26A428".
    pub os_build: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Cpu {
    pub usage: f32,
    pub per_core: Vec<f32>,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub core_count: usize,
    pub logical_cpu: usize,
    pub p_core_count: u32,
    pub e_core_count: u32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Memory {
    pub used: u64,
    pub total: u64,
    pub available: u64,
    pub used_percent: f64,
    pub swap_used: u64,
    pub swap_total: u64,
    /// File-backed pages (reclaimable cache).
    pub cached: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DiskInfo {
    pub mount: String,
    pub device: String,
    pub used: u64,
    pub total: u64,
    pub used_percent: f64,
    pub fstype: String,
    pub external: bool,
}

/// Throughput in MB/s since the previous sample.
#[derive(Debug, Clone, Serialize, Default)]
pub struct DiskIo {
    pub read_rate: f64,
    pub write_rate: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct NetIf {
    pub name: String,
    pub rx_rate_mbs: f64,
    pub tx_rate_mbs: f64,
    pub ip: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct NetHistory {
    pub rx_history: Vec<f64>,
    pub tx_history: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub command: String,
    pub cpu: f32,
    pub memory: f64,
    pub memory_bytes: u64,
}

pub struct Collector {
    sys: System,
    nets: Networks,
    disks: Disks,
    top_n: usize,
    /// Rank processes by memory instead of CPU before truncating to `top_n`.
    pub sort_by_memory: bool,
    last_net: HashMap<String, (u64, u64)>,
    last_net_at: Instant,
    last_io: Option<(Instant, u64, u64)>,
    rx_hist: VecDeque<f64>,
    tx_hist: VecDeque<f64>,
    hardware: Hardware,
    controls: Arc<Mutex<Controls>>,
    batteries: Arc<Mutex<Vec<Battery>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Collector {
    pub fn new(top_n: usize) -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        let nets = Networks::new_with_refreshed_list();
        let disks = Disks::new_with_refreshed_list();

        // Prime the baselines so the first real sample yields a rate.
        let last_net = nets
            .iter()
            .map(|(n, d)| (n.clone(), (d.total_received(), d.total_transmitted())))
            .collect();
        let last_io = read_disk_io_counters().map(|(r, w)| (Instant::now(), r, w));
        let hardware = hardware_info(&sys, &disks);

        Self {
            sys,
            nets,
            disks,
            top_n,
            sort_by_memory: false,
            last_net,
            last_net_at: Instant::now(),
            last_io,
            rx_hist: VecDeque::new(),
            tx_hist: VecDeque::new(),
            hardware,
            controls: Arc::new(Mutex::new(Controls::default())),
            batteries: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Refresh the Control Center state once, waiting for the slow items too.
    pub fn refresh_controls_blocking(&self, now_playing: bool) {
        let mut c = Controls::default();
        c.collect_fast();
        c.collect_slow();
        if now_playing {
            c.collect_now_playing();
        }
        *self.controls.lock().unwrap() = c;
        *self.batteries.lock().unwrap() = battery::read().into_iter().collect();
    }

    /// Keep the Control Center state fresh on a background thread. Bluetooth
    /// (system_profiler) is slow, so it refreshes less often than the rest.
    pub fn start_controls(&self, now_playing: bool) {
        let (shared, stop, bat) = (self.controls.clone(), self.stop.clone(), self.batteries.clone());
        std::thread::spawn(move || {
            let mut tick = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let mut c = shared.lock().unwrap().clone();
                c.collect_fast();
                if tick.is_multiple_of(5) {
                    c.collect_slow();
                }
                if now_playing {
                    c.collect_now_playing();
                }
                *shared.lock().unwrap() = c;
                *bat.lock().unwrap() = battery::read().into_iter().collect();
                tick = tick.wrapping_add(1);
                // Sleep in short slices so shutdown is prompt.
                for _ in 0..30 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        });
    }

    pub fn collect(&mut self) -> Snapshot {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        self.nets.refresh(true);
        self.disks.refresh(true);

        let load = System::load_average();
        let (p, e) = perf_cores();
        let cpus = self.sys.cpus();
        let cpu = Cpu {
            usage: self.sys.global_cpu_usage(),
            per_core: cpus.iter().map(|c| c.cpu_usage()).collect(),
            load1: load.one,
            load5: load.five,
            load15: load.fifteen,
            core_count: System::physical_core_count().unwrap_or(cpus.len()),
            logical_cpu: cpus.len(),
            p_core_count: p,
            e_core_count: e,
        };

        let total = self.sys.total_memory();
        // Free + inactive pages count as available; sysinfo also counts
        // purgeable/file cache, which under-reports usage by several GB.
        let (available, cached) = mach_vm_memory().unwrap_or((self.sys.available_memory(), 0));
        let used = total.saturating_sub(available);
        let memory = Memory {
            used,
            total,
            available,
            used_percent: pct(used, total),
            swap_used: self.sys.used_swap(),
            swap_total: self.sys.total_swap(),
            cached,
        };

        let (network, rx_total, tx_total) = self.network_rates();
        push_hist(&mut self.rx_hist, rx_total);
        push_hist(&mut self.tx_hist, tx_total);

        Snapshot {
            collected_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            host: System::host_name().unwrap_or_default(),
            platform: format!(
                "darwin {}",
                System::os_version().unwrap_or_else(|| "?".into())
            ),
            uptime_seconds: System::uptime(),
            procs: self.sys.processes().len(),
            hardware: self.hardware.clone(),
            cpu,
            memory,
            disks: self.disk_list(),
            disk_io: self.disk_io(),
            network,
            network_history: NetHistory {
                rx_history: self.rx_hist.iter().copied().collect(),
                tx_history: self.tx_hist.iter().copied().collect(),
            },
            batteries: self.batteries.lock().unwrap().clone(),
            controls: self.controls.lock().unwrap().clone(),
            top_processes: self.top_processes(total),
        }
    }

    fn network_rates(&mut self) -> (Vec<NetIf>, f64, f64) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_net_at).as_secs_f64().max(0.001);
        self.last_net_at = now;

        let mut out = Vec::new();
        let (mut rx_sum, mut tx_sum) = (0.0, 0.0);
        for (name, data) in self.nets.iter() {
            let (rx, tx) = (data.total_received(), data.total_transmitted());
            let (prx, ptx) = self.last_net.insert(name.clone(), (rx, tx)).unwrap_or((rx, tx));
            if is_noise_interface(name) {
                continue;
            }
            let rx_rate = rx.saturating_sub(prx) as f64 / dt / MB;
            let tx_rate = tx.saturating_sub(ptx) as f64 / dt / MB;
            let ip = data
                .ip_networks()
                .iter()
                .find(|n| n.addr.is_ipv4())
                .map(|n| n.addr.to_string())
                .unwrap_or_default();
            // Interfaces without an address and without traffic are clutter.
            if ip.is_empty() && rx == 0 && tx == 0 {
                continue;
            }
            rx_sum += rx_rate;
            tx_sum += tx_rate;
            out.push(NetIf { name: name.clone(), rx_rate_mbs: rx_rate, tx_rate_mbs: tx_rate, ip });
        }
        out.sort_by(|a, b| {
            let ta = a.rx_rate_mbs + a.tx_rate_mbs;
            let tb = b.rx_rate_mbs + b.tx_rate_mbs;
            tb.partial_cmp(&ta)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.ip.is_empty().cmp(&b.ip.is_empty()))
                .then(a.name.cmp(&b.name))
        });
        (out, rx_sum, tx_sum)
    }

    fn disk_list(&self) -> Vec<DiskInfo> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for d in self.disks.list() {
            let mount = d.mount_point().to_string_lossy().to_string();
            // Skip APFS firmlink/system volumes that alias the same container.
            if mount.starts_with("/System/Volumes") || mount.starts_with("/private/var/vm") {
                continue;
            }
            let device = d.name().to_string_lossy().to_string();
            let total = d.total_space();
            if total == 0 || !seen.insert((device.clone(), total)) {
                continue;
            }
            // On APFS statfs reports container-wide free space, so used = total - free
            // matches what Finder shows (and what status-go's APFS correction yields).
            let used = total.saturating_sub(d.available_space());
            out.push(DiskInfo {
                external: mount.starts_with("/Volumes/") || d.is_removable(),
                mount,
                device,
                used,
                total,
                used_percent: pct(used, total),
                fstype: d.file_system().to_string_lossy().to_string(),
            });
        }
        out.sort_by_key(|d| (d.mount != "/", d.mount.clone()));
        out
    }

    fn disk_io(&mut self) -> DiskIo {
        let Some((r, w)) = read_disk_io_counters() else {
            return DiskIo::default();
        };
        let now = Instant::now();
        let rates = match self.last_io {
            Some((t, pr, pw)) => {
                let dt = now.duration_since(t).as_secs_f64().max(0.001);
                DiskIo {
                    read_rate: r.saturating_sub(pr) as f64 / dt / MB,
                    write_rate: w.saturating_sub(pw) as f64 / dt / MB,
                }
            }
            None => DiskIo::default(),
        };
        self.last_io = Some((now, r, w));
        rates
    }

    fn top_processes(&self, total_mem: u64) -> Vec<Proc> {
        let mut v: Vec<Proc> = self
            .sys
            .processes()
            .values()
            .map(|p| {
                let name = p.name().to_string_lossy().to_string();
                let command = p
                    .cmd()
                    .first()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| name.clone());
                Proc {
                    pid: p.pid().as_u32(),
                    ppid: p.parent().map(|p| p.as_u32()).unwrap_or(0),
                    command: basename(&command),
                    name,
                    cpu: p.cpu_usage(),
                    memory: pct(p.memory(), total_mem),
                    memory_bytes: p.memory(),
                }
            })
            .collect();
        if self.sort_by_memory {
            v.sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes));
        } else {
            v.sort_by(|a, b| {
                b.cpu
                    .partial_cmp(&a.cpu)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.memory_bytes.cmp(&a.memory_bytes))
            });
        }
        v.truncate(self.top_n);
        v
    }
}

fn hardware_info(sys: &System, disks: &Disks) -> Hardware {
    let disk_size = disks
        .list()
        .iter()
        .find(|d| d.mount_point().to_str() == Some("/"))
        .map(|d| d.total_space())
        .unwrap_or(0);
    let model_id = sysctl_string("hw.model").unwrap_or_default();
    // The friendly name ("MacBook Air") is only available from system_profiler (~0.1 s).
    let sp = crate::util::run("/usr/sbin/system_profiler", &["SPHardwareDataType", "-json"], std::time::Duration::from_secs(5));
    let model = if sp.ok { parse_machine_name(&sp.stdout) } else { None }.unwrap_or_else(|| model_id.clone());
    let build = crate::util::run("/usr/bin/sw_vers", &["-buildVersion"], std::time::Duration::from_secs(3));
    Hardware {
        model,
        model_id,
        os_build: build.stdout.trim().to_string(),
        cpu_model: sys.cpus().first().map(|c| c.brand().to_string()).unwrap_or_default(),
        total_ram: human_bytes(sys.total_memory()),
        disk_size: human_bytes(disk_size),
        os_version: format!("macOS {}", System::os_version().unwrap_or_default()),
    }
}

/// (available, cached) bytes from the Mach VM statistics:
/// available = free + inactive pages, cached = file-backed pages.
// `libc` marks mach_host_self deprecated in favour of the mach2 crate; one call
// does not justify another dependency.
#[allow(deprecated)]
fn mach_vm_memory() -> Option<(u64, u64)> {
    // SAFETY: `stats` is a zeroed vm_statistics64 and `count` is its size in
    // integer_t units, as host_statistics64 requires.
    let (free, inactive, external) = unsafe {
        let mut stats: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let rc = libc::host_statistics64(
            libc::mach_host_self(),
            libc::HOST_VM_INFO64,
            &mut stats as *mut libc::vm_statistics64 as libc::host_info64_t,
            &mut count,
        );
        if rc != 0 {
            return None;
        }
        (stats.free_count as u64, stats.inactive_count as u64, stats.external_page_count as u64)
    };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return None;
    }
    let page = page as u64;
    Some(((free + inactive) * page, external * page))
}

fn parse_machine_name(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("SPHardwareDataType")?.get(0)?.get("machine_name")?.as_str().map(String::from).filter(|s| !s.is_empty())
}

/// Performance / efficiency core counts on Apple Silicon (0/0 on Intel).
fn perf_cores() -> (u32, u32) {
    (
        sysctl_u32("hw.perflevel0.logicalcpu").unwrap_or(0),
        sysctl_u32("hw.perflevel1.logicalcpu").unwrap_or(0),
    )
}

fn sysctl_u32(name: &str) -> Option<u32> {
    let cname = CString::new(name).ok()?;
    let mut val: u32 = 0;
    let mut len = std::mem::size_of::<u32>();
    // SAFETY: `val` is a valid u32 and `len` matches its size.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            &mut val as *mut u32 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(val)
}

fn sysctl_string(name: &str) -> Option<String> {
    let cname = CString::new(name).ok()?;
    let mut len: usize = 0;
    // SAFETY: first call only queries the required length.
    let rc = unsafe { libc::sysctlbyname(cname.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: `buf` has `len` bytes.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(len);
    while buf.last() == Some(&0) {
        buf.pop();
    }
    String::from_utf8(buf).ok()
}

/// Cumulative bytes read/written across all block-storage drivers.
///
/// The machine exposes these counters through the IORegistry, so we read them
/// from `ioreg` and difference them.
fn read_disk_io_counters() -> Option<(u64, u64)> {
    let out = Command::new("/usr/sbin/ioreg")
        .args(["-c", "IOBlockStorageDriver", "-r", "-d", "1", "-w0"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_ioreg_io(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_ioreg_io(text: &str) -> (u64, u64) {
    let (mut r, mut w) = (0u64, 0u64);
    for line in text.lines().filter(|l| l.contains("\"Statistics\"")) {
        r = r.saturating_add(extract_counter(line, "\"Bytes (Read)\"="));
        w = w.saturating_add(extract_counter(line, "\"Bytes (Write)\"="));
    }
    (r, w)
}

fn extract_counter(line: &str, key: &str) -> u64 {
    line.find(key)
        .map(|i| {
            line[i + key.len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
        })
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn is_noise_interface(name: &str) -> bool {
    ["lo", "utun", "awdl", "llw", "bridge", "gif", "stf", "anpi", "ap", "XHC", "vmenet", "vnic", "nan"]
        .iter()
        .any(|p| name.starts_with(p))
}

fn push_hist(h: &mut VecDeque<f64>, v: f64) {
    if h.len() >= HISTORY_LEN {
        h.pop_front();
    }
    h.push_back(v);
}

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 { 0.0 } else { part as f64 / total as f64 * 100.0 }
}

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

pub fn human_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ioreg_statistics() {
        let line = r#"    | |   "Statistics" = {"Operations (Write)"=3325042,"Bytes (Read)"=87560269824,"Errors (Write)"=0,"Bytes (Write)"=51983159296,"Operations (Read)"=5375479}"#;
        assert_eq!(parse_ioreg_io(line), (87_560_269_824, 51_983_159_296));
        let two = format!("{line}\n{line}");
        assert_eq!(parse_ioreg_io(&two).0, 2 * 87_560_269_824);
        assert_eq!(parse_ioreg_io("nothing here"), (0, 0));
    }

    #[test]
    fn noise_interfaces_filtered() {
        assert!(is_noise_interface("utun3"));
        assert!(is_noise_interface("lo0"));
        assert!(!is_noise_interface("en0"));
    }

    #[test]
    fn parses_the_friendly_model_name() {
        assert_eq!(
            parse_machine_name(r#"{"SPHardwareDataType":[{"machine_name":"MacBook Air","machine_model":"Mac16,12"}]}"#).as_deref(),
            Some("MacBook Air")
        );
        assert_eq!(parse_machine_name("{}"), None);
        assert_eq!(parse_machine_name("garbage"), None);
    }

    #[test]
    fn bytes_format() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
    }
}
