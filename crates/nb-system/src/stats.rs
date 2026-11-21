//! System statistics sampler. Exists only while a stats guard is alive.

use nb_core::{ProcessUse, SystemStats};
use std::collections::HashMap;
use std::time::Instant;
use windows::Wdk::Graphics::Direct3D::{
    D3DKMT_ADAPTER_PERFDATA, D3DKMT_ADAPTERINFO, D3DKMT_CLOSEADAPTER, D3DKMT_ENUMADAPTERS2, D3DKMT_NODE_PERFDATA,
    D3DKMT_QUERYADAPTERINFO, D3DKMTCloseAdapter, D3DKMTEnumAdapters2, D3DKMTQueryAdapterInfo, KMTQAITYPE_ADAPTERPERFDATA,
    KMTQAITYPE_NODEPERFDATA,
};
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW,
    PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{GetProcessTimes, GetSystemTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::core::w;

fn ft(f: FILETIME) -> u64 {
    (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
}

/// CPU busy percentage from two (idle, kernel, user) snapshots. Kernel time
/// includes idle time, as returned by `GetSystemTimes`.
pub fn cpu_percent(prev: (u64, u64, u64), cur: (u64, u64, u64)) -> f32 {
    let idle = cur.0.wrapping_sub(prev.0);
    let total = cur.1.wrapping_sub(prev.1).wrapping_add(cur.2.wrapping_sub(prev.2));
    if total == 0 {
        return 0.0;
    }
    (total.saturating_sub(idle) as f64 * 100.0 / total as f64).clamp(0.0, 100.0) as f32
}

/// Bits per second from two byte counters read `secs` apart.
pub fn rate(prev: u64, cur: u64, secs: f64) -> u64 {
    if secs <= 0.0 || cur < prev {
        return 0;
    }
    ((cur - prev) as f64 * 8.0 / secs) as u64
}

/// Reduces "GPU Engine" instances to one percentage: the busiest
/// (adapter, engine type) group, summed over processes, as Task Manager does.
pub fn gpu_from_instances(items: &[(String, f64)]) -> Option<f32> {
    use std::collections::HashMap;
    let mut groups: HashMap<(String, String), f64> = HashMap::new();
    for (name, v) in items {
        // pid_123_luid_0xAAAA_0xBBBB_phys_0_eng_0_engtype_3D
        let luid = name
            .split("_luid_")
            .nth(1)
            .and_then(|s| s.split("_phys_").next())
            .unwrap_or("")
            .to_string();
        let ty = name.rsplit("engtype_").next().unwrap_or("").to_string();
        if name.contains("engtype_") {
            *groups.entry((luid, ty)).or_default() += v;
        }
    }
    groups.values().cloned().reduce(f64::max).map(|v| v.clamp(0.0, 100.0) as f32)
}

/// How many applications each ranking keeps.
const TOP_PROCESSES: usize = 8;

/// Adds up (name, cpu %, ram bytes) by application, then keeps the heaviest
/// users of each resource.
pub fn aggregate_processes(samples: &[(String, f32, u64)]) -> Vec<ProcessUse> {
    let mut by_name: HashMap<&str, (f32, u64)> = HashMap::new();
    for (name, cpu, ram) in samples {
        let e = by_name.entry(name.as_str()).or_default();
        e.0 += cpu;
        e.1 += ram;
    }
    let mut all: Vec<ProcessUse> =
        by_name.into_iter().map(|(n, (cpu_percent, ram_bytes))| ProcessUse { name: n.to_string(), cpu_percent, ram_bytes }).collect();
    all.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent));
    let mut keep: Vec<ProcessUse> = all.iter().take(TOP_PROCESSES).cloned().collect();
    all.sort_by(|a, b| b.ram_bytes.cmp(&a.ram_bytes));
    for p in all.into_iter().take(TOP_PROCESSES) {
        if !keep.iter().any(|k| k.name == p.name) {
            keep.push(p);
        }
    }
    keep
}

