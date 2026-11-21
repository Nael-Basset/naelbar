//! Claude Code usage and activity, as shown by the island.

use std::path::PathBuf;

use nb_core::{ClaudeHook, ClaudeHookKind, ClaudeUsage, UsageWindow};
use serde_json::{Value, json};

pub const FIVE_HOUR_MS: u64 = 5 * 3_600_000;
pub const SEVEN_DAY_MS: u64 = 7 * 24 * 3_600_000;
const MAX_SESSIONS: usize = 8;
const MAX_ENDED_SUBAGENTS: usize = 12;
const FINISHED_SHOWN_MS: u64 = 6000;

#[derive(Debug, Clone, PartialEq)]
pub struct Approval {
    /// `None` for the main thread.
    pub agent: Option<String>,
    pub tool: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Subagent {
    pub id: String,
    pub kind: String,
    pub started_at: u64,
    pub ended_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub project: String,
    pub running: bool,
    pub approvals: Vec<Approval>,
    pub subagents: Vec<Subagent>,
    pub finished_at: Option<u64>,
    pub updated_at: u64,
}

impl Session {
    fn new(id: &str, project: &str, at: u64) -> Self {
        Self { id: id.to_string(), project: project.to_string(), running: false, approvals: Vec::new(), subagents: Vec::new(), finished_at: None, updated_at: at }
    }
}

#[derive(Default)]
pub struct ClaudeState {
    pub usage: Option<ClaudeUsage>,
    pub sessions: Vec<Session>,
    pub link: nb_core::ClaudeLink,
    /// Start of the current 5-hour window, to measure the weekly cost of it.
    pub baseline: Option<Baseline>,
    /// Weekly points per 5-hour point, last measured.
    pub ratio: Option<f32>,
}

/// Percentages and reset times seen when the current 5-hour window began.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Baseline {
    pub five: f32,
    pub week: f32,
    pub five_resets: u64,
    pub week_resets: u64,
}

/// Reset times jitter by milliseconds; a move beyond this is a new window.
const RESET_TOLERANCE_MS: u64 = 60_000;
const RATIO_MIN_POINTS: f32 = 5.0;

/// Keeps `base` and `ratio` up to date with a new usage sample.
pub fn update_calibration(base: &mut Option<Baseline>, ratio: &mut Option<f32>, u: &ClaudeUsage) {
    let (Some(f), Some(w)) = (u.five_hour, u.seven_day) else { return };
    if !f.used_percent.is_finite() || !w.used_percent.is_finite() {
        return;
    }
    let fresh = Baseline { five: f.used_percent, week: w.used_percent, five_resets: f.resets_at_ms, week_resets: w.resets_at_ms };
    match base {
        Some(b)
            if b.five_resets.abs_diff(f.resets_at_ms) <= RESET_TOLERANCE_MS
                && b.week_resets.abs_diff(w.resets_at_ms) <= RESET_TOLERANCE_MS
                && f.used_percent >= b.five =>
        {
            let df = f.used_percent - b.five;
            if df >= RATIO_MIN_POINTS {
                *ratio = Some(((w.used_percent - b.week) / df).clamp(0.0, 2.0));
            }
        }
        _ => *base = Some(fresh),
    }
}

/// Weekly percent when the 5-hour window ends at `target_five`.
pub fn week_forecast(week_now: f32, five_now: f32, target_five: f32, ratio: f32) -> f32 {
    (week_now + (target_five - five_now) * ratio).clamp(0.0, 100.0)
}

impl ClaudeState {
    pub fn load(&mut self) {
        let bytes = usage_path().and_then(|p| std::fs::read(p).ok());
        self.usage = bytes.as_deref().and_then(usage_from_bytes);
        let (base, ratio) = bytes.as_deref().map(calib_from_bytes).unwrap_or((None, None));
        self.baseline = base;
        self.ratio = ratio;
    }

