//! The single seam between the shell and the outside world.
//!
//! Wiring a real service means: start it in [`Services::start`] with a clone of
//! the sink, keep its `Handle` in [`Services`], and forward the matching
//! commands in [`Services::execute`]. Nothing else in the shell knows about
//! services. `--demo` replaces them all with fake data.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use nb_core::*;

use crate::state::{Command, unix_ms};

const WEATHER_MAX_AGE: Duration = Duration::from_secs(30 * 60);

pub struct Services {
    demo: Option<Demo>,
    live: Option<Live>,
}

impl Services {
    pub fn start(sink: Sink, demo: bool, weather_location: Option<(f64, f64)>) -> Self {
        if demo {
            Self { demo: Some(Demo::start(sink)), live: None }
        } else {
            Self { demo: None, live: Some(Live::start(sink, weather_location)) }
        }
    }

    /// Reads the Bluetooth batteries now (short-lived thread, answer as `Event::Devices`).
    pub fn read_devices(&self) {
        if let Some(l) = &self.live {
            nb_system::devices::refresh(l.sink.clone());
        }
    }

    /// Same, unless a read started in the last 10 s (the island opens often).
    pub fn request_devices(&self) {
        if let Some(l) = &self.live {
            crate::widgets::batteries::request(&l.sink);
        }
    }

    /// A new place replaces the cached weather at once.
    pub fn set_weather_location(&mut self, location: Option<(f64, f64)>) {
        if let Some(l) = &mut self.live {
            l.weather_location = location;
            l.weather_fetched = None;
            l.refresh_weather();
        }
    }

    /// Which media apps may drive the island, and which one wins.
    pub fn set_media_prefs(&mut self, prefs: nb_media::Prefs) {
        if let Some(m) = self.live.as_ref().and_then(|l| l.media.as_ref()) {
            m.set_prefs(prefs);
        }
    }

    /// Installs or removes the lock key hook. It only exists while wanted.
    pub fn set_lock_keys(&mut self, on: bool) {
        if let Some(l) = &mut self.live {
            if on && l.lock_keys.is_none() {
                l.lock_keys = optional("lock keys", nb_system::watch_lock_keys(l.sink.clone()));
            } else if !on {
                l.lock_keys = None;
            }
        }
    }

    /// Starts or stops the Downloads folder watch and the privacy indicator
    /// watch. Each exists only while wanted. Stopping the indicators sends an
    /// empty list so nothing stays lit.
    pub fn set_activity(&mut self, downloads: bool, indicators: bool) {
        let Some(l) = &mut self.live else { return };
        if downloads && l.downloads.is_none() {
            l.downloads = optional("downloads", nb_system::watch_downloads(l.sink.clone()));
        } else if !downloads {
            l.downloads = None;
        }
        if indicators && l.capture.is_none() {
            l.capture = optional("privacy indicators", nb_system::watch_capture(l.sink.clone()));
        } else if !indicators && l.capture.take().is_some() {
            (l.sink)(Event::Capture(Vec::new()));
        }
    }

    /// Runs a command on the UI thread. Must not block.
    pub fn execute(&mut self, cmd: &Command) {
        if let Some(d) = &mut self.demo {
            d.execute(cmd);
        }
        if let Some(l) = &mut self.live {
            l.execute(cmd);
        }
    }
}

/// A service that fails to start only loses its own feature.
fn optional<T, E: std::fmt::Debug>(name: &str, result: Result<T, E>) -> Option<T> {
    result.map_err(|e| eprintln!("naelbar: {name} unavailable: {e:?}")).ok()
}

struct Live {
    sink: Sink,
    media: Option<nb_media::Handle>,
    audio: Option<nb_audio::Handle>,
    clipboard: Option<nb_clipboard::Handle>,
    system: Option<nb_system::Handle>,
    _endpoint: Option<nb_integrations::Handle>,
    _toasts: Option<nb_integrations::WpnMirror>,
    stats: Option<nb_system::StatsGuard>,
    weather_location: Option<(f64, f64)>,
    weather_fetched: Option<Instant>,
    lock_keys: Option<nb_system::LockKeyWatch>,
    downloads: Option<nb_system::DownloadWatch>,
    capture: Option<nb_system::CaptureWatch>,
}

