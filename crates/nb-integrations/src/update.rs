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

/// The latest release of `repo` when newer than `current`. Reads where
/// github.com's "latest release" page redirects to: unlike GitHub's API, it
/// has no hourly limit shared by everyone behind the same internet address.
fn latest(repo: &str, asset: &str, current: &str) -> Result<Option<Release>, Error> {
    let location = crate::weather::https_location("github.com", &format!("/{repo}/releases/latest"))?;
    Ok(parse_location(&location, current, repo, asset))
}

/// `https://github.com/{repo}/releases/tag/v1.2.3` into a release, if newer.
fn parse_location(location: &str, current: &str, repo: &str, asset: &str) -> Option<Release> {
    let tag = location.trim().strip_prefix(&format!("https://github.com/{repo}/releases/tag/"))?;
    if tag.is_empty() || !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
        return None;
    }
    let (latest, now) = (parse_version(tag)?, parse_version(current)?);
    (latest > now).then(|| Release { version: tag.trim_start_matches('v').to_string(), path: format!("/{repo}/releases/download/{tag}/{asset}") })
}

/// Asks GitHub for the latest release. `None` when up to date.
pub fn check_update(current: &str) -> Result<Option<Release>, Error> {
    latest(REPO, ASSET, current)
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
    latest(EXT_REPO, EXT_ASSET, current.unwrap_or("0.0.0"))
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

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.1.10"), Some((0, 1, 10)));
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("v1.0"), None);
        assert_eq!(parse_version("v1.0.0-beta"), None);
    }

    #[test]
    fn latest_from_the_redirect() {
        let loc = "https://github.com/Nael-Basset/naelbar/releases/tag/v0.1.6";
        let r = parse_location(loc, "0.1.5", REPO, ASSET).unwrap();
        assert_eq!(r.version, "0.1.6");
        assert_eq!(r.path, "/Nael-Basset/naelbar/releases/download/v0.1.6/naelbar.exe");
        assert_eq!(parse_location(loc, "0.1.6", REPO, ASSET), None);
        assert_eq!(parse_location("https://evil.example/Nael-Basset/naelbar/releases/tag/v9.0.0", "0.1.5", REPO, ASSET), None);
        assert_eq!(parse_location("https://github.com/Nael-Basset/naelbar/releases/tag/v9.0.0/../x", "0.1.5", REPO, ASSET), None);
        let ext = parse_location("https://github.com/Nael-Basset/ytm-remote/releases/tag/v1.19.0", "0.0.0", EXT_REPO, EXT_ASSET).unwrap();
        assert_eq!(ext.path, "/Nael-Basset/ytm-remote/releases/download/v1.19.0/youtube_music_player_chrome.zip");
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