struct Pdh {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

impl Pdh {
    fn open() -> Option<Pdh> {
        let mut q = PDH_HQUERY::default();
        let mut c = PDH_HCOUNTER::default();
        // SAFETY: valid out pointers; the query is closed in Drop.
        unsafe {
            if PdhOpenQueryW(None, 0, &mut q) != 0 {
                return None;
            }
            if PdhAddEnglishCounterW(q, w!("\\GPU Engine(*)\\Utilization Percentage"), 0, &mut c) != 0 {
                PdhCloseQuery(q);
                return None;
            }
            // Baseline sample; rate counters need two.
            PdhCollectQueryData(q);
        }
        Some(Pdh { query: q, counter: c })
    }

    fn read(&mut self) -> Option<f32> {
        // SAFETY: the buffer is sized by PDH and 8-byte aligned (Vec<u64>).
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let mut size = 0u32;
            let mut count = 0u32;
            let r = PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            if r != PDH_MORE_DATA {
                return None;
            }
            let mut buf = vec![0u64; (size as usize).div_ceil(8) + 1];
            let ptr = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(ptr)) != 0 {
                return None;
            }
            let items = std::slice::from_raw_parts(ptr, count as usize);
            let v: Vec<(String, f64)> = items
                .iter()
                .filter(|i| i.FmtValue.CStatus == 0)
                .map(|i| (i.szName.to_string().unwrap_or_default(), i.FmtValue.Anonymous.doubleValue))
                .collect();
            gpu_from_instances(&v)
        }
    }
}

impl Drop for Pdh {
    fn drop(&mut self) {
        // SAFETY: query handle owned by this struct.
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

/// Highest ACPI zone temperature in Celsius from Kelvin readings, or `None`
/// when nothing plausible (20 to 110 C) is reported.
pub fn hottest_celsius(kelvin: &[f64]) -> Option<f32> {
    kelvin.iter().map(|k| k - 273.15).filter(|c| (20.0..=110.0).contains(c)).reduce(f64::max).map(|c| c as f32)
}

/// Current CPU clock from "% Processor Performance" and the base frequency in MHz.
pub fn effective_mhz(performance_percent: f64, base_mhz: f64) -> Option<u32> {
    let v = performance_percent * base_mhz / 100.0;
    (v.is_finite() && v > 0.0 && v < 20_000.0).then(|| v.round() as u32)
}

/// Temperature (deci-Celsius) and frequency (Hz) of one adapter to display units.
/// A zero temperature means the adapter is asleep.
pub fn gpu_units(deci_celsius: u32, hz: u64) -> Option<(f32, u32)> {
    (deci_celsius > 0).then(|| (deci_celsius as f32 / 10.0, (hz / 1_000_000) as u32))
}

/// Formatted values of a wildcard counter, valid entries only.
unsafe fn counter_values(counter: PDH_HCOUNTER) -> Vec<f64> {
    // SAFETY: the buffer is sized by PDH and 8-byte aligned (Vec<u64>).
    unsafe {
        let mut size = 0u32;
        let mut count = 0u32;
        if PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None) != PDH_MORE_DATA {
            return Vec::new();
        }
        let mut buf = vec![0u64; (size as usize).div_ceil(8) + 1];
        let ptr = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
        if PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(ptr)) != 0 {
            return Vec::new();
        }
        std::slice::from_raw_parts(ptr, count as usize)
            .iter()
            .filter(|i| i.FmtValue.CStatus == 0)
            .map(|i| i.FmtValue.Anonymous.doubleValue)
            .collect()
    }
}

/// Extra counters and adapter handles, alive only after the usage panel was opened.
struct Sensors {
    query: Option<PDH_HQUERY>,
    perf: PDH_HCOUNTER,
    freq: PDH_HCOUNTER,
    thermal: PDH_HCOUNTER,
    has_clock: bool,
    has_thermal: bool,
    /// Kernel handle of each display adapter.
    adapters: Vec<u32>,
}