impl Live {
    fn start(sink: Sink, weather_location: Option<(f64, f64)>) -> Self {
        let clipboard = optional("clipboard", nb_clipboard::start(sink.clone(), Default::default()));
        // The history is not re-emitted by the service. Oldest first, because
        // the shell pushes each item to the front.
        if let Some(c) = &clipboard {
            for item in c.list().into_iter().rev() {
                sink(Event::ClipboardAdded(item));
            }
        }
        Self {
            media: optional("media", nb_media::start(sink.clone())),
            audio: optional("audio", nb_audio::start(sink.clone())),
            clipboard,
            system: optional("system", nb_system::start(sink.clone())),
            _endpoint: optional("endpoint", nb_integrations::start(sink.clone(), nb_integrations::Config::default())),
            // Reads the user's notification database: no package identity needed.
            _toasts: optional("notification mirror", nb_integrations::start_wpn_mirror(sink.clone())),
            stats: None,
            weather_location,
            weather_fetched: None,
            lock_keys: None,
            downloads: None,
            capture: None,
            sink,
        }
    }

    fn refresh_weather(&mut self) {
        let Some((lat, lon)) = self.weather_location else { return };
        if self.weather_fetched.is_some_and(|t| t.elapsed() < WEATHER_MAX_AGE) {
            return;
        }
        self.weather_fetched = Some(Instant::now());
        nb_integrations::fetch_weather(self.sink.clone(), lat, lon, None);
    }

    fn execute(&mut self, cmd: &Command) {
        match cmd {
            Command::MediaToggle => { self.media.as_ref().map(|m| m.toggle()); }
            Command::MediaNext => { self.media.as_ref().map(|m| m.next()); }
            Command::MediaPrevious => { self.media.as_ref().map(|m| m.previous()); }
            Command::MediaSeek(ms) => { self.media.as_ref().map(|m| m.seek(*ms)); }
            Command::SetVolume(v) => { self.audio.as_ref().map(|a| a.set_output_volume(*v)); }
            Command::SetMuted(on) => { self.audio.as_ref().map(|a| a.set_output_muted(*on)); }
            Command::SetMicMuted(on) => { self.audio.as_ref().map(|a| a.set_mic_muted(*on)); }
            Command::SetBrightness(id, b) => { self.system.as_ref().map(|s| s.set_brightness(*id, *b)); }
            Command::BrightnessRefresh => { self.system.as_ref().map(|s| s.request_brightness()); }
            Command::SonarRefresh => nb_integrations::sonar_refresh(self.sink.clone()),
            Command::SonarVolume(c, v) => nb_integrations::sonar_set_volume(c, *v),
            Command::SetWifi(on) => { self.system.as_ref().map(|s| s.set_wifi(*on)); }
            Command::SetBluetooth(on) => { self.system.as_ref().map(|s| s.set_bluetooth(*on)); }
            Command::CopyClip(id) => { self.clipboard.as_ref().map(|c| c.copy_item(*id)); }
            Command::ClearClips => { self.clipboard.as_ref().map(|c| c.clear()); }
            Command::PinClip(id, on) => { self.clipboard.as_ref().map(|c| c.pin(*id, *on)); }
            Command::DeleteClip(id) => { self.clipboard.as_ref().map(|c| c.delete(*id)); }
            Command::StatsSubscribe(true) => {
                self.stats = self.system.as_ref().map(|s| s.subscribe_stats());
                self.refresh_weather();
            }
            Command::StatsSubscribe(false) => self.stats = None,
            Command::StatsDetail(on) => { self.system.as_ref().map(|s| s.set_stats_detail(*on)); }
            Command::RefreshBatteries => crate::widgets::batteries::request(&self.sink),
            _ => {}
        }
    }
}

