//! Windows toast mirror via `UserNotificationListener`.
//!
//! Access normally requires package identity; unpackaged executables are
//! typically refused. Any refusal is reported as [`Error::Unsupported`].

use nb_core::{Event, Notification, Sink};
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::Management::{
    UserNotificationListener, UserNotificationListenerAccessStatus,
};
use windows::UI::Notifications::{
    KnownNotificationBindings, UserNotificationChangedEventArgs, UserNotificationChangedKind,
};

use crate::{Error, now_ms};

/// Dropping unsubscribes from toast changes.
pub struct ToastMirror {
    listener: UserNotificationListener,
    token: i64,
}

impl Drop for ToastMirror {
    fn drop(&mut self) {
        let _ = self.listener.RemoveNotificationChanged(self.token);
    }
}

fn unsupported(what: &str, e: windows::core::Error) -> Error {
    Error::Unsupported(format!("{what}: {e}"))
}

pub fn start_toast_mirror(sink: Sink) -> Result<ToastMirror, Error> {
    let listener = UserNotificationListener::Current().map_err(|e| unsupported("listener unavailable", e))?;
    let status = listener
        .RequestAccessAsync()
        .and_then(|op| op.join())
        .map_err(|e| unsupported("access request failed", e))?;
    if status != UserNotificationListenerAccessStatus::Allowed {
        return Err(Error::Unsupported(format!("notification access not allowed ({})", status.0)));
    }
    let handler = TypedEventHandler::<UserNotificationListener, UserNotificationChangedEventArgs>::new(
        move |sender, args| {
            if let (Some(l), Some(a)) = (sender.as_ref(), args.as_ref()) {
                if a.ChangeKind()? == UserNotificationChangedKind::Added {
                    if let Some(n) = read_notification(l, a.UserNotificationId()?) {
                        sink(Event::Notification(n));
                    }
                }
            }
            Ok(())
        },
    );
    let token = listener
        .NotificationChanged(&handler)
        .map_err(|e| unsupported("subscription refused", e))?;
    Ok(ToastMirror { listener, token })
}

fn read_notification(l: &UserNotificationListener, id: u32) -> Option<Notification> {
    let un = l.GetNotification(id).ok()?;
    let source = un
        .AppInfo()
        .and_then(|a| a.DisplayInfo())
        .and_then(|d| d.DisplayName())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let binding = un.Notification().ok()?.Visual().ok()?.GetBinding(&KnownNotificationBindings::ToastGeneric().ok()?).ok()?;
    let texts = binding.GetTextElements().ok()?;
    let mut lines: Vec<String> = Vec::new();
    for t in texts {
        if let Ok(s) = t.Text() {
            lines.push(s.to_string());
        }
    }
    let title = if lines.is_empty() { String::new() } else { lines.remove(0) };
    Some(Notification { source, title, body: lines.join("\n"), received_at_ms: now_ms(), app_id: String::new() })
}
