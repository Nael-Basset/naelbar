//! Loopback HTTP endpoint: one thread parked in a blocking `accept`.

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use nb_core::{AgentStatus, AgentUpdate, Event, Notification, Sink};
use serde_json::Value;

use crate::http::{self, Limits, ParseError, Request};
use crate::{Error, now_ms};

pub const DEFAULT_PORT: u16 = 47911;

const MAX_TITLE: usize = 120;
const MAX_BODY: usize = 500;
const MAX_AGENT: usize = 32;
const MAX_SESSION: usize = 64;
const MAX_DETAIL: usize = 200;

#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    /// 0 picks a free port (see [`Handle::port`]).
    pub port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Config { enabled: true, port: DEFAULT_PORT }
    }
}

/// Dropping stops the endpoint and joins its thread.
pub struct Handle {
    port: Option<u16>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Handle {
    /// Bound port, or `None` when the endpoint is disabled.
    pub fn port(&self) -> Option<u16> {
        self.port
    }
}

/// Starts the endpoint. With `enabled = false` no socket or thread exists.
pub fn start(sink: Sink, config: Config) -> Result<Handle, Error> {
    let stop = Arc::new(AtomicBool::new(false));
    if !config.enabled {
        return Ok(Handle { port: None, stop, thread: None });
    }
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, config.port)))?;
    let port = listener.local_addr()?.port();
    let stop2 = stop.clone();
    let thread = std::thread::Builder::new()
        .name("nb-integrations-http".into())
        .spawn(move || serve(listener, sink, stop2))?;
    Ok(Handle { port: Some(port), stop, thread: Some(thread) })
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let (Some(port), Some(t)) = (self.port, self.thread.take()) {
            // Wake the parked accept with a throwaway connection.
            let _ = TcpStream::connect_timeout(
                &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                Duration::from_millis(500),
            );
            let _ = t.join();
        }
    }
}

fn serve(listener: TcpListener, sink: Sink, stop: Arc<AtomicBool>) {
    let limits = Limits::default();
    loop {
        let conn = listener.accept();
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok((mut stream, peer)) = conn else { continue };
        if !peer.ip().is_loopback() {
            continue;
        }
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
        let (code, reason, text) = match http::read_request(&mut stream, &limits) {
            Ok(req) => handle_request(&req, &sink),
            Err(e) => respond_error(e),
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{text}",
            text.len()
        );
        let _ = stream.flush();
    }
}

fn respond_error(e: ParseError) -> (u16, &'static str, &'static str) {
    let (c, r) = e.status();
    (c, r, "")
}

type Reply = (u16, &'static str, &'static str);

const EMPTY_JSON: &str = "{}";

/// A browser extension, which a web page cannot pretend to be.
fn is_extension(origin: &str) -> bool {
    origin.starts_with("chrome-extension://") || origin.starts_with("moz-extension://")
}