impl Sensors {
    fn open() -> Option<Sensors> {
        let mut s = Sensors {
            query: None,
            perf: PDH_HCOUNTER::default(),
            freq: PDH_HCOUNTER::default(),
            thermal: PDH_HCOUNTER::default(),
            has_clock: false,
            has_thermal: false,
            adapters: Vec::new(),
        };
        // SAFETY: valid out pointers; the query and adapters are released in Drop.
        unsafe {
            let mut q = PDH_HQUERY::default();
            if PdhOpenQueryW(None, 0, &mut q) == 0 {
                s.query = Some(q);
                s.has_clock = PdhAddEnglishCounterW(q, w!("\\Processor Information(_Total)\\% Processor Performance"), 0, &mut s.perf) == 0
                    && PdhAddEnglishCounterW(q, w!("\\Processor Information(_Total)\\Processor Frequency"), 0, &mut s.freq) == 0;
                s.has_thermal = PdhAddEnglishCounterW(q, w!("\\Thermal Zone Information(*)\\Temperature"), 0, &mut s.thermal) == 0;
                PdhCollectQueryData(q); // baseline
            }
            let mut list = [D3DKMT_ADAPTERINFO::default(); 16];
            let mut e = D3DKMT_ENUMADAPTERS2 { NumAdapters: list.len() as u32, pAdapters: list.as_mut_ptr() };
            if D3DKMTEnumAdapters2(&mut e).is_ok() {
                s.adapters = list[..(e.NumAdapters as usize).min(list.len())].iter().map(|a| a.hAdapter).collect();
            }
        }
        Some(s)
    }

    fn read(&mut self, out: &mut SystemStats) {
        // SAFETY: counters belong to our open query; D3DKMT structs are sized as required.
        unsafe {
            if let Some(q) = self.query
                && PdhCollectQueryData(q) == 0
            {
                if self.has_clock {
                    let p = counter_values(self.perf);
                    let f = counter_values(self.freq);
                    if let (Some(p), Some(f)) = (p.first(), f.first()) {
                        out.cpu_mhz = effective_mhz(*p, *f);
                    }
                }
                if self.has_thermal {
                    out.cpu_temp_c = hottest_celsius(&counter_values(self.thermal));
                }
            }
            for &h in &self.adapters {
                let mut perf = D3DKMT_ADAPTER_PERFDATA::default();
                let mut qi = D3DKMT_QUERYADAPTERINFO {
                    hAdapter: h,
                    Type: KMTQAITYPE_ADAPTERPERFDATA,
                    pPrivateDriverData: &mut perf as *mut _ as *mut _,
                    PrivateDriverDataSize: size_of::<D3DKMT_ADAPTER_PERFDATA>() as u32,
                };
                if !D3DKMTQueryAdapterInfo(&mut qi).is_ok() {
                    continue;
                }
                let mut node = D3DKMT_NODE_PERFDATA::default();
                let mut qn = D3DKMT_QUERYADAPTERINFO {
                    hAdapter: h,
                    Type: KMTQAITYPE_NODEPERFDATA,
                    pPrivateDriverData: &mut node as *mut _ as *mut _,
                    PrivateDriverDataSize: size_of::<D3DKMT_NODE_PERFDATA>() as u32,
                };
                let hz = if D3DKMTQueryAdapterInfo(&mut qn).is_ok() { node.Frequency } else { 0 };
                if let Some((t, mhz)) = gpu_units(perf.Temperature, hz) {
                    out.gpu_temp_c = Some(t);
                    out.gpu_mhz = (mhz > 0).then_some(mhz);
                    break;
                }
            }
        }
    }
}

impl Drop for Sensors {
    fn drop(&mut self) {
        // SAFETY: handles owned by this struct.
        unsafe {
            if let Some(q) = self.query {
                PdhCloseQuery(q);
            }
            for &h in &self.adapters {
                let _ = D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER { hAdapter: h });
            }
        }
    }
}

