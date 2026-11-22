//! Updates from the project's GitHub releases: the latest release's tag,
//! compared with the running version, and its `naelbar.exe` asset.

use crate::Error;

const REPO: &str = "Nael-Basset/naelbar";
const ASSET: &str = "naelbar.exe";
/// The YouTube Music extension, for Chromium browsers.
const EXT_REPO: &str = "Nael-Basset/ytm-remote";
const EXT_ASSET: &str = "youtube_music_player_chrome.zip";
/// An executable larger than this is not ours.
const MAX_EXE: usize = 64 * 1024 * 1024;

/// A release newer than the running version.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    /// Path of the asset on github.com (which redirects to the file).
    pub path: String,
}

/// `1.2.3` (with or without a leading `v`) as numbers.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u32>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// The latest release when it is newer than `current` and carries the exe.
pub(crate) fn parse_latest(body: &[u8], current: &str) -> Result<Option<Release>, Error> {
    parse_release(body, current, REPO, ASSET)
}

fn parse_release(body: &[u8], current: &str, repo: &str, asset: &str) -> Result<Option<Release>, Error> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| Error::Json(e.to_string()))?;
    let tag = v["tag_name"].as_str().unwrap_or_default();
    let (Some(latest), Some(now)) = (parse_version(tag), parse_version(current)) else { return Ok(None) };
    if latest <= now || v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return Ok(None);
    }
    let prefix = format!("https://github.com/{repo}/releases/download/");
    let path = v["assets"].as_array().into_iter().flatten().find(|a| a["name"].as_str() == Some(asset)).and_then(|a| a["browser_download_url"].as_str()).filter(|u| u.starts_with(&prefix)).and_then(|u| u.strip_prefix("https://github.com"));
    Ok(path.map(|p| Release { version: tag.trim_start_matches('v').to_string(), path: p.to_string() }))
}

/// Asks GitHub for the latest release. `None` when up to date.
pub fn check_update(current: &str) -> Result<Option<Release>, Error> {
    let body = crate::weather::https_get_max("api.github.com", &format!("/repos/{REPO}/releases/latest"), 256 * 1024)?;
    parse_latest(&body, current)
}

/// Downloads the release's executable and checks it looks like one.
pub fn download_update(r: &Release) -> Result<Vec<u8>, Error> {
    let exe = crate::weather::https_get_max("github.com", &r.path, MAX_EXE)?;
    if exe.len() < 512 * 1024 || !exe.starts_with(b"MZ") {
        return Err(Error::Network("not an executable".into()));
    }
    Ok(exe)
}

/// The extension's latest release, when newer than `current` (`None`: not
/// installed yet, any release will do).
pub fn check_extension(current: Option<&str>) -> Result<Option<Release>, Error> {
    let body = crate::weather::https_get_max("api.github.com", &format!("/repos/{EXT_REPO}/releases/latest"), 256 * 1024)?;
    parse_release(&body, current.unwrap_or("0.0.0"), EXT_REPO, EXT_ASSET)
}

/// Downloads the extension's zip and checks it looks like one.
pub fn download_extension(r: &Release) -> Result<Vec<u8>, Error> {
    let zip = crate::weather::https_get_max("github.com", &r.path, 16 * 1024 * 1024)?;
    if zip.len() < 1024 || !zip.starts_with(b"PK") {
        return Err(Error::Network("not a zip".into()));
    }
    Ok(zip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(tag: &str, url: &str) -> Vec<u8> {
        serde_json::json!({"tag_name": tag, "draft": false, "prerelease": false, "assets": [{"name": "naelbar.exe", "browser_download_url": url}]}).to_string().into_bytes()
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.1.10"), Some((0, 1, 10)));
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("v1.0"), None);
        assert_eq!(parse_version("v1.0.0-beta"), None);
    }

    #[test]
    fn only_newer_releases_from_the_repo() {
        let url = "https://github.com/Nael-Basset/naelbar/releases/download/v0.1.2/naelbar.exe";
        let r = parse_latest(&body("v0.1.2", url), "0.1.1").unwrap().unwrap();
        assert_eq!(r.version, "0.1.2");
        assert_eq!(r.path, "/Nael-Basset/naelbar/releases/download/v0.1.2/naelbar.exe");
        assert_eq!(parse_latest(&body("v0.1.1", url), "0.1.1").unwrap(), None);
        assert_eq!(parse_latest(&body("v0.1.0", url), "0.1.1").unwrap(), None);
        assert_eq!(parse_latest(&body("v0.1.10", url), "0.1.9").unwrap().map(|r| r.version), Some("0.1.10".into()));
        // An asset hosted anywhere else is ignored.
        assert_eq!(parse_latest(&body("v0.2.0", "https://evil.example/naelbar.exe"), "0.1.1").unwrap(), None);
        assert!(parse_latest(b"not json", "0.1.1").is_err());
    }

    #[test]
    fn extension_release() {
        let b = serde_json::json!({"tag_name": "v1.19.0", "assets": [{"name": "youtube_music_player_chrome.zip", "browser_download_url": "https://github.com/Nael-Basset/ytm-remote/releases/download/v1.19.0/youtube_music_player_chrome.zip"}]}).to_string();
        let r = parse_release(b.as_bytes(), "0.0.0", EXT_REPO, EXT_ASSET).unwrap().unwrap();
        assert_eq!(r.version, "1.19.0");
        assert_eq!(parse_release(b.as_bytes(), "1.19.0", EXT_REPO, EXT_ASSET).unwrap(), None);
        // The naelbar repo's link does not pass for the extension.
        assert_eq!(parse_release(b.as_bytes(), "0.0.0", REPO, EXT_ASSET).unwrap(), None);
    }

    /// Talks to GitHub: `cargo test -p nb-integrations -- --ignored live`.
    #[test]
    #[ignore]
    fn live_latest_release_downloads() {
        let r = check_update("0.0.1").unwrap().expect("a release");
        let exe = download_update(&r).unwrap();
        assert!(exe.len() > 1024 * 1024);
    }
}
