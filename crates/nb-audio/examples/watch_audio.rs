//! Prints audio events until Ctrl+C. Read-only: changes nothing.

use std::sync::{Arc, OnceLock};
use std::thread::{self, Thread};

use windows::core::BOOL;
use windows::Win32::System::Console::SetConsoleCtrlHandler;

static MAIN: OnceLock<Thread> = OnceLock::new();

unsafe extern "system" fn on_ctrl(_: u32) -> BOOL {
    if let Some(t) = MAIN.get() {
        t.unpark();
    }
    BOOL(1)
}

fn main() {
    let _ = MAIN.set(thread::current());
    unsafe { SetConsoleCtrlHandler(Some(on_ctrl), true).expect("ctrl handler") };
    let sink: nb_core::Sink = Arc::new(|e| println!("{e:#?}"));
    let handle = nb_audio::start(sink).expect("start nb-audio");
    println!("watching; Ctrl+C to quit");
    thread::park();
    println!("stopping");
    drop(handle);
}
