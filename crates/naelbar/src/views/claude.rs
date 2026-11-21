//! Claude tab: subscription limits with pace, and live sessions.

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};

use super::View;
use crate::claude::{FIVE_HOUR_MS, Pace, SEVEN_DAY_MS, Session, pace, week_forecast};
use crate::state::{AppState, Command};
use crate::ui::theme::*;
use crate::ui::{Align, Icon, Rect, TextStyle, Ui, id, tr};

#[derive(Default)]
pub struct ClaudeView;

const STALE_MS: u64 = 10 * 60_000;
const ROW_H: f32 = 18.0;
const SUB_H: f32 = 14.0;
/// Head 19 + bar 12 + reset line 15 + verdict 14 + week line 14.
const FIVE_ROW_H: f32 = 74.0;

impl View for ClaudeView {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn icon(&self) -> Icon {
        Icon::Spark
    }

    fn build(&mut self, ui: &mut Ui, st: &AppState, area: Rect) {
        let mut rest = area;
        let now = ui.now_ms;
        let off = local_offset_ms();
        if st.claude.link == nb_core::ClaudeLink::NeedsLogin {
            let r = rest.take_top(28.0, 6.0);
            let resp = ui.interact(id("claude.login", 0), r);
            ui.fill(r, R_SMALL, if resp.hovered { TILE_HOVER } else { TILE });
            let label = tr("Se connecter à claude.ai pour lire tes limites", "Sign in to claude.ai to read your limits");
            ui.text(r, label, TextStyle::new(11.5, 500, TEXT).aligned(Align::Center));
            if resp.clicked {
                ui.out.push(Command::ClaudeLogin);
            }
        }
        match &st.claude.usage {
            Some(u) => {
                let stale = now.saturating_sub(u.received_at_ms) >= STALE_MS;
                let age = stale.then(|| age_label(now - u.received_at_ms));
                if stale {
                    let mins = (now - u.received_at_ms) / 60_000;
                    ui.redraw_at(u.received_at_ms + (mins + 1) * 60_000);
                }
                let five = u.five_hour.map(|w| (w.used_percent, w.resets_at_ms));
                let week = u.seven_day.map(|w| (w.used_percent, w.resets_at_ms));
                let extra = week_line(five, week, st.claude.ratio, now);
                limit_row(ui, rest.take_top(FIVE_ROW_H, 4.0), tr("5 heures", "5 hours"), five, FIVE_HOUR_MS, now, off, false, age.as_deref(), extra.as_deref());
                limit_row(ui, rest.take_top(60.0, 6.0), tr("Semaine", "Week"), week, SEVEN_DAY_MS, now, off, true, None, None);
            }
            None => {
                if st.claude.link != nb_core::ClaudeLink::NeedsLogin {
                    let r = rest.take_top(34.0, 6.0);
                    ui.fill(r, R_SMALL, TILE);
                    let t = TextStyle::new(11.5, 400, TEXT_DIM).aligned(Align::Center);
                    let message = if st.claude.link == nb_core::ClaudeLink::Failed {
                        tr("Impossible de lire tes limites pour l'instant.", "Your limits cannot be read right now.")
                    } else {
                        tr("Lecture de tes limites…", "Reading your limits…")
                    };
                    ui.text(r.inset_xy(8.0, 0.0), message, t);
                }
            }
        }
        sessions(ui, &st.claude.sessions, rest, now);
    }
}

