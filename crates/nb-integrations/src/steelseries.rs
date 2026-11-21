//! SteelSeries GG Sonar channel volumes.
//!
//! Sonar's local HTTP API is NOT documented by SteelSeries: it is the one
//! community tools use, and a GG update may change or remove it. Everything
//! here runs on short-lived threads, only when the quick tab asks for it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nb_core::{Event, Sink, SonarChannel};

/// Channels shown, in order, with Sonar's ids.
pub const CHANNELS: [&str; 5] = ["game", "chatRender", "media", "aux", "chatCapture"];

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const TIMEOUT: Duration = Duration::from_millis(800);

static BASE: Mutex<Option<String>> = Mutex::new(None);
static PENDING: Mutex<Vec<(&'static str, f32)>> = Mutex::new(Vec::new());
static WRITING: AtomicBool = AtomicBool::new(false);
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Sonar's `/volumeSettings/classic` answer, reduced to the shown channels.
pub fn parse_volumes(json: &str) -> Option<Vec<SonarChannel>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let devices = v.get("devices")?;
    let list: Vec<SonarChannel> = CHANNELS
        .iter()
        .filter_map(|id| {
            let c = devices.get(*id)?.get("classic")?;
            let volume = c.get("volume")?.as_f64()? as f32;
            let muted = c.get("muted").and_then(|m| m.as_bool()).unwrap_or(false);
            volume.is_finite().then(|| SonarChannel { id: (*id).to_string(), volume: volume.clamp(0.0, 1.0), muted })
        })
        .collect();
    (!list.is_empty()).then_some(list)
}

/// The Sonar web server address from GG's `/subApps` answer. Only a loopback
/// HTTP address is accepted.
pub fn parse_sonar_address(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let addr = v.get("subApps")?.get("sonar")?.get("metadata")?.get("webServerAddress")?.as_str()?;
    addr.strip_prefix("http://127.0.0.1:")?.parse::<u16>().ok()?;
    Some(addr.to_string())
}