pub struct Sampler {
    prev_cpu: (u64, u64, u64),
    prev_net: (u64, u64),
    prev_at: Instant,
    pdh: Option<Pdh>,
    /// Clock and temperature sources, opened on first need.
    sensors: Option<Sensors>,
    sensors_tried: bool,
    /// pid to (creation time, cpu time), to measure use between two ticks.
    prev_procs: HashMap<u32, (u64, u64)>,
    prev_procs_at: Instant,
    cpus: f64,
}

fn cpu_times() -> (u64, u64, u64) {
    let (mut i, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: valid out pointers.
    let _ = unsafe { GetSystemTimes(Some(&mut i), Some(&mut k), Some(&mut u)) };
    (ft(i), ft(k), ft(u))
}

/// Total (received, sent) octets over hardware interfaces that are up.
fn net_octets() -> (u64, u64) {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    let (mut rx, mut tx) = (0u64, 0u64);
    // SAFETY: the table is freed with FreeMibTable; rows are read within NumEntries.
    unsafe {
        if GetIfTable2(&mut table).is_err() || table.is_null() {
            return (0, 0);
        }
        let n = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), n);
        for r in rows {
            let hardware = r.InterfaceAndOperStatusFlags._bitfield & 1 != 0;
            // IfOperStatusUp = 1
            if hardware && r.OperStatus.0 == 1 {
                rx += r.InOctets;
                tx += r.OutOctets;
            }
        }
        FreeMibTable(table as *const _);
    }
    (rx, tx)
}

impl Sampler {
    pub fn new() -> Sampler {
        Sampler {
            prev_cpu: cpu_times(),
            prev_net: net_octets(),
            prev_at: Instant::now(),
            pdh: Pdh::open(),
            sensors: None,
            sensors_tried: false,
            prev_procs: HashMap::new(),
            prev_procs_at: Instant::now(),
            cpus: std::thread::available_parallelism().map_or(1, |n| n.get()) as f64,
        }
    }

    pub fn sample(&mut self) -> SystemStats {
        let now = Instant::now();
        let secs = now.duration_since(self.prev_at).as_secs_f64();
        let cpu = cpu_times();
        let net = net_octets();
        let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        // SAFETY: valid struct with dwLength set.
        let ram = if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { m.dwMemoryLoad as f32 } else { 0.0 };
        let out = SystemStats {
            cpu_percent: cpu_percent(self.prev_cpu, cpu),
            gpu_percent: self.pdh.as_mut().and_then(|p| p.read()),
            gpu_temp_c: None,
            gpu_mhz: None,
            cpu_mhz: None,
            cpu_temp_c: None,
            ram_percent: ram,
            ram_used_bytes: m.ullTotalPhys.saturating_sub(m.ullAvailPhys),
            ram_total_bytes: m.ullTotalPhys,
            net_down_bps: rate(self.prev_net.0, net.0, secs),
            net_up_bps: rate(self.prev_net.1, net.1, secs),
        };
        self.prev_cpu = cpu;
        self.prev_net = net;
        self.prev_at = now;
        out
    }

    /// Fills CPU/GPU clock and temperature. Only runs while the detail panel is open.
    pub fn sample_sensors(&mut self, out: &mut SystemStats) {
        if !self.sensors_tried {
            self.sensors_tried = true;
            self.sensors = Sensors::open();
        }
        if let Some(s) = self.sensors.as_mut() {
            s.read(out);
        }
    }