#[allow(clippy::too_many_arguments)]
fn limit_row(ui: &mut Ui, r: Rect, label: &str, win: Option<(f32, u64)>, length: u64, now: u64, off: i64, weekly: bool, age: Option<&str>, extra: Option<&str>) {
    let mut rest = r;
    let Some((used, resets)) = win else {
        let mut head = rest.take_top(16.0, 2.0);
        ui.text(head.take_right(40.0, 4.0), "–", TextStyle::new(12.0, 600, TEXT_FAINT).aligned(Align::End));
        ui.text(head, label, TextStyle::new(12.0, 600, TEXT));
        return;
    };
    let p = pace(used, resets, now, length);
    let mut head = rest.take_top(16.0, 3.0);
    let pct = if p.is_some() { format!("{:.0} %", used.clamp(0.0, 999.0)) } else { "–".to_string() };
    ui.text(head.take_right(44.0, 4.0), &pct, TextStyle::new(12.0, 600, TEXT).aligned(Align::End));
    if let Some(a) = age {
        ui.text(head.take_right(70.0, 4.0), a, TextStyle::new(10.0, 400, TEXT_FAINT).aligned(Align::End));
    }
    ui.text(head, label, TextStyle::new(12.0, 600, TEXT));

    // Bar with a tick at the expected position.
    let bar_area = rest.take_top(10.0, 2.0);
    let bar = bar_area.centered(bar_area.w, 5.0);
    ui.fill(bar, 2.5, TILE_HOVER);
    if p.is_some() {
        let f = (used / 100.0).clamp(0.0, 1.0);
        if f > 0.0 {
            ui.fill(Rect::new(bar.x, bar.y, (bar.w * f).max(5.0), bar.h), 2.5, accent());
        }
    }
    let small = TextStyle::new(10.5, 400, TEXT_DIM);
    let Some(p) = p else {
        ui.text(rest.take_top(14.0, 0.0), tr("Fenêtre réinitialisée, en attente de données", "Window reset, waiting for data"), small);
        return;
    };
    let x = bar.x + bar.w * (p.expected_percent / 100.0).clamp(0.0, 1.0);
    ui.fill(Rect::new(x - 1.0, bar_area.y + 0.5, 2.0, 9.0), 1.0, TEXT);

    let left = resets - now;
    let local = (resets as i64 + off).max(0) as u64;
    let when = if weekly || left >= 20 * 3_600_000 { format!("{} {}", weekday(local), hhmm(local)) } else { hhmm(local) };
    let line = format!("{} {}, {}", tr("Réinit.", "Resets"), when, in_label(left));
    ui.text(rest.take_top(14.0, 1.0), &line, small);
    ui.text(rest.take_top(14.0, 0.0), &verdict(used, &p, off, now), TextStyle::new(10.5, 500, TEXT));
    if let Some(e) = extra {
        ui.text(rest.take_top(14.0, 0.0), e, small);
    }

    // Both texts change on the minute.
    ui.redraw_at(now + left % 60_000 + 1);
}

/// Time of day, with the weekday when it is not today.
fn when_label(at: u64, off: i64, now: u64) -> String {
    let local = (at as i64 + off).max(0) as u64;
    if day_number(now as i64 + off) == day_number(local as i64) { hhmm(local) } else { format!("{} {}", weekday(local), hhmm(local)) }
}

/// Weekly percent at the end of the current 5-hour session.
fn week_line(five: Option<(f32, u64)>, week: Option<(f32, u64)>, ratio: Option<f32>, now: u64) -> Option<String> {
    let ((five_now, five_resets), (week_now, _)) = (five?, week?);
    let p = pace(five_now, five_resets, now, FIVE_HOUR_MS)?;
    let Some(ratio) = ratio else {
        return Some(tr("Semaine en fin de session : calcul après quelques % utilisés", "Week at session end: computed after a few % used").to_string());
    };
    let full = week_forecast(week_now, five_now, 100.0, ratio);
    Some(match p.projected_percent {
        Some(e) => {
            let end = week_forecast(week_now, five_now, e.clamp(0.0, 100.0), ratio);
            format!("{} ≈ {end:.0} %{} {full:.0} %{}", tr("Semaine en fin de session :", "Week at session end:"), tr(" (", " ("), tr(" si tu la remplis)", " if you fill it)"))
        }
        None => format!("{} {full:.0} %{}", tr("Semaine en fin de session :", "Week at session end:"), tr(" si tu la remplis", " if you fill it")),
    })
}

