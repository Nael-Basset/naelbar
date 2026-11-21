//! Measures what the pill visualizer's sampling costs, without the island:
//! reads the default output's peak meter 30 times per second (reopening it
//! every 10 s, like the shell) and prints the CPU time used.
//!
//! cargo run --release -p nb-media --example meter_cost -- 20

use std::time::{Duration, Instant};

use nb_media::{MAX_CHANNELS, PeakMeter};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

fn cpu_ms() -> f64 {
    let (mut a, mut b, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: valid out pointers for the current process.
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u);
    }
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 10_000.0;
    t(k) + t(u)
}

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(20);
    // SAFETY: plain COM initialisation, like the island's UI thread.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let start = Instant::now();
    let cpu0 = cpu_ms();
    let mut meter = PeakMeter::open().ok();
    let mut opened = Instant::now();
    let (mut samples, mut loud, mut max_peak) = (0u32, 0u32, 0.0f32);
    let mut ch = [0.0f32; MAX_CHANNELS];
    while start.elapsed() < Duration::from_secs(secs) {
        if opened.elapsed() >= Duration::from_secs(10) {
            meter = PeakMeter::open().ok();
            opened = Instant::now();
        }
        if let Some(Ok((peak, _))) = meter.as_ref().map(|m| m.read(&mut ch)) {
            samples += 1;
            loud += u32::from(peak > 0.01);
            max_peak = max_peak.max(peak);
        }
        std::thread::sleep(Duration::from_millis(33));
    }
    let wall = start.elapsed().as_secs_f64() * 1000.0;
    let cpu = cpu_ms() - cpu0;
    println!("samples {samples}, with sound {loud}, max peak {max_peak:.2}");
    println!("cpu {cpu:.1} ms over {wall:.0} ms = {:.3} % of one core, {:.1} us per sample", cpu / wall * 100.0, cpu * 1000.0 / samples.max(1) as f64);
}