    /// Reads every process it may open. Only runs while the detail panel is open.
    pub fn processes(&mut self) -> Vec<ProcessUse> {
        let now = Instant::now();
        let elapsed_100ns = now.duration_since(self.prev_procs_at).as_secs_f64() * 1e7;
        let mut samples = Vec::with_capacity(256);
        let mut next = HashMap::with_capacity(self.prev_procs.len().max(256));
        // SAFETY: the snapshot and process handles are closed below, and the
        // out structs are sized as the API requires.
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return Vec::new() };
            let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut more = Process32FirstW(snap, &mut e).is_ok();
            while more {
                let pid = e.th32ProcessID;
                if pid != 0 {
                    if let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                        let (mut created, mut exit, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
                        if GetProcessTimes(h, &mut created, &mut exit, &mut kernel, &mut user).is_ok() {
                            let started = ft(created);
                            let cpu_time = ft(kernel) + ft(user);
                            let cpu = match self.prev_procs.get(&pid) {
                                Some(&(s, t)) if s == started && elapsed_100ns > 0.0 => (cpu_time.saturating_sub(t) as f64 * 100.0 / (elapsed_100ns * self.cpus)) as f32,
                                _ => 0.0,
                            };
                            next.insert(pid, (started, cpu_time));
                            let mut mem = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
                            let ram = if GetProcessMemoryInfo(h, &mut mem as *mut _ as *mut PROCESS_MEMORY_COUNTERS, mem.cb).is_ok() { mem.PrivateUsage as u64 } else { 0 };
                            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                            let name = String::from_utf16_lossy(&e.szExeFile[..len]);
                            let name = name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(&name).to_string();
                            samples.push((name, cpu, ram));
                        }
                        let _ = CloseHandle(h);
                    }
                }
                more = Process32NextW(snap, &mut e).is_ok();
            }
            let _ = CloseHandle(snap);
        }
        self.prev_procs = next;
        self.prev_procs_at = now;
        aggregate_processes(&samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_math() {
        // 100 total ticks of which 25 idle -> 75 %
        assert!((cpu_percent((0, 0, 0), (25, 80, 20)) - 75.0).abs() < 0.01);
        assert_eq!(cpu_percent((5, 5, 5), (5, 5, 5)), 0.0);
    }

    #[test]
    fn processes_add_up_per_application() {
        let v = vec![("brave".to_string(), 3.0, 100), ("brave".to_string(), 2.0, 50), ("steam".to_string(), 1.0, 500)];
        let out = aggregate_processes(&v);
        let brave = out.iter().find(|p| p.name == "brave").unwrap();
        assert_eq!((brave.cpu_percent, brave.ram_bytes), (5.0, 150));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_big_memory_user_survives_the_cpu_ranking() {
        let mut v: Vec<(String, f32, u64)> = (0..20).map(|i| (format!("busy{i}"), 50.0, 1)).collect();
        v.push(("idle-but-huge".to_string(), 0.0, 1 << 40));
        assert!(aggregate_processes(&v).iter().any(|p| p.name == "idle-but-huge"));
    }

    #[test]
    fn sensor_math() {
        assert_eq!(hottest_celsius(&[300.15, 330.15, 0.0]), Some(57.0));
        assert_eq!(hottest_celsius(&[0.0, 500.0]), None);
        assert_eq!(hottest_celsius(&[]), None);
        assert_eq!(effective_mhz(50.0, 2300.0), Some(1150));
        assert_eq!(effective_mhz(0.0, 2300.0), None);
        assert_eq!(gpu_units(0, 210_000_000), None);
        assert_eq!(gpu_units(455, 1_500_000_000), Some((45.5, 1500)));
    }

    #[test]
    fn rate_math() {
        assert_eq!(rate(1000, 3000, 2.0), 8000);
        assert_eq!(rate(3000, 1000, 1.0), 0);
        assert_eq!(rate(0, 10, 0.0), 0);
    }

    #[test]
    fn gpu_grouping() {
        let v = vec![
            ("pid_1_luid_0x0_0xA_phys_0_eng_0_engtype_3D".to_string(), 10.0),
            ("pid_2_luid_0x0_0xA_phys_0_eng_0_engtype_3D".to_string(), 15.0),
            ("pid_2_luid_0x0_0xA_phys_0_eng_1_engtype_Copy".to_string(), 5.0),
            ("pid_3_luid_0x0_0xB_phys_0_eng_0_engtype_3D".to_string(), 90.0),
        ];
        assert_eq!(gpu_from_instances(&v), Some(90.0));
        assert_eq!(gpu_from_instances(&v[..3]), Some(25.0));
        assert_eq!(gpu_from_instances(&[]), None);
    }
}
