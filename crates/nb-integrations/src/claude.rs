//! Claude Code data: status line usage and hook inputs, reduced to core events.
//!
//! Only whitelisted fields are read. Tool inputs, prompts, assistant messages
//! and transcript paths are never copied or logged.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use nb_core::{ClaudeHook, ClaudeHookKind, ClaudeUsage, UsageWindow};
use serde_json::Value;

use crate::http;

const MAX_SESSION: usize = 64;
const MAX_AGENT: usize = 32;
const MAX_PROJECT: usize = 80;
const MAX_TOOL: usize = 64;

fn window(root: &Value, key: &str) -> Option<UsageWindow> {
    let w = root.get("rate_limits")?.get(key)?;
    let pct = w.get("used_percentage")?.as_f64()?;
    let reset = w.get("resets_at")?.as_f64()?;
    if !pct.is_finite() || !reset.is_finite() || reset < 0.0 {
        return None;
    }
    Some(UsageWindow {
        used_percent: pct.clamp(0.0, 100.0) as f32,
        resets_at_ms: (reset * 1000.0).min(u64::MAX as f64 / 2.0) as u64,
    })
}

/// Parses the status line JSON. `None` when no window is present.
pub(crate) fn parse_usage(body: &[u8], now_ms: u64) -> Option<ClaudeUsage> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let five_hour = window(&v, "five_hour");
    let seven_day = window(&v, "seven_day");
    if five_hour.is_none() && seven_day.is_none() {
        return None;
    }
    Some(ClaudeUsage { five_hour, seven_day, received_at_ms: now_ms })
}

/// Last folder of a Windows or POSIX path.
pub(crate) fn project_name(cwd: &str) -> String {
    cwd.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or("").to_string()
}

/// Parses a Claude Code hook input. `None` for events we do not show.
pub(crate) fn parse_hook(body: &[u8], now_ms: u64) -> Option<ClaudeHook> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
    let kind = match s("hook_event_name") {
        "SessionStart" => ClaudeHookKind::SessionStart,
        "SessionEnd" => ClaudeHookKind::SessionEnd,
        "PermissionRequest" => {
            ClaudeHookKind::PermissionRequest { tool: http::sanitize(s("tool_name"), MAX_TOOL) }
        }
        "UserPromptSubmit" | "PostToolUse" | "PostToolUseFailure" => ClaudeHookKind::Progress,
        "Stop" => ClaudeHookKind::Stop,
        "SubagentStart" => {
            ClaudeHookKind::SubagentStart { agent_type: http::sanitize(s("agent_type"), MAX_AGENT) }
        }
        "SubagentStop" => ClaudeHookKind::SubagentStop,
        _ => return None,
    };
    let session = http::sanitize(s("session_id"), MAX_SESSION);
    if session.is_empty() {
        return None;
    }
    let agent = http::sanitize(s("agent_id"), MAX_AGENT);
    Some(ClaudeHook {
        session,
        project: http::sanitize(&project_name(s("cwd")), MAX_PROJECT),
        agent: if agent.is_empty() { None } else { Some(agent) },
        kind,
        at_ms: now_ms,
    })
}

fn pct_text(w: &UsageWindow) -> String {
    format!("{} %", w.used_percent.round() as u32)
}

/// One line for the status line, for example `5 h 34 % · sem. 12 %`.
pub(crate) fn usage_text(u: &ClaudeUsage) -> String {
    let mut parts = Vec::new();
    if let Some(w) = &u.five_hour {
        parts.push(format!("5 h {}", pct_text(w)));
    }
    if let Some(w) = &u.seven_day {
        parts.push(format!("sem. {}", pct_text(w)));
    }
    parts.join(" \u{b7} ")
}