/// Plausible fake data. Emits its state once, then only reacts to commands.
struct Demo {
    sink: Sink,
    media: MediaState,
    audio: AudioState,
    radios: RadioState,
    stats_on: Arc<AtomicBool>,
    track: usize,
}

const TRACKS: [(&str, &str, &str, u64); 3] = [
    ("Slow Tide", "Harbor Lights", "Night Ferries", 222_000),
    ("Paper Lanterns", "The Quiet Hours", "Low Season", 187_000),
    ("Glasshouse", "Ines Marlow", "Conservatory", 251_000),
];

impl Demo {
    fn start(sink: Sink) -> Self {
        let art: Arc<[u8]> = demo_png().into();
        let media = MediaState {
            app_id: "demo.player".into(),
            title: TRACKS[0].0.into(),
            artist: TRACKS[0].1.into(),
            album: TRACKS[0].2.into(),
            status: PlaybackStatus::Playing,
            position_ms: 41_000,
            duration_ms: TRACKS[0].3,
            position_sampled_at_ms: unix_ms(),
            artwork: Some(art.clone()),
            can_previous: true,
            can_next: true,
            can_toggle: true,
        };
        let device = |name: &str| AudioDevice { id: name.to_lowercase(), name: name.into() };
        let audio = AudioState {
            output_volume: 0.42,
            output_muted: false,
            mic_volume: 0.7,
            mic_muted: false,
            mic_in_use: false,
            output: Some(device("Speakers")),
            input: Some(device("Microphone")),
            outputs: vec![device("Speakers")],
            inputs: vec![device("Microphone")],
        };
        let power = PowerState { battery_percent: Some(82), charging: false, mode: PowerMode::Balanced, seconds_left: Some(3 * 3600 + 20 * 60) };
        let radios = RadioState { wifi: Some(true), bluetooth: Some(false) };

        let events = {
            let (media, audio) = (media.clone(), audio.clone());
            let now = unix_ms();
            vec![
                Event::Media(Some(media)),
                Event::Audio(audio),
                Event::Power(power),
                Event::Radios(radios),
                Event::Brightness(64),
                Event::Weather(Weather { temperature_c: 13.0, min_c: 8.0, max_c: 16.0, condition: "Light rain".into() }),
                Event::ClipboardAdded(ClipItem { id: 1, content: ClipContent::Text("cargo build --release -p naelbar".into()), captured_at_ms: now, pinned: false }),
                Event::ClipboardAdded(ClipItem { id: 2, content: ClipContent::Files(vec!["C:\\Users\\demo\\report.pdf".into(), "C:\\Users\\demo\\photo.jpg".into()]), captured_at_ms: now, pinned: false }),
                Event::ClipboardAdded(ClipItem { id: 3, content: ClipContent::Image { png: art, width: 96, height: 96 }, captured_at_ms: now, pinned: false }),
                Event::ClipboardAdded(ClipItem { id: 4, content: ClipContent::Text("The ferry leaves at seven, so meet at the quay a little before then.".into()), captured_at_ms: now, pinned: false }),
            ]
        };
        // Exercises the cross-thread path; the thread ends right after.
        let feeder = sink.clone();
        thread::spawn(move || {
            for e in events {
                feeder(e);
            }
        });

        Self { sink, media, audio, radios, stats_on: Arc::new(AtomicBool::new(false)), track: 0 }
    }

    fn emit_media(&mut self) {
        self.media.position_sampled_at_ms = unix_ms();
        (self.sink)(Event::Media(Some(self.media.clone())));
    }

    fn position_now(&self) -> u64 {
        if self.media.status == PlaybackStatus::Playing {
            (self.media.position_ms + unix_ms().saturating_sub(self.media.position_sampled_at_ms)).min(self.media.duration_ms)
        } else {
            self.media.position_ms
        }
    }