fn verdict(used: f32, p: &Pace, off: i64, now: u64) -> String {
    let resume = p.resume_at_ms.map(|t| format!("{} {}", tr("reprends", "resume"), when_label(t, off, now)));
    if let Some(at) = p.limit_at_ms {
        let mut s = format!("{} {}", tr("Trop rapide : limite", "Too fast: limit"), when_label(at, off, now));
        if let Some(r) = resume {
            s = format!("{s}, {r}");
        }
        return s;
    }
    let end = p.projected_percent.map(|e| e.clamp(0.0, 100.0));
    let diff = p.expected_percent - used;
    if let (Some(r), true) = (&resume, diff <= -1.0) {
        return format!("{} {:.0} %, {r}", tr("Au-dessus du rythme de", "Above pace by"), -diff);
    }
    match end {
        None => format!("{} {:.0} %", tr("Début de fenêtre, rythme attendu :", "Early in the window, expected pace:"), p.expected_percent),
        Some(e) if diff.abs() < 1.0 => format!("{} {e:.0} %", tr("Pile au rythme, fin à", "Right on pace, ending at")),
        Some(e) if diff > 0.0 => format!("{} {diff:.0} %{} {e:.0} %", tr("Marge de", "Margin of"), tr(", tu finirais à", ", you would end at")),
        Some(e) => format!("{} {:.0} %{} {e:.0} %", tr("Au-dessus du rythme de", "Above pace by"), -diff, tr(", fin à", ", ending at")),
    }
}

fn sessions(ui: &mut Ui, list: &[Session], area: Rect, now: u64) {
    if list.is_empty() || area.h < ROW_H {
        return;
    }
    let mut order: Vec<&Session> = list.iter().collect();
    // Approvals first, then running, then the rest; most recent first within.
    order.sort_by_key(|s| (s.approvals.is_empty(), !s.running, std::cmp::Reverse(s.updated_at)));

    let heights: Vec<f32> = order.iter().map(|s| ROW_H + SUB_H * running_subs(s).count() as f32).collect();
    let total: f32 = heights.iter().sum();
    let reserve = if total > area.h + 0.5 { SUB_H } else { 0.0 };
    let mut y = area.y;
    let limit = area.bottom() - reserve;
    let mut shown = 0;
    for (s, h) in order.iter().zip(&heights) {
        // Sessions are drawn whole only; a long subagent list is cut at the limit.
        if y + ROW_H > limit + 0.5 {
            break;
        }
        session_row(ui, s, Rect::new(area.x, y, area.w, ROW_H));
        y += ROW_H;
        shown += 1;
        let _ = h;
        for a in running_subs(s) {
            if y + SUB_H > limit + 0.5 {
                break;
            }
            let r = Rect::new(area.x + 10.0, y, area.w - 10.0, SUB_H);
            let secs = now.saturating_sub(a.started_at) / 1000;
            let mut row = r;
            ui.text(row.take_right(44.0, 0.0), &format!("{}:{:02}", secs / 60, secs % 60), TextStyle::new(10.5, 500, TEXT_DIM).aligned(Align::End));
            ui.text(row, &a.kind, TextStyle::new(10.5, 400, TEXT_DIM));
            ui.redraw_next_second(now.saturating_sub(a.started_at));
            y += SUB_H;
        }
    }
    let hidden = order.len() - shown;
    if hidden > 0 {
        let r = Rect::new(area.x, area.bottom() - SUB_H, area.w, SUB_H);
        ui.text(r, &format!("+{hidden}"), TextStyle::new(10.5, 500, TEXT_FAINT));
    }
}

fn running_subs(s: &Session) -> impl Iterator<Item = &crate::claude::Subagent> {
    s.subagents.iter().filter(|a| a.ended_at.is_none())
}

