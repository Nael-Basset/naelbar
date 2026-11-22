//! The YouTube Music extension (ytm-remote), kept up to date by naelbar in
//! `%LOCALAPPDATA%\naelbar\extension-youtube-music`. The browser loads that
//! folder once ("Load unpacked"); after an update, reloading the extension
//! in the browser is all it takes.
//!
//! Installed only when asked from the settings. After that, the daily update
//! check also refreshes it.

use std::path::PathBuf;

pub fn dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("naelbar").join("extension-youtube-music"))
}

/// Version written in the installed manifest, if installed.
pub fn installed_version() -> Option<String> {
    let text = std::fs::read_to_string(dir()?.join("manifest.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v["version"].as_str().map(str::to_string)
}

/// Installs (when `install` is set) or updates the extension. Returns the new
/// version when something changed.
pub fn refresh(install: bool) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let current = installed_version();
    if current.is_none() && !install {
        return Ok(None);
    }
    let Some(release) = nb_integrations::check_extension(current.as_deref())? else { return Ok(None) };
    let zip = nb_integrations::download_extension(&release)?;
    let target = dir().ok_or("no LOCALAPPDATA")?;
    let parent = target.parent().ok_or("no parent")?.to_path_buf();
    std::fs::create_dir_all(&parent)?;
    let zip_path = parent.join("extension-youtube-music.zip");
    let fresh = parent.join("extension-youtube-music.new");
    let old = parent.join("extension-youtube-music.old");
    let _ = std::fs::remove_dir_all(&fresh);
    let _ = std::fs::remove_dir_all(&old);
    std::fs::write(&zip_path, &zip)?;
    std::fs::create_dir_all(&fresh)?;
    // Windows' own tar reads zip archives.
    let ok = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("tar").arg("-xf").arg(&zip_path).arg("-C").arg(&fresh).creation_flags(CREATE_NO_WINDOW).status()?.success()
    };
    let _ = std::fs::remove_file(&zip_path);
    if !ok || !fresh.join("manifest.json").is_file() {
        let _ = std::fs::remove_dir_all(&fresh);
        return Err("archive de l'extension illisible".into());
    }
    // Same folder path as before, so the browser keeps pointing at it.
    if target.exists() {
        std::fs::rename(&target, &old)?;
    }
    if let Err(e) = std::fs::rename(&fresh, &target) {
        let _ = std::fs::rename(&old, &target);
        return Err(e.into());
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(Some(release.version))
}

/// Shows the folder in Explorer, to pick it in "Load unpacked".
pub fn reveal() {
    if let Some(d) = dir() {
        let _ = std::process::Command::new("explorer.exe").arg(d).spawn();
    }
}

#[cfg(test)]
mod tests {
    /// Downloads and unpacks the real extension into a throwaway folder:
    /// `cargo test -p naelbar -- --ignored ytmext_live`.
    #[test]
    #[ignore]
    fn ytmext_live_install() {
        let tmp = std::env::temp_dir().join(format!("naelbar-ytmext-{}", std::process::id()));
        unsafe { std::env::set_var("LOCALAPPDATA", &tmp) };
        let v = super::refresh(true).unwrap().expect("installed");
        assert_eq!(super::installed_version().as_deref(), Some(v.as_str()));
        assert!(super::dir().unwrap().join("background.js").is_file());
        // Up to date: nothing more to do.
        assert_eq!(super::refresh(false).unwrap(), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
