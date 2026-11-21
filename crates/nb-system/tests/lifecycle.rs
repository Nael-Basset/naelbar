//! Read-only lifecycle checks: no setter is called.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[test]
fn stats_timer_only_while_guarded_and_clean_shutdown() {
    let stats = Arc::new(AtomicUsize::new(0));
    let s2 = stats.clone();
    let sink: nb_core::Sink = Arc::new(move |e| {
        if matches!(e, nb_core::Event::Stats(_)) {
            s2.fetch_add(1, Ordering::SeqCst);
        }
    });
    let h = nb_system::start(sink).expect("start");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!h.stats_timer_active(), "no timer without a guard");
    assert_eq!(stats.load(Ordering::SeqCst), 0, "no stats without a guard");

    let g1 = h.subscribe_stats();
    let g2 = h.subscribe_stats();
    let t = Instant::now();
    while !h.stats_timer_active() && t.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(h.stats_timer_active());
    std::thread::sleep(Duration::from_millis(2500));
    assert!(stats.load(Ordering::SeqCst) >= 2);

    drop(g1);
    std::thread::sleep(Duration::from_millis(1200));
    assert!(h.stats_timer_active(), "one guard left");
    drop(g2);
    let t = Instant::now();
    while h.stats_timer_active() && t.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!h.stats_timer_active(), "timer torn down with last guard");
    let n = stats.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(2200));
    assert_eq!(stats.load(Ordering::SeqCst), n, "no stats after last guard");
    drop(h);
}
