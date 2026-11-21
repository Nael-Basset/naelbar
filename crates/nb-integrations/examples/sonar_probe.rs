//! Reads Sonar, then writes the unchanged volume back, to exercise the real code paths.
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn main() {
    let seen: Arc<Mutex<Vec<nb_core::Event>>> = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let sink: nb_core::Sink = Arc::new(move |e| s2.lock().unwrap().push(e));
    nb_integrations::sonar_refresh(sink.clone());
    std::thread::sleep(Duration::from_millis(3500));
    let first = seen.lock().unwrap().clone();
    println!("events after refresh: {}", first.len());
    let mut media = None;
    for e in &first {
        if let nb_core::Event::Sonar(list) = e {
            println!("channels: {}", list.len());
            for c in list {
                println!("  {} {:.4} muted {}", c.id, c.volume, c.muted);
                if c.id == "media" {
                    media = Some(c.volume);
                }
            }
        }
    }
    if let Some(v) = media {
        println!("writing media back unchanged ({v:.4})");
        nb_integrations::sonar_set_volume("media", v);
        std::thread::sleep(Duration::from_millis(2500));
        let after = seen.lock().unwrap().len();
        println!("events after write: {} (a read-back is expected)", after);
    } else {
        println!("no media channel, nothing written");
    }
}