    pub fn on_usage(&mut self, u: ClaudeUsage) {
        self.usage = Some(u);
        update_calibration(&mut self.baseline, &mut self.ratio, &u);
        if let Some(p) = usage_path() {
            save_usage(&p, &u, self.baseline, self.ratio);
        }
    }

    pub fn on_hook(&mut self, h: ClaudeHook) {
        if h.kind == ClaudeHookKind::SessionEnd {
            self.sessions.retain(|s| s.id != h.session);
            return;
        }
        let idx = match self.sessions.iter().position(|s| s.id == h.session) {
            Some(i) => i,
            None => {
                if self.sessions.len() >= MAX_SESSIONS {
                    if let Some(old) = self.sessions.iter().enumerate().min_by_key(|(_, s)| (!s.approvals.is_empty(), s.updated_at)).map(|(i, _)| i) {
                        self.sessions.remove(old);
                    }
                }
                self.sessions.push(Session::new(&h.session, &h.project, h.at_ms));
                self.sessions.len() - 1
            }
        };
        let s = &mut self.sessions[idx];
        s.updated_at = h.at_ms;
        if !h.project.is_empty() {
            s.project = h.project.clone();
        }
        match h.kind {
            ClaudeHookKind::SessionStart => {
                s.running = false;
                s.finished_at = None;
            }
            ClaudeHookKind::SessionEnd => {}
            ClaudeHookKind::Progress => {
                s.running = true;
                s.finished_at = None;
                s.approvals.retain(|a| a.agent != h.agent);
            }
            ClaudeHookKind::PermissionRequest { tool } => {
                s.approvals.retain(|a| a.agent != h.agent);
                s.approvals.push(Approval { agent: h.agent, tool });
            }
            ClaudeHookKind::Stop => {
                if h.agent.is_none() {
                    s.running = false;
                    s.approvals.clear();
                    s.finished_at = Some(h.at_ms);
                }
            }
            ClaudeHookKind::SubagentStart { agent_type } => {
                let id = h.agent.unwrap_or_default();
                s.subagents.retain(|a| a.id != id);
                s.subagents.push(Subagent { id, kind: agent_type, started_at: h.at_ms, ended_at: None });
            }
            ClaudeHookKind::SubagentStop => {
                let id = h.agent.unwrap_or_default();
                if let Some(a) = s.subagents.iter_mut().find(|a| a.id == id && a.ended_at.is_none()) {
                    a.ended_at = Some(h.at_ms);
                }
                s.approvals.retain(|a| a.agent.as_deref() != Some(id.as_str()));
                let ended = s.subagents.iter().filter(|a| a.ended_at.is_some()).count();
                for _ in MAX_ENDED_SUBAGENTS..ended {
                    // Oldest ended first; the list is in start order.
                    if let Some(i) = s.subagents.iter().position(|a| a.ended_at.is_some()) {
                        s.subagents.remove(i);
                    }
                }
            }
        }
    }

    pub fn dismiss(&mut self, session: &str) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session) {
            s.approvals.clear();
        }
    }

    pub fn has_waiting(&self) -> bool {
        self.sessions.iter().any(|s| !s.approvals.is_empty())
    }

    /// When the "finished" flash ends, if one is showing at `now_ms`.
    pub fn finished_until(&self, now_ms: u64) -> Option<u64> {
        if self.has_waiting() {
            return None;
        }
        self.sessions.iter().filter_map(|s| s.finished_at).map(|t| t + FINISHED_SHOWN_MS).filter(|&t| t > now_ms).max()
    }
}

// ---------------------------------------------------------------- persistence

fn usage_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("naelbar").join("claude-usage.json"))
}

fn window_json(w: &Option<UsageWindow>) -> Value {
    match w {
        Some(w) => json!({ "used": w.used_percent, "resets": w.resets_at_ms }),
        None => Value::Null,
    }
}

fn window_from(v: &Value) -> Option<UsageWindow> {
    let used = v.get("used")?.as_f64()? as f32;
    let resets = v.get("resets")?.as_u64()?;
    used.is_finite().then_some(UsageWindow { used_percent: used, resets_at_ms: resets })
}

