//! Prints media events until Ctrl+C.
use std::sync::Arc;

use nb_core::Event;

fn main() {
    let sink: nb_core::Sink = Arc::new(|event| {
        if let Event::Media(m) = event {
            match m {
                None => println!("media: none"),
                Some(s) => println!(
                    "media: [{}] {:?} | {} - {} ({}) | {}/{} ms sampled@{} | art={} | prev={} next={} toggle={}",
                    s.app_id,
                    s.status,
                    s.artist,
                    s.title,
                    s.album,
                    s.position_ms,
                    s.duration_ms,
                    s.position_sampled_at_ms,
                    s.artwork.as_ref().map_or(0, |a| a.len()),
                    s.can_previous,
                    s.can_next,
                    s.can_toggle,
                ),
            }
        }
    });
    let _handle = nb_media::start(sink).expect("start media service");
    println!("watching media events, Ctrl+C to quit");
    loop {
        std::thread::park();
    }
}
