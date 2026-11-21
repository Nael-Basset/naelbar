//! Minimal, strict HTTP/1.1 request parser with hard limits.

use std::io::{self, Read};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_head: usize,
    pub max_headers: usize,
    pub max_body: usize,
    /// Body limit for `/claude/*` routes (hook inputs carry tool_input).
    pub max_claude_body: usize,
    /// Most bytes drained and discarded from an oversized `/claude/*` body.
    pub max_drain: usize,
    pub total_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_head: 8 * 1024,
            max_headers: 32,
            max_body: 4 * 1024,
            max_claude_body: 2 * 1024 * 1024,
            max_drain: 32 * 1024 * 1024,
            total_timeout: Duration::from_secs(3),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    Malformed,
    HeadTooLarge,
    BodyTooLarge,
    Timeout,
    Unsupported,
    Io,
}

impl ParseError {
    pub fn status(&self) -> (u16, &'static str) {
        match self {
            ParseError::Malformed | ParseError::Io => (400, "Bad Request"),
            ParseError::HeadTooLarge => (431, "Request Header Fields Too Large"),
            ParseError::BodyTooLarge => (413, "Payload Too Large"),
            ParseError::Timeout => (408, "Request Timeout"),
            ParseError::Unsupported => (501, "Not Implemented"),
        }
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// A `/claude/*` body above the limit: drained and dropped, `body` empty.
    pub oversized: bool,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn map_io(e: io::Error) -> ParseError {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => ParseError::Timeout,
        _ => ParseError::Io,
    }
}

/// Reads one request. The reader should have a socket read timeout set; the
/// total deadline additionally bounds slow-drip senders.
pub fn read_request<R: Read>(r: &mut R, limits: &Limits) -> Result<Request, ParseError> {
    let deadline = Instant::now() + limits.total_timeout;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = find_head_end(&buf) {
            break i;
        }
        if buf.len() > limits.max_head {
            return Err(ParseError::HeadTooLarge);
        }
        if Instant::now() > deadline {
            return Err(ParseError::Timeout);
        }
        let n = r.read(&mut chunk).map_err(map_io)?;
        if n == 0 {
            return Err(ParseError::Malformed);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    if head_end > limits.max_head {
        return Err(ParseError::HeadTooLarge);
    }
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| ParseError::Malformed)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(ParseError::Malformed)?;
    let mut parts = request_line.split(' ');
    let method = parts.next().ok_or(ParseError::Malformed)?;
    let target = parts.next().ok_or(ParseError::Malformed)?;
    let version = parts.next().ok_or(ParseError::Malformed)?;
    if parts.next().is_some()
        || method.is_empty()
        || !method.bytes().all(|b| b.is_ascii_uppercase())
    {
        return Err(ParseError::Malformed);
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(ParseError::Malformed);
    }
    if !target.starts_with('/') || target.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(ParseError::Malformed);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let mut headers = Vec::new();
    for line in lines {
        if headers.len() >= limits.max_headers {
            return Err(ParseError::HeadTooLarge);
        }
        let (k, v) = line.split_once(':').ok_or(ParseError::Malformed)?;
        if k.is_empty() || k.bytes().any(|b| b <= b' ' || b == 0x7f) {
            return Err(ParseError::Malformed);
        }
        headers.push((k.to_string(), v.trim().to_string()));
    }

    let mut req = Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body: Vec::new(),
        oversized: false,
    };
    let claude_route = req.path.starts_with("/claude/");
    if req.header("transfer-encoding").is_some() || req.header("expect").is_some() {
        return Err(ParseError::Unsupported);
    }
    let len = match req.header("content-length") {
        None => 0,
        Some(v) => {
            let count = req
                .headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .count();
            if count > 1 || v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ParseError::Malformed);
            }
            v.parse::<usize>().map_err(|_| ParseError::BodyTooLarge)?
        }
    };
    if claude_route && len > limits.max_claude_body {
        // Read and discard so the caller is never left blocked on a write.
        let mut left = len.saturating_sub(buf.len() - head_end - 4).min(limits.max_drain);
        while left > 0 && Instant::now() <= deadline {
            let want = left.min(chunk.len());
            match r.read(&mut chunk[..want]) {
                Ok(0) | Err(_) => break,
                Ok(n) => left -= n,
            }
        }
        req.oversized = true;
        return Ok(req);
    }
    let max = if claude_route { limits.max_claude_body } else { limits.max_body };
    if len > max {
        return Err(ParseError::BodyTooLarge);
    }
    let mut body = buf[head_end + 4..].to_vec();
    body.truncate(len);
    while body.len() < len {
        if Instant::now() > deadline {
            return Err(ParseError::Timeout);
        }
        let want = (len - body.len()).min(chunk.len());
        let n = r.read(&mut chunk[..want]).map_err(map_io)?;
        if n == 0 {
            return Err(ParseError::Malformed);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    req.body = body;
    Ok(req)
}

/// True when the Host header names a loopback authority (any port).
pub fn host_is_loopback(host: &str) -> bool {
    let h = host.trim();
    let name = if let Some(rest) = h.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inner, tail)) if tail.is_empty() || tail.starts_with(':') => inner,
            _ => return false,
        }
    } else {
        match h.rsplit_once(':') {
            Some((n, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => n,
            _ => h,
        }
    };
    let name = name.to_ascii_lowercase();
    name == "127.0.0.1" || name == "localhost" || name == "::1"
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() && hex(b[i + 1]).is_some() && hex(b[i + 2]).is_some() => {
                out.push((hex(b[i + 1]).unwrap() << 4) | hex(b[i + 2]).unwrap());
                i += 3;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

pub fn parse_pairs(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// Removes control characters (newline kept) and truncates to `max` chars.
pub fn sanitize(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &[u8]) -> Result<Request, ParseError> {
        read_request(&mut &s[..], &Limits::default())
    }

    #[test]
    fn parses_get_with_query() {
        let r = parse(b"GET /notify?title=a%20b&body=x+y HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n").unwrap();
        assert_eq!(r.path, "/notify");
        let p = parse_pairs(&r.query);
        assert_eq!(p[0], ("title".into(), "a b".into()));
        assert_eq!(p[1].1, "x y");
    }

    #[test]
    fn parses_post_body() {
        let r = parse(b"POST /agent HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\nabcdEXTRA").unwrap();
        assert_eq!(r.body, b"abcd");
    }

    #[test]
    fn rejects_big_head() {
        let mut s = b"GET / HTTP/1.1\r\nX: ".to_vec();
        s.extend(std::iter::repeat_n(b'a', 20_000));
        s.extend_from_slice(b"\r\n\r\n");
        assert_eq!(parse(&s).unwrap_err(), ParseError::HeadTooLarge);
    }

    #[test]
    fn rejects_unterminated_big_head() {
        let mut s = b"GET / HTTP/1.1\r\nX: ".to_vec();
        s.extend(std::iter::repeat_n(b'a', 20_000));
        assert_eq!(parse(&s).unwrap_err(), ParseError::HeadTooLarge);
    }

    #[test]
    fn rejects_too_many_headers() {
        let mut s = String::from("GET / HTTP/1.1\r\n");
        for i in 0..40 {
            s.push_str(&format!("H{i}: v\r\n"));
        }
        s.push_str("\r\n");
        assert_eq!(parse(s.as_bytes()).unwrap_err(), ParseError::HeadTooLarge);
    }

    #[test]
    fn rejects_big_body() {
        let r = parse(b"POST /agent HTTP/1.1\r\nContent-Length: 999999\r\n\r\n");
        assert_eq!(r.unwrap_err(), ParseError::BodyTooLarge);
    }

    #[test]
    fn rejects_bad_content_length_and_chunked() {
        assert_eq!(parse(b"POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n").unwrap_err(), ParseError::Malformed);
        assert_eq!(
            parse(b"POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx").unwrap_err(),
            ParseError::Malformed
        );
        assert_eq!(
            parse(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap_err(),
            ParseError::Unsupported
        );
    }

    #[test]
    fn rejects_truncated_body_and_garbage() {
        assert_eq!(parse(b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nab").unwrap_err(), ParseError::Malformed);
        assert_eq!(parse(b"garbage\r\n\r\n").unwrap_err(), ParseError::Malformed);
        assert_eq!(parse(b"GET / HTTP/2\r\n\r\n").unwrap_err(), ParseError::Malformed);
    }

    #[test]
    fn host_check() {
        for ok in ["127.0.0.1", "127.0.0.1:47911", "localhost:80", "LOCALHOST", "[::1]:5"] {
            assert!(host_is_loopback(ok), "{ok}");
        }
        for bad in [
            "evil.com",
            "evil.com:47911",
            "127.0.0.1.evil.com",
            "localhost.evil.com",
            "192.168.1.2",
            "",
            "[::1]x",
            "0.0.0.0",
        ] {
            assert!(!host_is_loopback(bad), "{bad}");
        }
    }

    #[test]
    fn sanitize_truncates() {
        assert_eq!(sanitize("a\u{0}b\r\nc", 10), "ab\nc");
        assert_eq!(sanitize(&"é".repeat(500), 120).chars().count(), 120);
    }
}