fn session_row(ui: &mut Ui, s: &Session, r: Rect) {
    let mut row = r;
    let (state, color) = if let Some(a) = s.approvals.first() {
        if ui.icon_button(id("claude.dismiss", hash(&s.id)), row.take_right(ROW_H, 2.0), Icon::Close, 11.0, TEXT_DIM).clicked {
            ui.out.push(Command::ClaudeDismiss(s.id.clone()));
        }
        (format!("{} : {}", tr("À valider", "Approve"), a.tool), accent())
    } else if s.running {
        (tr("En cours", "Running").to_string(), TEXT_DIM)
    } else if s.finished_at.is_some() {
        (tr("Terminé", "Done").to_string(), TEXT_DIM)
    } else {
        (tr("Prêt", "Idle").to_string(), TEXT_FAINT)
    };
    let state_w = (state.chars().count() as f32 * 6.0 + 6.0).min(row.w * 0.6);
    ui.text(row.take_right(state_w, 6.0), &state, TextStyle::new(11.0, 500, color).aligned(Align::End));
    ui.push_clip(row, 0.0);
    ui.text(row, &s.project, TextStyle::new(11.5, 500, TEXT));
    ui.pop_clip();
}

fn hash(s: &str) -> usize {
    s.bytes().fold(5381usize, |h, b| h.wrapping_mul(33) ^ b as usize) & 0xFFFF
}

// ----------------------------------------------------------------- formatting

fn age_label(ms: u64) -> String {
    let m = ms / 60_000;
    let t = if m >= 1440 {
        format!("{} {}", m / 1440, tr("j", "d"))
    } else if m >= 60 {
        format!("{} h", m / 60)
    } else {
        format!("{m} min")
    };
    if crate::ui::is_english() { format!("{t} ago") } else { format!("il y a {t}") }
}

fn in_label(ms: u64) -> String {
    let mins = ms / 60_000;
    let (d, h, m) = (mins / 1440, mins / 60 % 24, mins % 60);
    let lead = tr("dans", "in");
    if d > 0 {
        format!("{lead} {d} {} {h} h", tr("j", "d"))
    } else if h > 0 {
        format!("{lead} {h} h {m:02}")
    } else {
        format!("{lead} {m} min")
    }
}

fn day_number(local_ms: i64) -> i64 {
    local_ms.div_euclid(86_400_000)
}

fn weekday(local_ms: u64) -> &'static str {
    const FR: [&str; 7] = ["dim.", "lun.", "mar.", "mer.", "jeu.", "ven.", "sam."];
    const EN: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let d = (day_number(local_ms as i64) + 4).rem_euclid(7) as usize;
    if crate::ui::is_english() { EN[d] } else { FR[d] }
}

fn hhmm(local_ms: u64) -> String {
    let m = local_ms / 60_000 % 1440;
    format!("{:02}:{:02}", m / 60, m % 60)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn st_secs(t: &SYSTEMTIME) -> i64 {
    days_from_civil(t.wYear as i64, t.wMonth as i64, t.wDay as i64) * 86_400 + t.wHour as i64 * 3600 + t.wMinute as i64 * 60 + t.wSecond as i64
}

/// Local minus UTC, in ms, rounded to the minute. The current offset is used
/// for all instants, so a daylight-saving change inside a window is ignored.
fn local_offset_ms() -> i64 {
    let (l, u) = unsafe { (GetLocalTime(), GetSystemTime()) };
    let diff = st_secs(&l) - st_secs(&u);
    ((diff + 30).div_euclid(60)) * 60_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        // 2024-01-01 was a Monday.
        let ms = days_from_civil(2024, 1, 1) as u64 * 86_400_000;
        assert_eq!((day_number(ms as i64) + 4).rem_euclid(7), 1);
        assert_eq!(hhmm(ms + 14 * 3_600_000 + 20 * 60_000), "14:20");
    }

    #[test]
    fn labels() {
        assert_eq!(in_label((2 * 60 + 10) * 60_000), "dans 2 h 10");
        assert_eq!(in_label(45 * 60_000), "dans 45 min");
        assert_eq!(in_label((3 * 1440 + 4 * 60) * 60_000), "dans 3 j 4 h");
        assert_eq!(age_label(3 * 3_600_000), "il y a 3 h");
    }
}
