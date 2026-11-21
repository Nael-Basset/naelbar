//! Prints integration events until Ctrl+C.
//! Usage: cargo run -p nb-integrations --example watch [port] [lat lon]
//! Then: curl "http://127.0.0.1:47911/notify?title=Hi&body=there"

use std::sync::Arc;

use nb_core::Event;
use nb_integrations::{Config, Error, fetch_weather, start, start_toast_mirror};

fn main() {
    let mut args = std::env::args().skip(1);
    let port = args.next().and_then(|a| a.parse().ok()).unwrap_or(nb_integrations::DEFAULT_PORT);
    let coords = match (args.next(), args.next()) {
        (Some(a), Some(b)) => a.parse::<f64>().ok().zip(b.parse::<f64>().ok()),
        _ => None,
    };
    let sink: nb_core::Sink = Arc::new(|e: Event| println!("{e:?}"));

    let handle = start(sink.clone(), Config { enabled: true, port }).expect("endpoint");
    println!("endpoint on 127.0.0.1:{}", handle.port().unwrap());

    let _toast = match start_toast_mirror(sink.clone()) {
        Ok(t) => {
            println!("toast mirror: active");
            Some(t)
        }
        Err(Error::Unsupported(why)) => {
            println!("toast mirror: unsupported ({why})");
            None
        }
        Err(e) => {
            println!("toast mirror: error {e}");
            None
        }
    };

    if let Some((lat, lon)) = coords {
        fetch_weather(sink, lat, lon, Some(Box::new(|e| println!("weather error: {e}"))));
    }

    println!("waiting for events, Ctrl+C to quit");
    // Parked forever; Ctrl+C terminates the process.
    loop {
        std::thread::park();
    }
}