fn usage_to_value(u: &ClaudeUsage, base: Option<Baseline>, ratio: Option<f32>) -> Value {
    let base = base.map(|b| json!({ "five": b.five, "week": b.week, "five_resets": b.five_resets, "week_resets": b.week_resets }));
    json!({
        "five_hour": window_json(&u.five_hour),
        "seven_day": window_json(&u.seven_day),
        "received": u.received_at_ms,
        "base": base,
        "ratio": ratio,
    })
}

/// Baseline and ratio from a saved file; absent in older files.
fn calib_from_bytes(b: &[u8]) -> (Option<Baseline>, Option<f32>) {
    let Ok(v) = serde_json::from_slice::<Value>(b) else { return (None, None) };
    let ratio = v.get("ratio").and_then(Value::as_f64).map(|r| r as f32).filter(|r| r.is_finite()).map(|r| r.clamp(0.0, 2.0));
    let base = v.get("base").and_then(|b| {
        let five = b.get("five")?.as_f64()? as f32;
        let week = b.get("week")?.as_f64()? as f32;
        let five_resets = b.get("five_resets")?.as_u64()?;
        let week_resets = b.get("week_resets")?.as_u64()?;
        (five.is_finite() && week.is_finite()).then_some(Baseline { five, week, five_resets, week_resets })
    });
    (base, ratio)
}

fn usage_from_bytes(b: &[u8]) -> Option<ClaudeUsage> {
    let v: Value = serde_json::from_slice(b).ok()?;
    let five_hour = v.get("five_hour").and_then(window_from);
    let seven_day = v.get("seven_day").and_then(window_from);
    let received_at_ms = v.get("received")?.as_u64()?;
    Some(ClaudeUsage { five_hour, seven_day, received_at_ms })
}