/// Status line command body: forwards Claude Code's JSON to the running app
/// (200 ms connect and write timeouts, every failure ignored) and returns the
/// text to display, or an empty string when there are no windows.
pub fn statusline_forward(input: &str, port: u16) -> String {
    let text = parse_usage(input.as_bytes(), 0).map(|u| usage_text(&u)).unwrap_or_default();
    if text.is_empty() {
        return text;
    }
    let t = Duration::from_millis(200);
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    if let Ok(mut s) = TcpStream::connect_timeout(&addr, t) {
        let _ = s.set_write_timeout(Some(t));
        let _ = s.set_read_timeout(Some(t));
        let req = format!(
            "POST /claude/usage HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{input}",
            input.len()
        );
        if s.write_all(req.as_bytes()).is_ok() {
            // Let the app finish reading before we close; bounded by the timeout.
            let mut sink = [0u8; 64];
            let _ = s.read(&mut sink);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(s: &str) -> Option<ClaudeUsage> {
        parse_usage(s.as_bytes(), 5)
    }

    #[test]
    fn usage_full_partial_and_empty() {
        let u = usage(r#"{"rate_limits":{"five_hour":{"used_percentage":34.4,"resets_at":1700000000},"seven_day":{"used_percentage":12,"resets_at":1700500000.5}}}"#).unwrap();
        let f = u.five_hour.unwrap();
        assert_eq!((f.used_percent, f.resets_at_ms), (34.4, 1_700_000_000_000));
        assert_eq!(u.seven_day.unwrap().resets_at_ms, 1_700_500_000_500);
        assert_eq!(u.received_at_ms, 5);
        let u = usage(r#"{"rate_limits":{"seven_day":{"used_percentage":1,"resets_at":2}}}"#).unwrap();
        assert!(u.five_hour.is_none() && u.seven_day.is_some());
        for bad in ["{}", r#"{"rate_limits":{}}"#, "nope", "[]", r#"{"rate_limits":{"five_hour":{}}}"#] {
            assert!(usage(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn usage_bad_values() {
        let u = usage(r#"{"rate_limits":{"five_hour":{"used_percentage":250,"resets_at":1},"seven_day":{"used_percentage":-4,"resets_at":1}}}"#).unwrap();
        assert_eq!(u.five_hour.unwrap().used_percent, 100.0);
        assert_eq!(u.seven_day.unwrap().used_percent, 0.0);
        assert!(usage(r#"{"rate_limits":{"five_hour":{"used_percentage":"9","resets_at":1}}}"#).is_none());
        assert!(usage(r#"{"rate_limits":{"five_hour":{"used_percentage":null,"resets_at":1}}}"#).is_none());
        assert!(usage(r#"{"rate_limits":{"five_hour":{"used_percentage":5,"resets_at":-1}}}"#).is_none());
        assert!(usage(r#"{"rate_limits":{"five_hour":{"used_percentage":5}}}"#).is_none());
        assert!(usage(r#"{"rate_limits":{"five_hour":{"used_percentage":NaN,"resets_at":1}}}"#).is_none());
        // Overflowing literals never produce a non-finite window.
        let u = usage(r#"{"rate_limits":{"five_hour":{"used_percentage":5,"resets_at":1e308}}}"#);
        assert!(u.is_none_or(|u| u.five_hour.is_some()));
    }

    fn hook(event: &str, extra: &str) -> Option<ClaudeHook> {
        let b = format!(r#"{{"session_id":"s1","cwd":"C:\\AI\\naelbar","hook_event_name":"{event}"{extra}}}"#);
        parse_hook(b.as_bytes(), 9)
    }

    #[test]
    fn hook_mapping_table() {
        use ClaudeHookKind::*;
        let cases: Vec<(&str, &str, Option<ClaudeHookKind>)> = vec![
            ("SessionStart", "", Some(SessionStart)),
            ("SessionEnd", "", Some(SessionEnd)),
            ("PermissionRequest", r#","tool_name":"Bash""#, Some(PermissionRequest { tool: "Bash".into() })),
            ("UserPromptSubmit", "", Some(Progress)),
            ("PostToolUse", "", Some(Progress)),
            ("PostToolUseFailure", "", Some(Progress)),
            ("Stop", "", Some(Stop)),
            ("SubagentStart", r#","agent_type":"Explore""#, Some(SubagentStart { agent_type: "Explore".into() })),
            ("SubagentStop", "", Some(SubagentStop)),
            ("Notification", "", None),
            ("PreToolUse", "", None),
            ("", "", None),
        ];
        for (ev, extra, want) in cases {
            assert_eq!(hook(ev, extra).map(|h| h.kind), want, "{ev}");
        }
        let h = hook("Stop", r#","agent_id":"ag1","tool_input":{"secret":1}"#).unwrap();
        assert_eq!(
            (h.session.as_str(), h.project.as_str(), h.agent.as_deref(), h.at_ms),
            ("s1", "naelbar", Some("ag1"), 9)
        );
        assert_eq!(hook("Stop", "").unwrap().agent, None);
        assert!(parse_hook(b"x", 0).is_none());
        assert!(parse_hook(br#"{"hook_event_name":"Stop"}"#, 0).is_none());
    }

    #[test]
    fn project_names() {
        assert_eq!(project_name(r"C:\AI\naelbar"), "naelbar");
        assert_eq!(project_name(r"C:\AI\naelbar\"), "naelbar");
        assert_eq!(project_name("/home/u/proj/"), "proj");
        assert_eq!(project_name("/home/u\\mixed"), "mixed");
        assert_eq!(project_name("single"), "single");
        assert_eq!(project_name(""), "");
        assert_eq!(project_name("/"), "");
    }

    #[test]
    fn statusline_text() {
        let both = r#"{"rate_limits":{"five_hour":{"used_percentage":34.4,"resets_at":1},"seven_day":{"used_percentage":12,"resets_at":1}}}"#;
        // Port 1 refuses quickly; failure is ignored.
        assert_eq!(statusline_forward(both, 1), "5 h 34 % \u{b7} sem. 12 %");
        let week = r#"{"rate_limits":{"seven_day":{"used_percentage":99.6,"resets_at":1}}}"#;
        assert_eq!(statusline_forward(week, 1), "sem. 100 %");
        assert_eq!(statusline_forward("{}", 1), "");
        assert_eq!(statusline_forward("garbage", 1), "");
    }

    #[test]
    fn statusline_forwards_to_endpoint() {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let h = crate::start(
            Arc::new(move |e| s2.lock().unwrap().push(e)),
            crate::Config { enabled: true, port: 0 },
        )
        .unwrap();
        let body = r#"{"rate_limits":{"five_hour":{"used_percentage":7,"resets_at":3}}}"#;
        assert_eq!(statusline_forward(body, h.port().unwrap()), "5 h 7 %");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
