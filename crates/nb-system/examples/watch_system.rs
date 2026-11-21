//! Prints nb-system events until Ctrl+C. Pass `--stats` to subscribe to stats.

use nb_core::Event;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use windows::core::BOOL;
use windows::Win32::System::Console::SetConsoleCtrlHandler;

static QUIT: OnceLock<mpsc::Sender<()>> = OnceLock::new();

unsafe extern "system" fn on_ctrl(_kind: u32) -> BOOL {
    if let Some(tx) = QUIT.get() {
        let _ = tx.send(());
    }
    true.into()
}

fn main() {
    let want_stats = std::env::args().any(|a| a == "--stats");
    let t0 = Instant::now();
    let sink: nb_core::Sink = Arc::new(move |e: Event| {
        println!("[{:>7.3}s] {:?}", t0.elapsed().as_secs_f64(), e);
    });
    let (tx, rx) = mpsc::channel();
    let _ = QUIT.set(tx);
    // SAFETY: handler is a plain function that only sends on a channel.
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(on_ctrl), true);
    }
    let handle = match nb_system::start(sink) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("start failed: {e}");
            std::process::exit(1);
        }
    };
    println!("running; Ctrl+C to stop. stats timer active: {}", handle.stats_timer_active());
    let guard = want_stats.then(|| handle.subscribe_stats());
    let _ = rx.recv();
    println!(
        "brightness supported: {:?}; stats timer active before drop: {}",
        handle.brightness_supported(),
        handle.stats_timer_active()
    );
    drop(guard);
    drop(handle);
    println!("stopped");
}