/// `ytm-` and 24 lowercase hex digits, as the extension makes them.
pub fn valid_ytm_room(s: &str) -> bool {
    s.len() == 28 && s.starts_with("ytm-") && s[4..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Pure request handler, separated from I/O for testing.
pub(crate) fn handle_request(req: &Request, sink: &Sink) -> Reply {
    // Web pages are refused. Only the room report may come from an extension.
    match req.header("origin") {
        None => {}
        Some(o) if req.path == "/ytm/room" && is_extension(o) => {}
        Some(_) => return (403, "Forbidden", "browser requests are refused"),
    }
    match req.header("host") {
        Some(h) if http::host_is_loopback(h) => {}
        _ => return (403, "Forbidden", "bad host"),
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET" | "POST", "/notify") => notify(req, sink),
        ("POST", "/agent") => agent(req, sink),
        ("POST", "/claude/usage") => {
            if !req.oversized {
                if let Some(u) = crate::claude::parse_usage(&req.body, now_ms()) {
                    sink(Event::ClaudeUsage(u));
                }
            }
            (200, "OK", EMPTY_JSON)
        }
        ("POST", "/claude/hook") => {
            if !req.oversized {
                if let Some(h) = crate::claude::parse_hook(&req.body, now_ms()) {
                    sink(Event::ClaudeHook(h));
                }
            }
            (200, "OK", EMPTY_JSON)
        }
        ("POST", "/ytm/room") => {
            let room = String::from_utf8_lossy(&req.body);
            let room = room.trim();
            if req.oversized || !valid_ytm_room(room) {
                return (400, "Bad Request", "");
            }
            sink(Event::YtmRoom(room.to_string()));
            (204, "No Content", "")
        }
        (_, "/notify" | "/agent" | "/claude/usage" | "/claude/hook" | "/ytm/room") => (405, "Method Not Allowed", ""),
        _ => (404, "Not Found", ""),
    }
}

fn notify(req: &Request, sink: &Sink) -> Reply {
    let mut pairs = http::parse_pairs(&req.query);
    if req.method == "POST" && !req.body.is_empty() {
        pairs.extend(http::parse_pairs(&String::from_utf8_lossy(&req.body)));
    }
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap_or("");
    let title = http::sanitize(get("title"), MAX_TITLE);
    let body = http::sanitize(get("body"), MAX_BODY);
    if title.is_empty() && body.is_empty() {
        return (400, "Bad Request", "title or body required");
    }
    let source = http::sanitize(get("source"), MAX_AGENT);
    sink(Event::Notification(Notification {
        source: if source.is_empty() { "local".into() } else { source },
        title,
        body,
        received_at_ms: now_ms(),
        app_id: String::new(),
    }));
    (204, "No Content", "")
}

pub(crate) fn parse_status(s: &str) -> Option<AgentStatus> {
    match s.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
        "working" | "busy" | "running" => Some(AgentStatus::Working),
        "needs_input" | "needsinput" | "input" | "waiting" => Some(AgentStatus::NeedsInput),
        "finished" | "done" | "idle" | "stopped" => Some(AgentStatus::Finished),
        _ => None,
    }
}

fn agent(req: &Request, sink: &Sink) -> Reply {
    let json: Value = if req.body.is_empty() {
        Value::Object(Default::default())
    } else {
        match serde_json::from_slice(&req.body) {
            Ok(v @ Value::Object(_)) => v,
            _ => return (400, "Bad Request", "invalid json"),
        }
    };
    let q = http::parse_pairs(&req.query);
    let field = |keys: &[&str]| -> String {
        for k in keys {
            if let Some(s) = json.get(*k).and_then(Value::as_str) {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
            if let Some((_, v)) = q.iter().find(|(n, _)| n == k) {
                if !v.is_empty() {
                    return v.clone();
                }
            }
        }
        String::new()
    };
    // Explicit fields win; Claude Code hook stdin fields (session_id, message,
    // cwd) are accepted as fallbacks so a hook can pipe its stdin through.
    let agent = http::sanitize(&field(&["agent"]), MAX_AGENT);
    let session = http::sanitize(&field(&["session", "session_id"]), MAX_SESSION);
    let mut detail = http::sanitize(&field(&["detail", "message"]), MAX_DETAIL);
    if detail.is_empty() {
        let cwd = field(&["cwd"]);
        detail = http::sanitize(cwd.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or(""), MAX_DETAIL);
    }
    let Some(status) = parse_status(&field(&["status"])) else {
        return (400, "Bad Request", "invalid status");
    };
    if agent.is_empty() {
        return (400, "Bad Request", "agent required");
    }
    sink(Event::Agent(AgentUpdate { agent, session, status, detail }));
    (204, "No Content", "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Mutex;

    fn collector() -> (Sink, Arc<Mutex<Vec<Event>>>) {
        let v = Arc::new(Mutex::new(Vec::new()));
        let v2 = v.clone();
        (Arc::new(move |e| v2.lock().unwrap().push(e)), v)
    }

    fn req(method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Request {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        Request {
            method: method.into(),
            path: path.into(),
            query: query.into(),
            headers: headers.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            body: body.to_vec(),
            oversized: false,
        }
    }

    const H: (&str, &str) = ("Host", "127.0.0.1:47911");

    #[test]
    fn notify_ok_and_truncation() {
        let (sink, ev) = collector();
        let long = "x".repeat(2000);
        let r = req("GET", &format!("/notify?title=Hi%20there&body={long}"), &[H], b"");
        assert_eq!(handle_request(&r, &sink).0, 204);
        let ev = ev.lock().unwrap();
        let Event::Notification(n) = &ev[0] else { panic!() };
        assert_eq!(n.title, "Hi there");
        assert_eq!(n.body.chars().count(), MAX_BODY);
    }

    #[test]
    fn notify_requires_content() {
        let (sink, ev) = collector();
        assert_eq!(handle_request(&req("GET", "/notify", &[H], b""), &sink).0, 400);
        assert!(ev.lock().unwrap().is_empty());
    }

    #[test]
    fn rejects_origin() {
        let (sink, ev) = collector();
        let r = req("POST", "/notify?title=a", &[H, ("Origin", "https://evil.com")], b"");
        assert_eq!(handle_request(&r, &sink).0, 403);
        let r = req("GET", "/notify?title=a", &[H, ("origin", "null")], b"");
        assert_eq!(handle_request(&r, &sink).0, 403);
        assert!(ev.lock().unwrap().is_empty());
    }

    #[test]
    fn rejects_foreign_or_missing_host() {
        let (sink, ev) = collector();
        for h in ["evil.com", "attacker.example:47911", "127.0.0.1.evil.com"] {
            let r = req("GET", "/notify?title=a", &[("Host", h)], b"");
            assert_eq!(handle_request(&r, &sink).0, 403, "{h}");
        }
        assert_eq!(handle_request(&req("GET", "/notify?title=a", &[], b""), &sink).0, 403);
        assert!(ev.lock().unwrap().is_empty());
    }

    #[test]
    fn agent_json_and_fallbacks() {
        let (sink, ev) = collector();
        let body = br#"{"agent":"claude","session":"s1","status":"needs_input","detail":"ok"}"#;
        assert_eq!(handle_request(&req("POST", "/agent", &[H], body), &sink).0, 204);
        // Claude Code hook stdin shape, with status and agent from the query.
        let body = br#"{"session_id":"abc","hook_event_name":"Notification","message":"Needs you","cwd":"C:\\AI\\naelbar"}"#;
        let r = req("POST", "/agent?agent=claude-code&status=needs_input", &[H], body);
        assert_eq!(handle_request(&r, &sink).0, 204);
        let ev = ev.lock().unwrap();
        let Event::Agent(a) = &ev[0] else { panic!() };
        assert_eq!((a.agent.as_str(), a.session.as_str(), a.status), ("claude", "s1", AgentStatus::NeedsInput));
        let Event::Agent(a) = &ev[1] else { panic!() };
        assert_eq!((a.session.as_str(), a.detail.as_str()), ("abc", "Needs you"));
    }

    #[test]
    fn agent_rejects_bad_input() {
        let (sink, ev) = collector();
        let bad = |b: &[u8]| handle_request(&req("POST", "/agent", &[H], b), &sink).0;
        assert_eq!(bad(b"not json"), 400);
        assert_eq!(bad(b"[1]"), 400);
        assert_eq!(bad(br#"{"agent":"a","status":"bogus"}"#), 400);
        assert_eq!(bad(br#"{"status":"working"}"#), 400);
        assert_eq!(handle_request(&req("GET", "/agent", &[H], b""), &sink).0, 405);
        assert_eq!(handle_request(&req("GET", "/nope", &[H], b""), &sink).0, 404);
        assert!(ev.lock().unwrap().is_empty());
    }

    fn raw(port: u16, data: &[u8]) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s.write_all(data).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    #[test]
    fn server_roundtrip_and_clean_shutdown() {
        let (sink, ev) = collector();
        let h = start(sink, Config { enabled: true, port: 0 }).unwrap();
        let port = h.port().unwrap();
        let ok = raw(port, format!("GET /notify?title=yo HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes());
        assert!(ok.starts_with("HTTP/1.1 204"), "{ok}");
        let bad = raw(port, b"GET /notify?title=yo HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://x\r\n\r\n");
        assert!(bad.starts_with("HTTP/1.1 403"), "{bad}");
        let big = raw(port, b"POST /agent HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 100000\r\n\r\n");
        assert!(big.starts_with("HTTP/1.1 413"), "{big}");
        assert_eq!(ev.lock().unwrap().len(), 1);
        let t = std::time::Instant::now();
        drop(h);
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    }

    #[test]
    fn claude_routes_protected_and_answer_empty_json() {
        let (sink, ev) = collector();
        let usage = br#"{"rate_limits":{"five_hour":{"used_percentage":34.4,"resets_at":1700000000}}}"#;
        let r = handle_request(&req("POST", "/claude/usage", &[H], usage), &sink);
        assert_eq!((r.0, r.2), (200, "{}"));
        let r = handle_request(&req("POST", "/claude/hook", &[H], b"garbage"), &sink);
        assert_eq!((r.0, r.2), (200, "{}"));
        let hook = br#"{"session_id":"s","cwd":"C:\\a\\proj","hook_event_name":"Stop"}"#;
        handle_request(&req("POST", "/claude/hook", &[H], hook), &sink);
        assert_eq!(ev.lock().unwrap().len(), 2);
        for path in ["/claude/usage", "/claude/hook"] {
            assert_eq!(handle_request(&req("POST", path, &[H, ("Origin", "http://x")], usage), &sink).0, 403);
            assert_eq!(handle_request(&req("POST", path, &[("Host", "evil.com")], usage), &sink).0, 403);
            assert_eq!(handle_request(&req("GET", path, &[H], b""), &sink).0, 405);
        }
        assert_eq!(ev.lock().unwrap().len(), 2);
    }

    #[test]
    fn claude_big_bodies_over_the_wire() {
        let (sink, ev) = collector();
        let h = start(sink, Config { enabled: true, port: 0 }).unwrap();
        let port = h.port().unwrap();
        let mk = |n: usize| {
            let mut v = format!(
                "POST /claude/hook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {n}\r\n\r\n"
            )
            .into_bytes();
            let head = br#"{"session_id":"s","cwd":"/x/y","hook_event_name":"Stop","tool_input":""#;
            v.extend_from_slice(head);
            v.resize(v.len() + n - head.len() - 2, b'a');
            v.extend_from_slice(b"\"}");
            v
        };
        let ok = raw(port, &mk(1_500_000));
        assert!(ok.starts_with("HTTP/1.1 200") && ok.ends_with("{}"), "{ok}");
        assert_eq!(ev.lock().unwrap().len(), 1);
        let big = raw(port, &mk(3_000_000));
        assert!(big.starts_with("HTTP/1.1 200") && big.ends_with("{}"), "{big}");
        assert_eq!(ev.lock().unwrap().len(), 1);
    }

    #[test]
    fn disabled_binds_nothing() {
        let (sink, _) = collector();
        let h = start(sink, Config { enabled: false, port: 0 }).unwrap();
        assert_eq!(h.port(), None);
    }

    #[test]
    fn the_extension_reports_its_room() {
        let (sink, events) = collector();
        let room = b"ytm-0123456789abcdef01234567";
        let ext = ("Origin", "chrome-extension://abcdefghijklmnop");
        assert_eq!(handle_request(&req("POST", "/ytm/room", &[H, ext], room), &sink).0, 204);
        assert_eq!(handle_request(&req("POST", "/ytm/room", &[H, ("Origin", "https://music.youtube.com")], room), &sink).0, 403);
        assert_eq!(handle_request(&req("POST", "/notify?title=a", &[H, ext], b""), &sink).0, 403);
        assert_eq!(handle_request(&req("POST", "/ytm/room", &[H, ext], b"ytm-short"), &sink).0, 400);
        assert_eq!(handle_request(&req("GET", "/ytm/room", &[H], b""), &sink).0, 405);
        assert!(matches!(events.lock().unwrap().as_slice(), [Event::YtmRoom(r)] if r == "ytm-0123456789abcdef01234567"));
    }
}
