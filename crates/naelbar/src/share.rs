//! Send one shelf file to a phone. A tiny HTTP server listens on the LAN
//! address (never on all interfaces) and serves only that file, at
//! `/<128-bit random token>/<file name>`. It stops after the first complete
//! download or after ten minutes. The only timer is that ten-minute wait, a
//! kernel wait on a condition variable. Everything else blocks in `accept`.

use std::io::{Read, Write};
use std::os::windows::io::IntoRawSocket;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};

use crate::shelf_store::notify;

/// How long the link lives if nobody downloads the file.
pub const LIFETIME: Duration = Duration::from_secs(10 * 60);
/// Longest file name placed in the URL, which keeps the QR code small.
const URL_NAME_MAX: usize = 48;
const CHUNK: usize = 256 * 1024;
/// Connections served at the same time.
const MAX_CONNS: usize = 4;

/// The raw listening socket, kept so that whoever stops the server can close
/// it directly: the thread blocked in `accept` then fails and ends, without
/// anything connecting to us (which would not work if the LAN IP is gone).
/// Whoever swaps the value out first closes it; the other does nothing.
#[derive(Default)]
pub(crate) struct SockHandle(AtomicUsize);

impl SockHandle {
    pub(crate) fn new(l: &TcpListener) -> Self {
        use std::os::windows::io::AsRawSocket;
        Self(AtomicUsize::new(l.as_raw_socket() as usize))
    }

    /// Closes the socket now (wakes a blocked `accept`). Safe to call twice.
    pub(crate) fn close(&self) {
        let raw = self.0.swap(0, Ordering::AcqRel);
        if raw != 0 {
            close_raw(raw);
        }
    }

    /// Called by the accept thread when it ends: closes the listener unless
    /// [`close`](Self::close) already did.
    pub(crate) fn finish(&self, listener: TcpListener) {
        let raw = listener.into_raw_socket() as usize;
        if self.0.compare_exchange(raw, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            close_raw(raw);
        }
    }
}

fn close_raw(raw: usize) {
    use windows::Win32::Networking::WinSock::{SOCKET, closesocket};
    unsafe {
        closesocket(SOCKET(raw));
    }
}

