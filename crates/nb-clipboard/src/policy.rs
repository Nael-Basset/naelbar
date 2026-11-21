//! Decision logic for "must this clipboard content be ignored?".
//!
//! Password managers and similar tools mark sensitive content with the
//! clipboard-history conventions. Anything flagged is never captured, never
//! stored, never emitted. The decision is a pure function so it can be tested.

use std::time::Duration;

pub const FMT_EXCLUDE_MONITOR: &str = "ExcludeClipboardContentFromMonitorProcessing";
pub const FMT_CAN_INCLUDE_HISTORY: &str = "CanIncludeInClipboardHistory";
pub const FMT_VIEWER_IGNORE: &str = "Clipboard Viewer Ignore";

/// State of the `CanIncludeInClipboardHistory` format (a DWORD, 0 = exclude).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanInclude {
    Absent,
    Value(u32),
    /// Present but its payload could not be read. Treated as sensitive.
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SensitivityFlags {
    pub exclude_from_monitor: bool,
    pub can_include_in_history: CanInclude,
    pub viewer_ignore: bool,
}

impl SensitivityFlags {
    /// True when the content must be skipped. Fails closed.
    pub fn is_sensitive(&self) -> bool {
        self.exclude_from_monitor
            || self.viewer_ignore
            || matches!(
                self.can_include_in_history,
                CanInclude::Value(0) | CanInclude::Unreadable
            )
    }
}

/// Delays (ms) applied before each successive `OpenClipboard` attempt.
/// The length of the slice is the hard bound on attempts.
pub const OPEN_BACKOFF_MS: [u64; 6] = [0, 10, 20, 40, 60, 80];

/// Runs `attempt` at most `delays_ms.len()` times, sleeping the listed delay
/// before each one. No unbounded loop: returns `None` once the slice is spent.
pub fn retry_bounded<T>(
    delays_ms: &[u64],
    mut attempt: impl FnMut() -> Option<T>,
    mut sleep: impl FnMut(Duration),
) -> Option<T> {
    for &d in delays_ms {
        if d > 0 {
            sleep(Duration::from_millis(d));
        }
        if let Some(v) = attempt() {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(e: bool, c: CanInclude, v: bool) -> SensitivityFlags {
        SensitivityFlags { exclude_from_monitor: e, can_include_in_history: c, viewer_ignore: v }
    }

    #[test]
    fn plain_content_is_kept() {
        assert!(!flags(false, CanInclude::Absent, false).is_sensitive());
    }

    #[test]
    fn exclude_monitor_skips() {
        assert!(flags(true, CanInclude::Absent, false).is_sensitive());
        assert!(flags(true, CanInclude::Value(1), false).is_sensitive());
    }

    #[test]
    fn viewer_ignore_skips() {
        assert!(flags(false, CanInclude::Absent, true).is_sensitive());
    }

    #[test]
    fn can_include_zero_skips_nonzero_keeps() {
        assert!(flags(false, CanInclude::Value(0), false).is_sensitive());
        assert!(!flags(false, CanInclude::Value(1), false).is_sensitive());
        assert!(!flags(false, CanInclude::Value(7), false).is_sensitive());
    }

    #[test]
    fn unreadable_can_include_fails_closed() {
        assert!(flags(false, CanInclude::Unreadable, false).is_sensitive());
    }

    #[test]
    fn all_combinations_match_rule() {
        for e in [false, true] {
            for v in [false, true] {
                for c in [
                    CanInclude::Absent,
                    CanInclude::Value(0),
                    CanInclude::Value(1),
                    CanInclude::Unreadable,
                ] {
                    let expect =
                        e || v || matches!(c, CanInclude::Value(0) | CanInclude::Unreadable);
                    assert_eq!(flags(e, c, v).is_sensitive(), expect);
                }
            }
        }
    }

    #[test]
    fn retry_is_bounded_and_stops_on_success() {
        let mut calls = 0;
        let mut slept = Vec::new();
        let r: Option<()> = retry_bounded(
            &OPEN_BACKOFF_MS,
            || {
                calls += 1;
                None
            },
            |d| slept.push(d),
        );
        assert!(r.is_none());
        assert_eq!(calls, OPEN_BACKOFF_MS.len());
        assert_eq!(slept.len(), OPEN_BACKOFF_MS.len() - 1);

        let mut calls = 0;
        let r = retry_bounded(
            &OPEN_BACKOFF_MS,
            || {
                calls += 1;
                (calls == 3).then_some(calls)
            },
            |_| {},
        );
        assert_eq!(r, Some(3));
        assert_eq!(calls, 3);
    }

    #[test]
    fn total_wait_is_small() {
        assert!(OPEN_BACKOFF_MS.iter().sum::<u64>() <= 250);
    }
}
