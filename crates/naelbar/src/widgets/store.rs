//! Files of the page-two widgets, and what the view keeps of them in memory.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::countdown::{self, Countdown};

pub fn dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("naelbar"))
}

/// Replaced in one step: the island may read the file at any time.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let written = std::fs::File::create(&tmp).and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()));
    let result = written.and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The calculator's last answer, shown in its tile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LastCalc {
    pub expr: String,
    pub result: String,
}

fn calc_path() -> Option<PathBuf> {
    dir().map(|d| d.join("calc.json"))
}

pub fn load_calc() -> LastCalc {
    calc_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_calc(last: &LastCalc) -> io::Result<()> {
    let path = calc_path().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "APPDATA is not set"))?;
    write_atomic(&path, &serde_json::to_vec(last)?)
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub countdowns: Vec<Countdown>,
    pub calc: LastCalc,
}

static CACHE: Mutex<Option<Snapshot>> = Mutex::new(None);

/// Read from disk on first use, then until [`invalidate`] is called.
pub fn snapshot() -> Snapshot {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.get_or_insert_with(|| Snapshot { countdowns: countdown::load(), calc: load_calc() }).clone()
}

/// Forgets what was read, because another process wrote the files.
pub fn invalidate() {
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Removes one countdown from the file. Returns true if the file changed.
pub fn remove_countdown(index: usize) -> bool {
    let mut list = countdown::load();
    if index >= list.len() {
        return false;
    }
    list.remove(index);
    let saved = countdown::save(&list).is_ok();
    invalidate();
    saved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_replaced_whole() {
        let dir = std::env::temp_dir().join(format!("naelbar-store-test-{}", std::process::id()));
        let path = dir.join("nested").join("x.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary file stays behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_calculation_survives_a_round_trip() {
        let last = LastCalc { expr: "12 km en mi".into(), result: "7,456454247 mi".into() };
        let json = serde_json::to_vec(&last).unwrap();
        assert_eq!(serde_json::from_slice::<LastCalc>(&json).unwrap(), last);
        assert_eq!(serde_json::from_slice::<LastCalc>(b"{}").unwrap(), LastCalc::default());
    }
}