/// A peer is allowed when it is the machine itself or sits on the same
/// private subnet as `local`: /24 inside 192.168/16, /16 inside 10/8 and
/// 172.16/12 (the usual home and office sizes).
pub(crate) fn same_subnet(local: Ipv4Addr, peer: Ipv4Addr) -> bool {
    if peer == local {
        return true;
    }
    if !is_lan(local) || !is_lan(peer) {
        return false;
    }
    let (l, p) = (local.octets(), peer.octets());
    if l[0] == 192 {
        l[..3] == p[..3]
    } else {
        l[..2] == p[..2]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Waiting,
    Done,
    Expired,
    /// No private network address, or the file could not be shared.
    Failed,
}

/// What the card draws.
#[derive(Clone, Debug)]
pub struct Card {
    pub name: String,
    /// `ip:port`, shown under the code.
    pub host: String,
    /// Side of the code in modules, quiet zone not included.
    pub modules: usize,
    /// Dark horizontal runs as (x, y, length), in modules.
    pub runs: Arc<[(u16, u16, u16)]>,
    pub status: Status,
}

struct Control {
    id: u64,
    stopped: Mutex<bool>,
    cv: Condvar,
    sock: SockHandle,
}

impl Control {
    fn is_stopped(&self) -> bool {
        *self.stopped.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ends the session: wakes the timer and the thread blocked in `accept`.
    fn stop(&self) {
        *self.stopped.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_all();
        self.sock.close();
    }
}

struct Session {
    card: Card,
    ctl: Arc<Control>,
}

static CURRENT: Mutex<Option<Session>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn current_lock() -> std::sync::MutexGuard<'static, Option<Session>> {
    CURRENT.lock().unwrap_or_else(|e| e.into_inner())
}

/// The transfer to draw, if one was started and not dismissed.
pub fn current() -> Option<Card> {
    current_lock().as_ref().map(|s| s.card.clone())
}

/// Ends the transfer and removes the card.
pub fn stop() {
    if let Some(s) = current_lock().take() {
        s.ctl.stop();
    }
}

fn set_status(id: u64, status: Status, ctrl: isize) {
    if let Some(s) = current_lock().as_mut().filter(|s| s.ctl.id == id) {
        // A finished session keeps its result.
        if s.card.status == Status::Waiting {
            s.card.status = status;
        }
    }
    notify(ctrl, 2);
}

/// Starts serving `path`. `ctrl` is told whenever the status changes.
pub fn start(path: PathBuf, ctrl: isize) {
    stop();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let failed = |_: Option<()>| Session {
        card: Card { name: name.clone(), host: String::new(), modules: 0, runs: Arc::from(Vec::new()), status: Status::Failed },
        ctl: Arc::new(Control { id, stopped: Mutex::new(true), cv: Condvar::new(), sock: SockHandle::default() }),
    };
    if !path.is_file() {
        *current_lock() = Some(failed(None));
        return;
    }
    let Some(ip) = lan_ip() else {
        *current_lock() = Some(failed(None));
        return;
    };
    let Ok(listener) = TcpListener::bind(SocketAddr::new(IpAddr::V4(ip), 0)) else {
        *current_lock() = Some(failed(None));
        return;
    };
    let Ok(addr) = listener.local_addr() else {
        *current_lock() = Some(failed(None));
        return;
    };
    let Some(token) = new_token() else {
        *current_lock() = Some(failed(None));
        return;
    };
    let url_name = url_name(&name);
    let url = build_url(addr, &token, &url_name);
    let Ok(qr) = qrcodegen::QrCode::encode_text(&url, qrcodegen::QrCodeEcc::Low) else {
        *current_lock() = Some(failed(None));
        return;
    };
    let ctl = Arc::new(Control { id, stopped: Mutex::new(false), cv: Condvar::new(), sock: SockHandle::new(&listener) });
    let card = Card { name: name.clone(), host: addr.to_string(), modules: qr.size() as usize, runs: qr_runs(&qr).into(), status: Status::Waiting };
    *current_lock() = Some(Session { card, ctl: ctl.clone() });

    let job = Job { path, token, url_name, name, ctl: ctl.clone(), ctrl, local: ip, conns: AtomicUsize::new(0) };
    let job = Arc::new(job);
    let spawned = {
        let job = job.clone();
        std::thread::Builder::new().name("share-accept".into()).spawn(move || accept_loop(listener, job))
    };
    let timer = {
        let job = job.clone();
        std::thread::Builder::new().name("share-timer".into()).spawn(move || expire_after(LIFETIME, &job))
    };
    if spawned.is_err() || timer.is_err() {
        ctl.stop();
        set_status(id, Status::Failed, ctrl);
    }
}

struct Job {
    path: PathBuf,
    token: String,
    url_name: String,
    name: String,
    ctl: Arc<Control>,
    ctrl: isize,
    /// Our LAN address: peers must share its subnet.
    local: Ipv4Addr,
    conns: AtomicUsize,
}

/// The one timer: waits for the stop signal for at most `after`.
fn expire_after(after: Duration, job: &Job) {
    let g = job.ctl.stopped.lock().unwrap_or_else(|e| e.into_inner());
    let (g, res) = job.ctl.cv.wait_timeout_while(g, after, |stopped| !*stopped).unwrap_or_else(|e| e.into_inner());
    drop(g);
    if res.timed_out() {
        set_status(job.ctl.id, Status::Expired, job.ctrl);
        job.ctl.stop();
    }
}

fn accept_loop(listener: TcpListener, job: Arc<Job>) {
    while let Ok((stream, peer)) = listener.accept() {
        if job.ctl.is_stopped() {
            break;
        }
        let allowed = matches!(peer.ip(), IpAddr::V4(p) if same_subnet(job.local, p));
        if !allowed || job.conns.load(Ordering::Acquire) >= MAX_CONNS {
            continue;
        }
        job.conns.fetch_add(1, Ordering::AcqRel);
        let j = job.clone();
        let spawned = std::thread::Builder::new().name("share-conn".into()).spawn(move || {
            if handle(stream, &j) {
                set_status(j.ctl.id, Status::Done, j.ctrl);
                j.ctl.stop();
            }
            j.conns.fetch_sub(1, Ordering::AcqRel);
        });
        if spawned.is_err() {
            job.conns.fetch_sub(1, Ordering::AcqRel);
        }
    }
    job.ctl.sock.finish(listener);
}

/// Answers one connection. True when the whole file went out.
fn handle(mut s: TcpStream, job: &Job) -> bool {
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    let Some(head) = read_head(&mut s) else { return false };
    let Some((method, target)) = parse_request(&head) else {
        let _ = s.write_all(&simple_response(400, "Bad Request"));
        return false;
    };
    match route(method, target, &job.token, &job.url_name) {
        Route::NotFound => {
            let _ = s.write_all(&simple_response(404, "Not Found"));
            false
        }
        Route::BadMethod => {
            let _ = s.write_all(&simple_response(405, "Method Not Allowed"));
            false
        }
        Route::File { head_only } => send_file(&mut s, job, head_only),
    }
}

fn send_file(s: &mut TcpStream, job: &Job, head_only: bool) -> bool {
    let Ok(mut f) = std::fs::File::open(&job.path) else {
        let _ = s.write_all(&simple_response(404, "Not Found"));
        return false;
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else { return false };
    if s.write_all(&file_response_head(&job.name, len)).is_err() {
        return false;
    }
    if head_only {
        return false;
    }
    let mut buf = vec![0u8; CHUNK];
    let mut sent = 0u64;
    while sent < len {
        if job.ctl.is_stopped() {
            return false;
        }
        let want = buf.len().min((len - sent) as usize);
        match f.read(&mut buf[..want]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => {
                if s.write_all(&buf[..n]).is_err() {
                    return false;
                }
                sent += n as u64;
            }
        }
    }
    let _ = s.flush();
    let _ = s.shutdown(std::net::Shutdown::Write);
    // The phone closing its side tells us it received everything.
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut sink = [0u8; 64];
    while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
    true
}

fn read_head(s: &mut TcpStream) -> Option<String> {
    let mut data = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !data.windows(4).any(|w| w == b"\r\n\r\n") {
        if data.len() > 8192 {
            return None;
        }
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => data.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8(data).ok()
}

// ---------------------------------------------------------------------------
// Pure helpers

/// First private IPv4 address of the interface that reaches the local network.
/// A UDP socket is only "connected": nothing is sent.
pub(crate) fn lan_ip() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if is_lan(ip) => Some(ip),
        _ => None,
    }
}

pub(crate) fn is_lan(ip: Ipv4Addr) -> bool {
    ip.is_private() && !ip.is_loopback()
}

/// 128 random bits as 32 lowercase hex digits.
pub fn new_token() -> Option<String> {
    let mut b = [0u8; 16];
    let status = unsafe { BCryptGenRandom(None, &mut b, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    status.is_ok().then(|| hex(&b))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The name as it appears in the URL: short enough for a small code, with its
/// extension kept.
pub fn url_name(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= URL_NAME_MAX {
        return name.to_string();
    }
    let ext = Path::new(name).extension().map(|e| e.to_string_lossy().into_owned()).filter(|e| e.chars().count() <= 8);
    match ext {
        Some(e) => {
            let keep = URL_NAME_MAX.saturating_sub(e.chars().count() + 1);
            let stem: String = chars.iter().take(keep).collect();
            format!("{stem}.{e}")
        }
        None => chars.iter().take(URL_NAME_MAX).collect(),
    }
}

pub fn build_url(addr: SocketAddr, token: &str, url_name: &str) -> String {
    format!("http://{addr}/{token}/{}", pct_encode(url_name))
}

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Comparison that does not stop at the first differing byte.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    File { head_only: bool },
    NotFound,
    BadMethod,
}

/// Splits `GET /path HTTP/1.1` out of the request head.
pub fn parse_request(head: &str) -> Option<(&str, &str)> {
    let line = head.lines().next()?;
    let mut it = line.split(' ');
    let (method, target, version) = (it.next()?, it.next()?, it.next()?);
    (version.starts_with("HTTP/1.") && it.next().is_none() && !method.is_empty()).then_some((method, target))
}

/// Decides what a request gets. The request path is only ever compared with the
/// one expected path, never turned into a file path, so nothing else on the
/// disk can be reached.
pub fn route(method: &str, target: &str, token: &str, url_name: &str) -> Route {
    let path = target.split(['?', '#']).next().unwrap_or("");
    let Some(decoded) = pct_decode(path) else { return Route::NotFound };
    let expected = format!("/{token}/{url_name}");
    if !ct_eq(decoded.as_bytes(), expected.as_bytes()) {
        return Route::NotFound;
    }
    match method {
        "GET" => Route::File { head_only: false },
        "HEAD" => Route::File { head_only: true },
        _ => Route::BadMethod,
    }
}

fn simple_response(code: u16, reason: &str) -> Vec<u8> {
    format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n").into_bytes()
}

fn file_response_head(name: &str, len: u64) -> Vec<u8> {
    let ascii: String = name.chars().map(|c| if c.is_ascii_graphic() && !matches!(c, '"' | '\\' | '%') || c == ' ' { c } else { '_' }).collect();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {len}\r\nContent-Disposition: attachment; filename=\"{ascii}\"; filename*=UTF-8''{}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        content_type(name),
        pct_encode(name)
    )
    .into_bytes()
}

fn content_type(name: &str) -> &'static str {
    let ext = Path::new(name).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Merges neighbouring dark modules of each row into one rectangle.
pub(crate) fn qr_runs(qr: &qrcodegen::QrCode) -> Vec<(u16, u16, u16)> {
    let n = qr.size();
    let mut runs = Vec::new();
    for y in 0..n {
        let mut x = 0;
        while x < n {
            if qr.get_module(x, y) {
                let start = x;
                while x < n && qr.get_module(x, y) {
                    x += 1;
                }
                runs.push((start as u16, y as u16, (x - start) as u16));
            } else {
                x += 1;
            }
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn tokens_are_128_bits_of_hex_and_never_repeat() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
        assert_ne!(a, "0".repeat(32));
    }

    #[test]
    fn url_has_the_token_then_the_encoded_name() {
        let addr: SocketAddr = "192.168.1.20:51000".parse().unwrap();
        assert_eq!(build_url(addr, TOKEN, "photo.png"), format!("http://192.168.1.20:51000/{TOKEN}/photo.png"));
        assert_eq!(build_url(addr, TOKEN, "Capture d'écran.png"), format!("http://192.168.1.20:51000/{TOKEN}/Capture%20d%27%C3%A9cran.png"));
    }

    #[test]
    fn long_names_are_cut_but_keep_their_extension() {
        assert_eq!(url_name("a.txt"), "a.txt");
        let long = format!("{}.jpeg", "x".repeat(200));
        let short = url_name(&long);
        assert_eq!(short.chars().count(), URL_NAME_MAX);
        assert!(short.ends_with(".jpeg"));
        assert_eq!(url_name(&"y".repeat(100)).chars().count(), URL_NAME_MAX);
    }

    #[test]
    fn only_the_exact_path_is_served() {
        let ok = |m: &str, t: &str| route(m, t, TOKEN, "my file.png");
        let good = format!("/{TOKEN}/my%20file.png");
        assert_eq!(ok("GET", &good), Route::File { head_only: false });
        assert_eq!(ok("HEAD", &good), Route::File { head_only: true });
        assert_eq!(ok("GET", &format!("{good}?x=1")), Route::File { head_only: false });
        assert_eq!(ok("GET", &format!("/{TOKEN}/my%20FILE.png")), Route::NotFound);
        assert_eq!(ok("POST", &good), Route::BadMethod);
        assert_eq!(ok("GET", "/"), Route::NotFound);
        assert_eq!(ok("GET", &format!("/{TOKEN}/")), Route::NotFound);
        assert_eq!(ok("GET", "/my%20file.png"), Route::NotFound);
        assert_eq!(ok("GET", &format!("/{}/my%20file.png", "f".repeat(32))), Route::NotFound);
    }

    #[test]
    fn traversal_attempts_never_match() {
        let r = |t: &str| route("GET", t, TOKEN, "a.txt");
        for t in [
            format!("/{TOKEN}/../a.txt"),
            format!("/{TOKEN}/..%2fa.txt"),
            format!("/{TOKEN}/%2e%2e/a.txt"),
            format!("/{TOKEN}/a.txt/../../Windows/win.ini"),
            format!("/{TOKEN}/a.txt/"),
            format!("/{TOKEN}//a.txt"),
            format!("/{TOKEN}/..\\a.txt"),
            "/C:/Windows/win.ini".to_string(),
            format!("/{TOKEN}/a.txt%00.png"),
            format!("/{TOKEN}/%zz"),
            format!("/{TOKEN}/%ff"),
        ] {
            assert_eq!(r(&t), Route::NotFound, "{t}");
        }
    }

    #[test]
    fn request_line_is_parsed_strictly() {
        assert_eq!(parse_request("GET /x HTTP/1.1\r\nHost: a\r\n\r\n"), Some(("GET", "/x")));
        assert_eq!(parse_request("GET /x HTTP/1.0\r\n\r\n"), Some(("GET", "/x")));
        assert_eq!(parse_request("GET /x\r\n\r\n"), None);
        assert_eq!(parse_request("GET /x FTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_request("GET /x HTTP/1.1 extra\r\n\r\n"), None);
        assert_eq!(parse_request(""), None);
    }

    #[test]
    fn only_private_addresses_are_used() {
        assert!(is_lan("192.168.0.5".parse().unwrap()));
        assert!(is_lan("10.1.2.3".parse().unwrap()));
        assert!(is_lan("172.20.0.1".parse().unwrap()));
        assert!(!is_lan("127.0.0.1".parse().unwrap()));
        assert!(!is_lan("0.0.0.0".parse().unwrap()));
        assert!(!is_lan("8.8.8.8".parse().unwrap()));
        assert!(!is_lan("169.254.3.4".parse().unwrap()));
    }

    #[test]
    fn response_head_is_well_formed() {
        let h = String::from_utf8(file_response_head("é \"q\".png", 42)).unwrap();
        assert!(h.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(h.contains("Content-Length: 42\r\n"));
        assert!(h.contains("Content-Type: image/png\r\n"));
        assert!(h.contains("filename=\"_ _q_.png\""));
        assert!(h.contains("filename*=UTF-8''%C3%A9%20%22q%22.png"));
        assert!(h.ends_with("\r\n\r\n"));
    }

    #[test]
    fn qr_runs_cover_every_dark_module_once() {
        let qr = qrcodegen::QrCode::encode_text("http://192.168.1.2:5000/abc/def.png", qrcodegen::QrCodeEcc::Low).unwrap();
        let dark: usize = (0..qr.size()).map(|y| (0..qr.size()).filter(|&x| qr.get_module(x, y)).count()).sum();
        let runs = qr_runs(&qr);
        assert_eq!(runs.iter().map(|r| r.2 as usize).sum::<usize>(), dark);
        assert!(runs.len() < dark);
    }

    /// Full round trip over the loopback interface, with the same handler.
    fn get(addr: SocketAddr, request: &str) -> Vec<u8> {
        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(request.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = c.read_to_end(&mut out);
        out
    }

    #[test]
    fn serves_the_file_once_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("naelbar-share-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        let secret = dir.join("secret.txt");
        std::fs::write(&file, b"hello phone").unwrap();
        std::fs::write(&secret, b"no").unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let ctl = Arc::new(Control { id: u64::MAX, stopped: Mutex::new(false), cv: Condvar::new(), sock: SockHandle::new(&listener) });
        let job = Arc::new(Job { path: file, token: TOKEN.into(), url_name: "hello.txt".into(), name: "hello.txt".into(), ctl: ctl.clone(), ctrl: 0, local: Ipv4Addr::LOCALHOST, conns: AtomicUsize::new(0) });
        let t = std::thread::spawn({
            let job = job.clone();
            move || accept_loop(listener, job)
        });

        let bad = get(addr, &format!("GET /{TOKEN}/secret.txt HTTP/1.1\r\n\r\n"));
        assert!(String::from_utf8_lossy(&bad).starts_with("HTTP/1.1 404"));
        let bad = get(addr, "GET /../secret.txt HTTP/1.1\r\n\r\n");
        assert!(String::from_utf8_lossy(&bad).starts_with("HTTP/1.1 404"));
        assert!(!ctl.is_stopped());

        let ok = get(addr, &format!("GET /{TOKEN}/hello.txt HTTP/1.1\r\nHost: x\r\n\r\n"));
        let text = String::from_utf8_lossy(&ok).into_owned();
        assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
        assert!(text.ends_with("hello phone"));
        t.join().unwrap();
        assert!(ctl.is_stopped(), "the server stops after the first complete download");
        assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_err() || get(addr, "x").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_ten_minute_timer_ends_the_session() {
        let ctl = Arc::new(Control { id: u64::MAX - 1, stopped: Mutex::new(false), cv: Condvar::new(), sock: SockHandle::default() });
        let job = Job { path: PathBuf::new(), token: TOKEN.into(), url_name: String::new(), name: String::new(), ctl: ctl.clone(), ctrl: 0, local: Ipv4Addr::LOCALHOST, conns: AtomicUsize::new(0) };
        expire_after(Duration::from_millis(50), &job);
        assert!(ctl.is_stopped());
        // Stopping early wakes the timer at once.
        let ctl = Arc::new(Control { id: u64::MAX - 2, stopped: Mutex::new(false), cv: Condvar::new(), sock: SockHandle::default() });
        let job = Arc::new(Job { path: PathBuf::new(), token: String::new(), url_name: String::new(), name: String::new(), ctl: ctl.clone(), ctrl: 0, local: Ipv4Addr::LOCALHOST, conns: AtomicUsize::new(0) });
        let t = std::thread::spawn({
            let job = job.clone();
            move || expire_after(Duration::from_secs(600), &job)
        });
        std::thread::sleep(Duration::from_millis(50));
        ctl.stop();
        t.join().unwrap();
    }

    #[test]
    fn peers_must_share_the_private_subnet() {
        let me = Ipv4Addr::new(192, 168, 1, 20);
        assert!(same_subnet(me, Ipv4Addr::new(192, 168, 1, 77)));
        assert!(!same_subnet(me, Ipv4Addr::new(192, 168, 2, 77)));
        assert!(!same_subnet(me, Ipv4Addr::new(8, 8, 8, 8)));
        assert!(same_subnet(Ipv4Addr::new(10, 1, 2, 3), Ipv4Addr::new(10, 1, 9, 9)));
        assert!(!same_subnet(Ipv4Addr::new(10, 1, 2, 3), Ipv4Addr::new(10, 2, 2, 3)));
        assert!(same_subnet(Ipv4Addr::LOCALHOST, Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn closing_the_socket_ends_a_blocked_accept() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let ctl = Arc::new(Control { id: u64::MAX - 3, stopped: Mutex::new(false), cv: Condvar::new(), sock: SockHandle::new(&listener) });
        let job = Arc::new(Job { path: PathBuf::new(), token: String::new(), url_name: String::new(), name: String::new(), ctl: ctl.clone(), ctrl: 0, local: Ipv4Addr::LOCALHOST, conns: AtomicUsize::new(0) });
        let t = std::thread::spawn({
            let job = job.clone();
            move || accept_loop(listener, job)
        });
        std::thread::sleep(Duration::from_millis(100));
        ctl.stop();
        t.join().unwrap();
    }
}