fn discover() -> Option<String> {
    let props = std::path::PathBuf::from(std::env::var_os("ProgramData")?).join("SteelSeries").join("GG").join("coreProps.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(props).ok()?).ok()?;
    let gg = v.get("ggEncryptedAddress")?.as_str()?;
    gg.strip_prefix("127.0.0.1:")?.parse::<u16>().ok()?;
    // GG's own server is HTTPS with a self-signed certificate; curl.exe ships with Windows.
    let out = Command::new("curl.exe")
        .args(["-sk", "--max-time", "2", &format!("https://{gg}/subApps")])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    parse_sonar_address(&String::from_utf8_lossy(&out.stdout))
}

/// Small journal for debugging the Sonar link, in the naelbar folder of LOCALAPPDATA (sonar.log),
/// emptied when it passes 64 KB. Holds addresses, channel ids, volumes and
/// status codes only.
fn log(msg: &str) {
    use std::io::Write;
    let Some(dir) = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("naelbar")) else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("sonar.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 64 * 1024) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() % 86_400_000).unwrap_or(0);
        let _ = writeln!(f, "[{:02}:{:02}:{:02}.{:03}] {msg}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    }
}

fn http(base: &str, method: &str, path: &str) -> Option<String> {
    let started = std::time::Instant::now();
    let r = http_raw(base, method, path);
    if method != "GET" || r.is_none() {
        log(&format!("{method} {path} -> {} en {} ms", if r.is_some() { "200" } else { "échec" }, started.elapsed().as_millis()));
    }
    r
}

fn http_raw(base: &str, method: &str, path: &str) -> Option<String> {
    let hostport = base.strip_prefix("http://")?;
    let mut s = TcpStream::connect_timeout(&hostport.parse().ok()?, TIMEOUT).ok()?;
    s.set_read_timeout(Some(TIMEOUT)).ok()?;
    s.set_write_timeout(Some(TIMEOUT)).ok()?;
    write!(s, "{method} {path} HTTP/1.1\r\nHost: {hostport}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").ok()?;
    let mut buf = Vec::new();
    s.take(1 << 20).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n")?;
    head.starts_with("HTTP/1.1 200").then(|| body.to_string())
}

fn base() -> Option<String> {
    if let Some(b) = BASE.lock().unwrap().clone() {
        return Some(b);
    }
    let b = discover();
    log(&format!("découverte de Sonar: {}", b.as_deref().unwrap_or("introuvable")));
    let b = b?;
    *BASE.lock().unwrap() = Some(b.clone());
    Some(b)
}

fn read() -> Option<Vec<SonarChannel>> {
    base().and_then(|b| http(&b, "GET", "/volumeSettings/classic")).and_then(|j| parse_volumes(&j))
}

/// Reads the volumes on a short-lived thread and emits `Event::Sonar`
/// (empty when Sonar is not reachable).
pub fn sonar_refresh(sink: Sink) {
    *SINK.lock().unwrap() = Some(sink.clone());
    std::thread::spawn(move || {
        let list = read().or_else(|| {
            // The port changes when GG restarts: rediscover once.
            *BASE.lock().unwrap() = None;
            read()
        });
        sink(Event::Sonar(list.unwrap_or_default()));
    });
}

/// Sets a channel volume (0 to 1). Calls made during a drag coalesce: only
/// the latest value per channel is sent.
pub fn sonar_set_volume(channel: &str, volume: f32) {
    let Some(id) = CHANNELS.iter().copied().find(|c| *c == channel) else { return };
    {
        let mut p = PENDING.lock().unwrap();
        p.retain(|(c, _)| *c != id);
        p.push((id, volume.clamp(0.0, 1.0)));
    }
    if WRITING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(|| {
        loop {
            let jobs: Vec<(&str, f32)> = std::mem::take(&mut *PENDING.lock().unwrap());
            if jobs.is_empty() {
                WRITING.store(false, Ordering::Release);
                if PENDING.lock().unwrap().is_empty() || WRITING.swap(true, Ordering::AcqRel) {
                    // Show what Sonar really applied.
                    let sink = SINK.lock().unwrap().clone();
                    if let (Some(sink), Some(list)) = (sink, read()) {
                        sink(Event::Sonar(list));
                    }
                    return;
                }
                continue;
            }
            let Some(b) = base() else {
                WRITING.store(false, Ordering::Release);
                return;
            };
            for (id, v) in jobs {
                let path = format!("/volumeSettings/classic/{id}/Volume/{v:.4}");
                if http(&b, "PUT", &path).is_none() {
                    // The port changes when GG restarts: forget it, rediscover, try once more.
                    *BASE.lock().unwrap() = None;
                    if let Some(b2) = base() {
                        let _ = http(&b2, "PUT", &path);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_shown_channels_in_order() {
        let j = r#"{"masters":{"classic":{"volume":0.52,"muted":false}},"devices":{"game":{"stream":{},"classic":{"volume":0.11,"muted":true}},"chatRender":{"classic":{"volume":0.19,"muted":false}},"chatCapture":{"classic":{"volume":1.0,"muted":false}},"media":{"classic":{"volume":0.3,"muted":false}},"aux":{"classic":{"volume":1.7,"muted":false}}}}"#;
        let l = parse_volumes(j).unwrap();
        assert_eq!(l.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), CHANNELS);
        assert!(l[0].muted);
        assert_eq!(l[3].volume, 1.0, "clamped");
        assert!(parse_volumes("{}").is_none());
        assert!(parse_volumes("not json").is_none());
    }

    #[test]
    fn only_a_loopback_sonar_address_is_kept() {
        let ok = r#"{"subApps":{"sonar":{"metadata":{"webServerAddress":"http://127.0.0.1:56792"}}}}"#;
        assert_eq!(parse_sonar_address(ok).as_deref(), Some("http://127.0.0.1:56792"));
        let far = r#"{"subApps":{"sonar":{"metadata":{"webServerAddress":"http://10.0.0.2:56792"}}}}"#;
        assert!(parse_sonar_address(far).is_none());
        let junk = r#"{"subApps":{"sonar":{"metadata":{"webServerAddress":"http://127.0.0.1:x/y"}}}}"#;
        assert!(parse_sonar_address(junk).is_none());
    }
}
