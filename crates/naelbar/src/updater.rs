//! Automatic updates. A minute after startup, then once a day, asks GitHub
//! for the latest release. When it is newer, downloads `naelbar.exe` next to
//! the running one, swaps the two (Windows lets a running exe be renamed),
//! and the island restarts on the new version. The old file is removed at
//! the next start.
//!
//! The thread sleeps between checks: nothing runs while idle. Builds from the
//! source tree (under `target`) never update themselves.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nb_core::{Event, Sink};

use crate::config::Config;

/// Set when the island closed to restart on a new version.
pub static RESTART: AtomicBool = AtomicBool::new(false);
static EXE: OnceLock<PathBuf> = OnceLock::new();
static SINK: OnceLock<Sink> = OnceLock::new();

/// Posted to the island by the settings' "Check now" button.
pub const WM_UPDATE_NOW: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 72;

/// Last outcome, kept in `%LOCALAPPDATA%\naelbar\update.log` (one line).
fn note(msg: &str) {
    if let Some(d) = std::env::var_os("LOCALAPPDATA") {
        let p = PathBuf::from(d).join("naelbar");
        let _ = std::fs::create_dir_all(&p);
        let _ = std::fs::write(p.join("update.log"), msg);
    }
}

const FIRST_CHECK: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(24 * 3600);

fn sibling(exe: &Path, name: &str) -> PathBuf {
    exe.with_file_name(name)
}

/// True for a build run from the source tree.
fn is_dev_build(exe: &Path) -> bool {
    exe.components().any(|c| c.as_os_str().eq_ignore_ascii_case("target"))
}

pub fn start(sink: Sink) {
    let Ok(exe) = std::env::current_exe() else { return };
    let _ = std::fs::remove_file(sibling(&exe, "naelbar.old.exe"));
    let _ = std::fs::remove_file(sibling(&exe, "naelbar.new.exe"));
    let _ = EXE.set(exe.clone());
    let _ = SINK.set(sink.clone());
    if is_dev_build(&exe) {
        return;
    }
    remove_old_later(&exe);
    let _ = std::thread::Builder::new().name("naelbar-update".into()).spawn(move || {
        std::thread::sleep(FIRST_CHECK);
        loop {
            if Config::load().auto_update {
                if run(&exe, &sink) {
                    return;
                }
            }
            std::thread::sleep(EVERY);
        }
    });
}

/// One check, and the update if there is one. True when the island restarts.
fn run(exe: &Path, sink: &Sink) -> bool {
    // The extension first: a naelbar update restarts the island.
    match crate::ytmext::refresh(false) {
        Ok(Some(v)) => sink(Event::Notification(nb_core::Notification {
            source: "naelbar".into(),
            title: format!("Extension YouTube Music {v}"),
            body: "Mise à jour. Actualise-la dans la page des extensions du navigateur.".into(),
            received_at_ms: crate::state::unix_ms(),
            app_id: String::new(),
        })),
        Ok(None) => {}
        Err(e) => note(&format!("extension, échec : {e}")),
    }
    match update(exe) {
        Ok(Some(version)) => {
            note(&format!("installée : {version}"));
            sink(Event::UpdateReady(version));
            true
        }
        Ok(None) => {
            note(&format!("à jour : {}", env!("CARGO_PKG_VERSION")));
            false
        }
        Err(e) => {
            note(&format!("échec : {e}"));
            false
        }
    }
}

/// The settings asked: checks right away, whatever the daily schedule.
pub fn check_now() {
    let (Some(exe), Some(sink)) = (EXE.get().cloned(), SINK.get().cloned()) else { return };
    if is_dev_build(&exe) {
        note("échec : version compilée depuis le code, pas de mise à jour automatique");
        return;
    }
    let _ = std::thread::Builder::new().name("naelbar-update-now".into()).spawn(move || {
        run(&exe, &sink);
    });
}

/// Downloads and puts in place a newer version, if any.
fn update(exe: &Path) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let Some(release) = nb_integrations::check_update(env!("CARGO_PKG_VERSION"))? else { return Ok(None) };
    let bytes = nb_integrations::download_update(&release)?;
    let new = sibling(exe, "naelbar.new.exe");
    let old = sibling(exe, "naelbar.old.exe");
    std::fs::write(&new, &bytes)?;
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(exe, &old) {
        let _ = std::fs::remove_file(&new);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(e.into());
    }
    Ok(Some(release.version))
}

/// The previous version stays locked while one of its windows (settings,
/// calendar) is still open: tries again for a few minutes, then gives up
/// until the next start.
fn remove_old_later(exe: &Path) {
    let old = sibling(exe, "naelbar.old.exe");
    if !old.exists() {
        return;
    }
    let _ = std::thread::Builder::new().name("naelbar-update-cleanup".into()).spawn(move || {
        for _ in 0..20 {
            std::thread::sleep(Duration::from_secs(15));
            if std::fs::remove_file(&old).is_ok() || !old.exists() {
                return;
            }
        }
    });
}

/// Closes a window of the old version and waits for it to go. True when it
/// was open.
fn close_window(class: windows::core::PCWSTR) -> bool {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, IsWindow, PostMessageW, WM_CLOSE};
    unsafe {
        let Ok(h) = FindWindowW(class, windows::core::PCWSTR::null()) else { return false };
        let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        for _ in 0..30 {
            if !IsWindow(Some(h)).as_bool() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        true
    }
}

/// After the island closed for an update, starts the new version. Windows
/// of the old one are closed, so nothing keeps showing the old version, and
/// the settings reopen on their About page.
pub fn relaunch_if_updated() {
    if !RESTART.load(Ordering::SeqCst) {
        return;
    }
    let settings = close_window(windows::core::w!("naelbar.settings"));
    close_window(windows::core::w!("naelbar.agendawin"));
    let Some(exe) = EXE.get() else { return };
    let _ = std::process::Command::new(exe).spawn();
    if settings {
        // Its process lets go of its lock just after its window.
        std::thread::sleep(Duration::from_millis(500));
        let _ = std::process::Command::new(exe).arg("--settings").arg("--section=5").spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_builds_do_not_update() {
        assert!(is_dev_build(Path::new(r"C:\AI\naelbar\target\a-shell\release\naelbar.exe")));
        assert!(!is_dev_build(Path::new(r"C:\Users\x\Downloads\naelbar.exe")));
    }
}
