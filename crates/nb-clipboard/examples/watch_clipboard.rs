//! Prints clipboard events until Ctrl+C. Prints only the content type and its
//! size, never the content.
//!
//! History is kept in a temp directory so running this does not touch your
//! real history. Pass `--appdata` to use `%APPDATA%\naelbar\clipboard`.

use nb_core::{ClipContent, Event, Sink};
use std::sync::Arc;

fn main() {
    let mut config = nb_clipboard::Config::default();
    if !std::env::args().any(|a| a == "--appdata") {
        config.dir = Some(std::env::temp_dir().join("naelbar-clipboard-watch"));
    }
    let sink: Sink = Arc::new(|ev| {
        if let Event::ClipboardAdded(item) = ev {
            let what = match &item.content {
                ClipContent::Text(t) => format!("text, {} chars", t.chars().count()),
                ClipContent::Image { png, width, height } => {
                    format!("image, {width}x{height}, {} bytes png", png.len())
                }
                ClipContent::Files(f) => format!("files, {} paths", f.len()),
            };
            println!("added id={} {}", item.id, what);
        }
    });
    let handle = match nb_clipboard::start(sink, config) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("failed to start: {e}");
            std::process::exit(1);
        }
    };
    println!("history: {} items. Waiting for clipboard changes, Ctrl+C to stop.", handle.list().len());
    // Ctrl+C ends the process; nothing to poll here.
    loop {
        std::thread::park();
    }
}