fn save_usage(path: &std::path::Path, u: &ClaudeUsage, base: Option<Baseline>, ratio: Option<f32>) {
    let Ok(bytes) = serde_json::to_vec(&usage_to_value(u, base, ratio)) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

// ----------------------------------------------------------------- pace maths

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pace {
    /// Share of the window already elapsed, as a percent.
    pub expected_percent: f32,
    /// Percent used at the end of the window at the current rate.
    pub projected_percent: Option<f32>,
    /// Unix ms at which 100 % is reached, when that is before the reset.
    pub limit_at_ms: Option<u64>,
    /// Unix ms at which the steady pace catches up with the usage, when the
    /// usage is above pace and that instant is in the future and before the reset.
    pub resume_at_ms: Option<u64>,
}

/// `None` once `resets_at_ms` has passed (the window is reset and unknown).
pub fn pace(used_percent: f32, resets_at_ms: u64, now_ms: u64, length_ms: u64) -> Option<Pace> {
    if resets_at_ms <= now_ms {
        return None;
    }
    let left = resets_at_ms - now_ms;
    if left > length_ms {
        return None;
    }
    let elapsed = length_ms - left;
    let frac = elapsed as f64 / length_ms as f64;
    let expected_percent = (frac * 100.0) as f32;
    let used = used_percent as f64;
    let resume_at_ms = (used.is_finite() && used > expected_percent as f64).then(|| {
        let start = resets_at_ms - length_ms;
        start + (used / 100.0 * length_ms as f64) as u64
    });
    let resume_at_ms = resume_at_ms.filter(|&t| t > now_ms && t < resets_at_ms);
    if frac < 0.03 || used <= 0.0 || !used.is_finite() {
        return Some(Pace { expected_percent, projected_percent: None, limit_at_ms: None, resume_at_ms });
    }
    let projected = used / frac;
    let limit_at_ms = (projected > 100.0).then(|| {
        let start = resets_at_ms - length_ms;
        start + (100.0 / (used / elapsed as f64)) as u64
    });
    Some(Pace { expected_percent, projected_percent: Some(projected as f32), limit_at_ms, resume_at_ms })
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = 3_600_000;

    fn hook(session: &str, agent: Option<&str>, kind: ClaudeHookKind, at: u64) -> ClaudeHook {
        ClaudeHook { session: session.into(), project: "proj".into(), agent: agent.map(Into::into), kind, at_ms: at }
    }

    fn perm(session: &str, agent: Option<&str>, tool: &str, at: u64) -> ClaudeHook {
        hook(session, agent, ClaudeHookKind::PermissionRequest { tool: tool.into() }, at)
    }

    #[test]
    fn approval_stays_until_progress_on_same_agent() {
        let mut c = ClaudeState::default();
        c.on_hook(perm("a", None, "Bash", 1));
        assert!(c.has_waiting());
        c.on_hook(hook("a", Some("sub"), ClaudeHookKind::Progress, 2));
        assert!(c.has_waiting(), "subagent progress must not clear the main approval");
        c.on_hook(hook("a", None, ClaudeHookKind::Progress, 3));
        assert!(!c.has_waiting());
    }

    #[test]
    fn subagent_approval_cleared_by_its_own_progress_and_stop() {
        let mut c = ClaudeState::default();
        c.on_hook(perm("a", Some("s1"), "Edit", 1));
        c.on_hook(perm("a", None, "Bash", 2));
        c.on_hook(hook("a", Some("s1"), ClaudeHookKind::Progress, 3));
        assert_eq!(c.sessions[0].approvals.len(), 1);
        c.on_hook(perm("a", Some("s1"), "Edit", 4));
        c.on_hook(hook("a", Some("s1"), ClaudeHookKind::SubagentStop, 5));
        assert_eq!(c.sessions[0].approvals.len(), 1);
        assert_eq!(c.sessions[0].approvals[0].agent, None);
    }

    #[test]
    fn stop_clears_approvals_and_records_finish() {
        let mut c = ClaudeState::default();
        c.on_hook(perm("a", None, "Bash", 1));
        c.on_hook(hook("a", Some("sub"), ClaudeHookKind::Stop, 2));
        assert!(c.has_waiting(), "a subagent Stop is not a main Stop");
        assert_eq!(c.sessions[0].finished_at, None);
        c.on_hook(hook("a", None, ClaudeHookKind::Stop, 1000));
        assert!(!c.has_waiting());
        assert_eq!(c.sessions[0].finished_at, Some(1000));
        assert_eq!(c.finished_until(2000), Some(7000));
        assert_eq!(c.finished_until(7000), None);
    }

    #[test]
    fn finished_hidden_while_waiting() {
        let mut c = ClaudeState::default();
        c.on_hook(hook("a", None, ClaudeHookKind::Stop, 1000));
        c.on_hook(perm("b", None, "Bash", 1500));
        assert_eq!(c.finished_until(2000), None);
    }

    #[test]
    fn session_end_and_dismiss() {
        let mut c = ClaudeState::default();
        c.on_hook(perm("a", None, "Bash", 1));
        c.on_hook(perm("b", None, "Bash", 2));
        c.dismiss("a");
        assert!(c.has_waiting());
        assert!(c.sessions[0].approvals.is_empty());
        c.on_hook(hook("b", None, ClaudeHookKind::SessionEnd, 3));
        assert_eq!(c.sessions.len(), 1);
        assert!(!c.has_waiting());
    }

    #[test]
    fn caps() {
        let mut c = ClaudeState::default();
        for i in 0..12u64 {
            c.on_hook(hook(&format!("s{i}"), None, ClaudeHookKind::Progress, i));
        }
        assert_eq!(c.sessions.len(), MAX_SESSIONS);
        assert!(!c.sessions.iter().any(|s| s.id == "s0"));
        assert!(c.sessions.iter().any(|s| s.id == "s11"));

        let mut c = ClaudeState::default();
        for i in 0..15u64 {
            let id = format!("g{i}");
            c.on_hook(hook("a", Some(&id), ClaudeHookKind::SubagentStart { agent_type: "x".into() }, i * 10));
            c.on_hook(hook("a", Some(&id), ClaudeHookKind::SubagentStop, i * 10 + 5));
        }
        c.on_hook(hook("a", Some("live"), ClaudeHookKind::SubagentStart { agent_type: "x".into() }, 999));
        let subs = &c.sessions[0].subagents;
        assert_eq!(subs.iter().filter(|a| a.ended_at.is_some()).count(), MAX_ENDED_SUBAGENTS);
        assert!(subs.iter().any(|a| a.id == "live"));
        assert!(!subs.iter().any(|a| a.id == "g0"));
        assert!(subs.iter().any(|a| a.id == "g14"));
    }

    #[test]
    fn usage_roundtrip_and_bad_file() {
        let u = ClaudeUsage {
            five_hour: Some(UsageWindow { used_percent: 42.5, resets_at_ms: 123 }),
            seven_day: None,
            received_at_ms: 99,
        };
        let base = Baseline { five: 10.0, week: 4.0, five_resets: 123, week_resets: 456 };
        let bytes = serde_json::to_vec(&usage_to_value(&u, Some(base), Some(0.25))).unwrap();
        assert_eq!(usage_from_bytes(&bytes), Some(u));
        assert_eq!(calib_from_bytes(&bytes), (Some(base), Some(0.25)));
        assert_eq!(usage_from_bytes(b"not json"), None);
        assert_eq!(usage_from_bytes(b"{\"five_hour\": 3}"), None);
    }

    #[test]
    fn old_file_still_loads() {
        let old = br#"{"five_hour":{"used":42.5,"resets":123},"seven_day":null,"received":99}"#;
        let u = usage_from_bytes(old).unwrap();
        assert_eq!(u.five_hour.unwrap().used_percent, 42.5);
        assert_eq!(calib_from_bytes(old), (None, None));
    }

    fn sample(five: f32, week: f32, five_resets: u64, week_resets: u64) -> ClaudeUsage {
        ClaudeUsage {
            five_hour: Some(UsageWindow { used_percent: five, resets_at_ms: five_resets }),
            seven_day: Some(UsageWindow { used_percent: week, resets_at_ms: week_resets }),
            received_at_ms: 0,
        }
    }

    #[test]
    fn ratio_needs_five_points() {
        let (mut b, mut r) = (None, None);
        update_calibration(&mut b, &mut r, &sample(10.0, 20.0, 1000 * H, 9000 * H));
        update_calibration(&mut b, &mut r, &sample(14.0, 21.0, 1000 * H, 9000 * H));
        assert_eq!(r, None);
        update_calibration(&mut b, &mut r, &sample(20.0, 22.0, 1000 * H, 9000 * H));
        assert!((r.unwrap() - 0.2).abs() < 1e-6);
        // Beyond the range it is clamped.
        update_calibration(&mut b, &mut r, &sample(20.0, 60.0, 1000 * H, 9000 * H));
        assert_eq!(r, Some(2.0));
    }

    #[test]
    fn ratio_window_change_weekly_reset_and_jitter() {
        let (mut b, mut r) = (None, None);
        update_calibration(&mut b, &mut r, &sample(10.0, 20.0, 1000 * H, 9000 * H));
        update_calibration(&mut b, &mut r, &sample(20.0, 22.0, 1000 * H, 9000 * H));
        assert!((r.unwrap() - 0.2).abs() < 1e-6);
        // Jitter under 60 s: same window, baseline kept.
        update_calibration(&mut b, &mut r, &sample(30.0, 24.0, 1000 * H + 900, 9000 * H - 50));
        assert_eq!(b.unwrap().five, 10.0);
        assert!((r.unwrap() - 0.2).abs() < 1e-6);
        // New 5 h window: baseline restarts, ratio kept.
        update_calibration(&mut b, &mut r, &sample(3.0, 25.0, 1005 * H, 9000 * H));
        assert_eq!(b.unwrap(), Baseline { five: 3.0, week: 25.0, five_resets: 1005 * H, week_resets: 9000 * H });
        assert!((r.unwrap() - 0.2).abs() < 1e-6);
        // Weekly reset inside the same 5 h window restarts it too.
        update_calibration(&mut b, &mut r, &sample(9.0, 1.0, 1005 * H, 9168 * H));
        assert_eq!(b.unwrap().week, 1.0);
        assert!((r.unwrap() - 0.2).abs() < 1e-6);
    }

    #[test]
    fn forecast_is_clamped() {
        assert!((week_forecast(14.0, 10.0, 40.0, 0.2) - 20.0).abs() < 1e-4);
        assert_eq!(week_forecast(95.0, 10.0, 100.0, 1.0), 100.0);
        assert_eq!(week_forecast(2.0, 50.0, 10.0, 1.0), 0.0);
    }

    #[test]
    fn resume_instant() {
        let start = 100 * H;
        let now = start + H;
        // 40 % used at 20 % elapsed: pace catches up at 40 % of the window.
        let over = pace(40.0, start + 5 * H, now, FIVE_HOUR_MS).unwrap();
        assert_eq!(over.resume_at_ms, Some(start + 2 * H));
        // Under pace, and exactly on pace: nothing to wait for.
        assert_eq!(pace(10.0, start + 5 * H, now, FIVE_HOUR_MS).unwrap().resume_at_ms, None);
        assert_eq!(pace(20.0, start + 5 * H, now, FIVE_HOUR_MS).unwrap().resume_at_ms, None);
        // Used over 100 %: the instant would fall after the reset.
        assert_eq!(pace(120.0, start + 5 * H, now, FIVE_HOUR_MS).unwrap().resume_at_ms, None);
        // Past instant (clock already beyond it) is dropped.
        let late = pace(40.0, start + 5 * H, start + 2 * H, FIVE_HOUR_MS).unwrap();
        assert_eq!(late.resume_at_ms, None);
    }

    #[test]
    fn pace_ahead_of_limit() {
        // Halfway through the 5 h window, 25 % used: projected 50 %.
        let now = 10 * H;
        let p = pace(25.0, now + 5 * H / 2, now, FIVE_HOUR_MS).unwrap();
        assert!((p.expected_percent - 50.0).abs() < 0.01);
        assert!((p.projected_percent.unwrap() - 50.0).abs() < 0.01);
        assert_eq!(p.limit_at_ms, None);
    }

    #[test]
    fn pace_projection_over_100() {
        // 1 h elapsed of 5 h, 40 % used: limit at 2.5 h after start.
        let start = 100 * H;
        let now = start + H;
        let p = pace(40.0, start + 5 * H, now, FIVE_HOUR_MS).unwrap();
        assert!((p.projected_percent.unwrap() - 200.0).abs() < 0.01);
        let at = p.limit_at_ms.unwrap();
        assert!(at.abs_diff(start + 5 * H / 2) < 5);
    }

    #[test]
    fn pace_early_zero_and_reset() {
        let now = 50 * H;
        // 3 % of 5 h is 9 min; 5 min elapsed.
        let early = pace(10.0, now + FIVE_HOUR_MS - 5 * 60_000, now, FIVE_HOUR_MS).unwrap();
        assert_eq!(early.projected_percent, None);
        assert!(early.expected_percent < 3.0);
        let zero = pace(0.0, now + H, now, FIVE_HOUR_MS).unwrap();
        assert_eq!(zero.projected_percent, None);
        assert!(pace(10.0, now, now, FIVE_HOUR_MS).is_none());
        assert!(pace(10.0, now - 1, now, FIVE_HOUR_MS).is_none());
        // Reset further away than the window length is nonsense.
        assert!(pace(10.0, now + FIVE_HOUR_MS + 1, now, FIVE_HOUR_MS).is_none());
        let week = pace(50.0, now + SEVEN_DAY_MS / 2, now, SEVEN_DAY_MS).unwrap();
        assert!((week.projected_percent.unwrap() - 100.0).abs() < 0.01);
    }
}