    fn change_track(&mut self, step: usize) {
        self.track = (self.track + step) % TRACKS.len();
        let (title, artist, album, duration) = TRACKS[self.track];
        self.media.title = title.into();
        self.media.artist = artist.into();
        self.media.album = album.into();
        self.media.duration_ms = duration;
        self.media.position_ms = 0;
        self.media.status = PlaybackStatus::Playing;
        self.emit_media();
    }

    fn execute(&mut self, cmd: &Command) {
        match cmd {
            Command::MediaToggle => {
                self.media.position_ms = self.position_now();
                self.media.status = if self.media.status == PlaybackStatus::Playing { PlaybackStatus::Paused } else { PlaybackStatus::Playing };
                self.emit_media();
            }
            Command::MediaNext => self.change_track(1),
            Command::MediaPrevious => self.change_track(TRACKS.len() - 1),
            Command::MediaSeek(ms) => {
                self.media.position_ms = *ms;
                self.emit_media();
            }
            Command::SetVolume(v) => {
                self.audio.output_volume = *v;
                (self.sink)(Event::Audio(self.audio.clone()));
            }
            Command::SetMuted(m) => {
                self.audio.output_muted = *m;
                (self.sink)(Event::Audio(self.audio.clone()));
            }
            Command::SetMicMuted(m) => {
                self.audio.mic_muted = *m;
                (self.sink)(Event::Audio(self.audio.clone()));
            }
            Command::SetBrightness(_, b) => (self.sink)(Event::Brightness(*b)),
            Command::SetWifi(on) => {
                self.radios.wifi = Some(*on);
                (self.sink)(Event::Radios(self.radios));
            }
            Command::SetBluetooth(on) => {
                self.radios.bluetooth = Some(*on);
                (self.sink)(Event::Radios(self.radios));
            }
            Command::StatsSubscribe(on) => {
                let was_on = self.stats_on.swap(*on, Ordering::SeqCst);
                if *on && !was_on {
                    let sink = self.sink.clone();
                    let flag = self.stats_on.clone();
                    thread::spawn(move || {
                        let mut n = 0u32;
                        while flag.load(Ordering::SeqCst) {
                            let wave = ((n as f32) * 0.7).sin() * 0.5 + 0.5;
                            sink(Event::Stats(SystemStats {
                                cpu_percent: 12.0 + wave * 30.0,
                                gpu_percent: Some(6.0 + wave * 18.0),
                                gpu_temp_c: Some(48.0),
                                gpu_mhz: Some(1200),
                                cpu_mhz: Some(3100),
                                cpu_temp_c: Some(61.0),
                                ram_percent: 46.0,
                                ram_used_bytes: 14_800_000_000,
                                ram_total_bytes: 32_000_000_000,
                                net_down_bps: (120_000.0 + wave * 900_000.0) as u64,
                                net_up_bps: (20_000.0 + wave * 60_000.0) as u64,
                            }));
                            n += 1;
                            thread::sleep(Duration::from_millis(1000));
                        }
                    });
                }
            }
            _ => {}
        }
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// A generated warm gradient, stored uncompressed.
fn demo_png() -> Vec<u8> {
    const SIZE: usize = 96;
    let mut raw = Vec::with_capacity(SIZE * (SIZE * 3 + 1));
    for y in 0..SIZE {
        raw.push(0);
        for x in 0..SIZE {
            let (fx, fy) = (x as f32 / SIZE as f32, y as f32 / SIZE as f32);
            let r = 60.0 + 180.0 * (1.0 - fy) * (0.4 + 0.6 * fx);
            let g = 40.0 + 110.0 * (1.0 - fy) * fx;
            let b = 70.0 + 50.0 * fy;
            raw.extend_from_slice(&[r as u8, g as u8, b as u8]);
        }
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(0xFFFF).peekable();
    while let Some(c) = chunks.next() {
        z.push(chunks.peek().is_none() as u8);
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(SIZE as u32).to_be_bytes());
    ihdr.extend_from_slice(&(SIZE as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &z);
    png_chunk(&mut out, b"IEND", &[]);
    out
}
