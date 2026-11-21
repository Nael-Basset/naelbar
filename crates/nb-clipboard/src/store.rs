//! History container: ordering, dedup, caps and persistence. No Win32 here.

use nb_core::{ClipContent, ClipItem};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Config {
    /// Maximum number of items kept. Pinned items are never evicted.
    pub max_items: usize,
    /// Maximum total size of stored PNG data, in bytes.
    pub max_image_bytes: u64,
    /// Persistence directory. `None` means `%APPDATA%\naelbar\clipboard`.
    pub dir: Option<PathBuf>,
    /// When false nothing is read from or written to disk.
    pub persist: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { max_items: 100, max_image_bytes: 64 * 1024 * 1024, dir: None, persist: true }
    }
}

impl Config {
    pub(crate) fn resolve_dir(&self) -> Option<PathBuf> {
        if !self.persist {
            return None;
        }
        match &self.dir {
            Some(d) => Some(d.clone()),
            None => std::env::var_os("APPDATA")
                .map(|a| PathBuf::from(a).join("naelbar").join("clipboard")),
        }
    }
}

impl Config {
    /// The history folder this config points at, whether or not it is used.
    fn history_dir(&self) -> Option<PathBuf> {
        match &self.dir {
            Some(d) => Some(d.clone()),
            None => std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("naelbar").join("clipboard")),
        }
    }
}

/// Deletes what a history folder holds (index, images, temp files, including
/// the plain-text ones older versions wrote), then the folder if it is empty.
fn purge_history(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if name == "index.json" || name.ends_with(".png") || name.ends_with(".tmp") || name.ends_with(".json") {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let _ = std::fs::remove_dir(dir);
}

pub(crate) const MAX_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Index {
    version: u32,
    next_id: u64,
    items: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    id: u64,
    captured_at_ms: u64,
    pinned: bool,
    #[serde(flatten)]
    kind: Kind,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Kind {
    Text { text: String },
    Image { file: String, width: u32, height: u32 },
    Files { paths: Vec<String> },
}

fn image_file(id: u64) -> String {
    format!("{id}.png")
}

fn io_err() -> std::io::Error {
    std::io::Error::other("DPAPI encryption failed")
}

/// Encrypts for the current user and replaces the file atomically. Nothing
/// readable is ever written.
fn write_encrypted(path: &Path, plain: &[u8]) -> std::io::Result<()> {
    let enc = crate::crypt::protect(plain).ok_or_else(io_err)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, enc)?;
    std::fs::rename(&tmp, path)
}

const PNG_MAGIC: &[u8] = &[0x89, b'P', b'N', b'G'];

/// Everything needed to write the store to disk, detached from the store so
/// the (possibly slow) I/O happens without holding its lock.
pub(crate) struct SaveJob {
    dir: PathBuf,
    index_json: String,
    images: Vec<(String, Arc<[u8]>)>,
    keep: HashSet<String>,
    pub rev: u64,
}

impl SaveJob {
    pub fn write(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        for (name, bytes) in &self.images {
            let p = self.dir.join(name);
            if !p.exists() {
                write_encrypted(&p, bytes)?;
            }
        }
        write_encrypted(&self.dir.join("index.json"), self.index_json.as_bytes())?;
        // Remove orphaned images.
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.ends_with(".png") && !self.keep.contains(&n) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        Ok(())
    }
}

pub(crate) struct Store {
    items: Vec<ClipItem>, // newest first
    next_id: u64,
    max_items: usize,
    max_image_bytes: u64,
    dir: Option<PathBuf>,
    rev: u64,
    saved_rev: u64,
}

fn content_eq(a: &ClipContent, b: &ClipContent) -> bool {
    match (a, b) {
        (ClipContent::Text(x), ClipContent::Text(y)) => x == y,
        (
            ClipContent::Image { png: p1, width: w1, height: h1 },
            ClipContent::Image { png: p2, width: w2, height: h2 },
        ) => w1 == w2 && h1 == h2 && p1[..] == p2[..],
        (ClipContent::Files(x), ClipContent::Files(y)) => x == y,
        _ => false,
    }
}

fn image_size(c: &ClipContent) -> u64 {
    match c {
        ClipContent::Image { png, .. } => png.len() as u64,
        _ => 0,
    }
}

impl Store {
    pub fn new(config: &Config) -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
            max_items: config.max_items.max(1),
            max_image_bytes: config.max_image_bytes,
            dir: config.resolve_dir(),
            rev: 0,
            saved_rev: 0,
        }
    }

    /// Creates the store and loads persisted history if any.
    pub fn load(config: &Config) -> Self {
        let mut s = Self::new(config);
        if let Some(dir) = s.dir.clone() {
            s.load_from(&dir);
        } else if !config.persist {
            // History is off: nothing from an earlier run may stay on disk.
            if let Some(dir) = config.history_dir() {
                purge_history(&dir);
            }
        }
        s
    }

    fn load_from(&mut self, dir: &Path) {
        let Ok(raw) = std::fs::read(dir.join("index.json")) else { return };
        let (index, mut migrate) = match crate::crypt::unprotect(&raw) {
            Some(b) => match serde_json::from_slice::<Index>(&b) {
                Ok(i) => (i, false),
                Err(_) => return,
            },
            // Written by an older version: plain JSON, rewritten encrypted.
            None => match serde_json::from_slice::<Index>(&raw) {
                Ok(i) => (i, true),
                Err(_) => return,
            },
        };
        let mut max_id = 0;
        for e in index.items {
            max_id = max_id.max(e.id);
            let content = match e.kind {
                Kind::Text { text } => ClipContent::Text(text),
                Kind::Files { paths } => {
                    ClipContent::Files(paths.into_iter().map(PathBuf::from).collect())
                }
                Kind::Image { file, width, height } => {
                    // Never follow a path out of the directory.
                    if file.contains(['/', '\\']) {
                        continue;
                    }
                    let path = dir.join(&file);
                    let Ok(raw) = std::fs::read(&path) else { continue };
                    let png = match crate::crypt::unprotect(&raw) {
                        Some(b) => b,
                        None if raw.starts_with(PNG_MAGIC) => {
                            // Plain image from an older version: encrypt in
                            // place. If that fails the item is kept and the
                            // file stays as it was for the next start.
                            let _ = write_encrypted(&path, &raw);
                            raw
                        }
                        // Corrupt or foreign: drop this item only.
                        None => {
                            migrate = true;
                            continue;
                        }
                    };
                    ClipContent::Image { png: png.into(), width, height }
                }
            };
            self.items.push(ClipItem {
                id: e.id,
                content,
                captured_at_ms: e.captured_at_ms,
                pinned: e.pinned,
            });
        }
        self.next_id = index.next_id.max(max_id + 1).max(1);
        // Caps may have shrunk since the last run. Dirty only if something went.
        let before = self.items.len();
        self.evict();
        if migrate || self.items.len() != before {
            self.rev += 1;
        }
    }

    /// Pinned items first, then the rest, each group newest first.
    pub fn list(&self) -> Vec<ClipItem> {
        let mut v = self.items.clone();
        v.sort_by_key(|i| !i.pinned); // stable
        v
    }

    pub fn get(&self, id: u64) -> Option<ClipItem> {
        self.items.iter().find(|i| i.id == id).cloned()
    }

    /// Adds content as the newest item. `None` when it is a consecutive
    /// duplicate, empty, or too large to keep.
    pub fn add(&mut self, content: ClipContent, now_ms: u64) -> Option<ClipItem> {
        match &content {
            ClipContent::Text(t) if t.trim().is_empty() || t.len() > MAX_TEXT_BYTES => {
                return None;
            }
            ClipContent::Files(f) if f.is_empty() => return None,
            ClipContent::Image { png, .. }
                if png.is_empty() || png.len() as u64 > self.max_image_bytes =>
            {
                return None;
            }
            _ => {}
        }
        if let Some(top) = self.items.first() {
            if content_eq(&top.content, &content) {
                return None;
            }
        }
        let item = ClipItem { id: self.next_id, content, captured_at_ms: now_ms, pinned: false };
        self.next_id += 1;
        self.items.insert(0, item.clone());
        self.evict();
        self.rev += 1;
        Some(item)
    }

    fn evict(&mut self) {
        let mut bytes: u64 = self.items.iter().map(|i| image_size(&i.content)).sum();
        // Walk from the oldest end, dropping unpinned items until within caps.
        let mut idx = self.items.len();
        while idx > 0 && (self.items.len() > self.max_items || bytes > self.max_image_bytes) {
            idx -= 1;
            if !self.items[idx].pinned {
                bytes -= image_size(&self.items[idx].content);
                self.items.remove(idx);
            }
        }
    }

    pub fn delete(&mut self, id: u64) -> bool {
        let n = self.items.len();
        self.items.retain(|i| i.id != id);
        let changed = self.items.len() != n;
        if changed {
            self.rev += 1;
        }
        changed
    }

    pub fn pin(&mut self, id: u64, pinned: bool) -> bool {
        match self.items.iter_mut().find(|i| i.id == id) {
            Some(i) if i.pinned != pinned => {
                i.pinned = pinned;
                self.rev += 1;
                true
            }
            _ => false,
        }
    }

    /// Removes every unpinned item.
    pub fn clear(&mut self) -> bool {
        let n = self.items.len();
        self.items.retain(|i| i.pinned);
        let changed = self.items.len() != n;
        if changed {
            self.rev += 1;
        }
        changed
    }

    pub fn is_dirty(&self) -> bool {
        self.dir.is_some() && self.rev != self.saved_rev
    }

    pub fn prepare_save(&self) -> Option<SaveJob> {
        if !self.is_dirty() {
            return None;
        }
        let dir = self.dir.clone()?;
        let mut images = Vec::new();
        let mut keep = HashSet::new();
        let mut entries = Vec::new();
        for i in &self.items {
            let kind = match &i.content {
                ClipContent::Text(t) => Kind::Text { text: t.clone() },
                ClipContent::Files(f) => Kind::Files {
                    paths: f.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                },
                ClipContent::Image { png, width, height } => {
                    let file = image_file(i.id);
                    keep.insert(file.clone());
                    images.push((file.clone(), png.clone()));
                    Kind::Image { file, width: *width, height: *height }
                }
            };
            entries.push(Entry {
                id: i.id,
                captured_at_ms: i.captured_at_ms,
                pinned: i.pinned,
                kind,
            });
        }
        let index = Index { version: 1, next_id: self.next_id, items: entries };
        let index_json = serde_json::to_string(&index).ok()?;
        Some(SaveJob { dir, index_json, images, keep, rev: self.rev })
    }

    pub fn mark_saved(&mut self, rev: u64) {
        self.saved_rev = rev;
    }

    #[cfg(test)]
    pub fn save_if_dirty(&mut self) -> std::io::Result<bool> {
        match self.prepare_save() {
            None => Ok(false),
            Some(job) => {
                job.write()?;
                self.mark_saved(job.rev);
                Ok(true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "nb-clipboard-test-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cfg(dir: Option<&TempDir>, max_items: usize, max_img: u64) -> Config {
        Config {
            max_items,
            max_image_bytes: max_img,
            dir: dir.map(|d| d.0.clone()),
            persist: dir.is_some(),
        }
    }
    fn text(s: &str) -> ClipContent {
        ClipContent::Text(s.to_string())
    }
    fn img(n: usize, fill: u8) -> ClipContent {
        ClipContent::Image { png: vec![fill; n].into(), width: 2, height: 2 }
    }

    #[test]
    fn newest_first_and_ids_increase() {
        let mut s = Store::new(&cfg(None, 10, 1000));
        let a = s.add(text("a"), 1).unwrap();
        let b = s.add(text("b"), 2).unwrap();
        assert!(b.id > a.id);
        let l = s.list();
        assert_eq!(l[0].id, b.id);
        assert_eq!(l[1].id, a.id);
    }

    #[test]
    fn consecutive_duplicates_dropped_nonconsecutive_kept() {
        let mut s = Store::new(&cfg(None, 10, 1000));
        assert!(s.add(text("a"), 1).is_some());
        assert!(s.add(text("a"), 2).is_none());
        assert!(s.add(text("b"), 3).is_some());
        assert!(s.add(text("a"), 4).is_some());
        assert_eq!(s.list().len(), 3);
        assert!(s.add(img(10, 1), 5).is_some());
        assert!(s.add(img(10, 1), 6).is_none());
        assert!(s.add(img(10, 2), 7).is_some());
    }

    #[test]
    fn empty_and_oversize_rejected() {
        let mut s = Store::new(&cfg(None, 10, 100));
        assert!(s.add(text("   \n"), 1).is_none());
        assert!(s.add(ClipContent::Files(vec![]), 1).is_none());
        assert!(s.add(img(101, 1), 1).is_none());
        assert!(s.add(text(&"x".repeat(MAX_TEXT_BYTES + 1)), 1).is_none());
    }

    #[test]
    fn item_cap_evicts_oldest() {
        let mut s = Store::new(&cfg(None, 3, 1000));
        for i in 0..5 {
            s.add(text(&format!("t{i}")), i).unwrap();
        }
        let l = s.list();
        assert_eq!(l.len(), 3);
        assert!(matches!(&l[0].content, ClipContent::Text(t) if t == "t4"));
        assert!(matches!(&l[2].content, ClipContent::Text(t) if t == "t2"));
    }

    #[test]
    fn image_byte_cap_evicts_oldest_images() {
        let mut s = Store::new(&cfg(None, 100, 100));
        s.add(img(60, 1), 1).unwrap();
        s.add(img(60, 2), 2).unwrap(); // 120 > 100, first goes
        let l = s.list();
        assert_eq!(l.len(), 1);
        assert_eq!(image_size(&l[0].content), 60);
    }

    #[test]
    fn pinned_survive_eviction_and_clear() {
        let mut s = Store::new(&cfg(None, 2, 1000));
        let a = s.add(text("a"), 1).unwrap();
        assert!(s.pin(a.id, true));
        for i in 0..4 {
            s.add(text(&format!("x{i}")), 10 + i).unwrap();
        }
        assert!(s.list().iter().any(|i| i.id == a.id));
        assert!(s.clear());
        let l = s.list();
        assert_eq!(l.len(), 1);
        assert!(l[0].pinned);
        assert!(!s.pin(a.id, true), "no change when already pinned");
    }

    #[test]
    fn delete_and_get() {
        let mut s = Store::new(&cfg(None, 10, 1000));
        let a = s.add(text("a"), 1).unwrap();
        assert!(s.get(a.id).is_some());
        assert!(s.delete(a.id));
        assert!(!s.delete(a.id));
        assert!(s.get(a.id).is_none());
    }

    #[test]
    fn persistence_roundtrip() {
        let d = TempDir::new("rt");
        let c = cfg(Some(&d), 10, 1_000_000);
        let mut s = Store::load(&c);
        s.add(text("h\u{e9}llo"), 1).unwrap();
        s.add(img(30, 7), 2).unwrap();
        let f = s.add(ClipContent::Files(vec![PathBuf::from(r"C:\a\b.txt")]), 3).unwrap();
        s.pin(f.id, true);
        assert!(s.save_if_dirty().unwrap());

        let s2 = Store::load(&c);
        let l = s2.list();
        assert_eq!(l.len(), 3);
        assert!(
            l[0].pinned
                && matches!(&l[0].content, ClipContent::Files(p) if p[0] == PathBuf::from(r"C:\a\b.txt"))
        );
        assert!(
            matches!(&l[1].content, ClipContent::Image { png, .. } if png[..] == [7u8; 30][..])
        );
        assert!(matches!(&l[2].content, ClipContent::Text(t) if t == "h\u{e9}llo"));
        let mut s2 = s2;
        assert!(!s2.is_dirty(), "loading alone does not dirty the store");
        let n = s2.add(text("new"), 9).unwrap();
        assert!(n.id > f.id, "ids stay unique across restarts");
    }

    #[test]
    fn written_only_on_change() {
        let d = TempDir::new("chg");
        let c = cfg(Some(&d), 10, 1000);
        let mut s = Store::load(&c);
        assert!(!s.save_if_dirty().unwrap(), "fresh empty store writes nothing");
        assert!(!d.0.join("index.json").exists());
        s.add(text("a"), 1).unwrap();
        assert!(s.save_if_dirty().unwrap());
        assert!(!s.save_if_dirty().unwrap(), "second save is a no-op");
        assert!(s.add(text("a"), 2).is_none());
        assert!(!s.save_if_dirty().unwrap(), "duplicate does not dirty");
        assert!(!s.delete(9999));
        assert!(!s.save_if_dirty().unwrap(), "failed delete does not dirty");
        s.clear();
        assert!(s.save_if_dirty().unwrap());
    }

    #[test]
    fn orphan_images_removed() {
        let d = TempDir::new("orph");
        let c = cfg(Some(&d), 10, 1000);
        let mut s = Store::load(&c);
        let i = s.add(img(10, 1), 1).unwrap();
        s.save_if_dirty().unwrap();
        assert!(d.0.join(image_file(i.id)).exists());
        s.delete(i.id);
        s.save_if_dirty().unwrap();
        assert!(!d.0.join(image_file(i.id)).exists());
    }

    #[test]
    fn corrupt_or_hostile_index_is_ignored() {
        let d = TempDir::new("bad");
        std::fs::create_dir_all(&d.0).unwrap();
        std::fs::write(d.0.join("index.json"), "{not json").unwrap();
        assert!(Store::load(&cfg(Some(&d), 10, 1000)).list().is_empty());
        std::fs::write(
            d.0.join("index.json"),
            r#"{"version":1,"next_id":5,"items":[{"id":1,"captured_at_ms":0,"pinned":false,"kind":"image","file":"..\\x.png","width":1,"height":1}]}"#,
        )
        .unwrap();
        assert!(Store::load(&cfg(Some(&d), 10, 1000)).list().is_empty());
    }

    fn read_dir_files(d: &TempDir) -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<_> = std::fs::read_dir(&d.0)
            .unwrap()
            .flatten()
            .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap()))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn nothing_readable_on_disk() {
        let d = TempDir::new("enc");
        let c = cfg(Some(&d), 10, 1000);
        let mut s = Store::load(&c);
        s.add(text("topsecretword"), 1).unwrap();
        s.add(ClipContent::Image { png: [&[0x89, b'P', b'N', b'G'][..], &[5u8; 20]].concat().into(), width: 2, height: 2 }, 2).unwrap();
        s.save_if_dirty().unwrap();
        let files = read_dir_files(&d);
        assert_eq!(files.len(), 2);
        for (_, b) in &files {
            assert!(!b.windows(13).any(|w| w == b"topsecretword"));
            assert!(!b.starts_with(PNG_MAGIC) && !b.starts_with(b"{"));
        }
        assert_eq!(Store::load(&c).list().len(), 2);
    }

    #[test]
    fn migrates_plain_store_without_loss() {
        let d = TempDir::new("mig");
        std::fs::create_dir_all(&d.0).unwrap();
        let png: Vec<u8> = [&[0x89, b'P', b'N', b'G'][..], &[9u8; 16]].concat();
        std::fs::write(d.0.join("7.png"), &png).unwrap();
        std::fs::write(
            d.0.join("index.json"),
            r#"{"version":1,"next_id":8,"items":[{"id":7,"captured_at_ms":5,"pinned":true,"kind":"image","file":"7.png","width":2,"height":2},{"id":6,"captured_at_ms":4,"pinned":false,"kind":"text","text":"plain note"}]}"#,
        )
        .unwrap();
        let c = cfg(Some(&d), 10, 1_000_000);
        let mut s = Store::load(&c);
        assert_eq!(s.list().len(), 2);
        assert!(s.is_dirty(), "index must be rewritten encrypted");
        assert!(s.save_if_dirty().unwrap());
        for (n, b) in read_dir_files(&d) {
            assert!(!b.starts_with(PNG_MAGIC) && !b.starts_with(b"{"), "{n} still plain");
        }
        let l = Store::load(&c).list();
        assert_eq!(l.len(), 2);
        assert!(l[0].pinned);
        assert!(matches!(&l[0].content, ClipContent::Image { png: p, .. } if p[..] == png[..]));
        assert!(matches!(&l[1].content, ClipContent::Text(t) if t == "plain note"));
    }

    #[test]
    fn corrupted_image_drops_only_that_item() {
        let d = TempDir::new("cor");
        let c = cfg(Some(&d), 10, 1_000_000);
        let mut s = Store::load(&c);
        let a = s.add(text("keep"), 1).unwrap();
        let i = s.add(img(30, 3), 2).unwrap();
        s.save_if_dirty().unwrap();
        let _ = a;
        let f = d.0.join(image_file(i.id));
        let mut b = std::fs::read(&f).unwrap();
        let n = b.len();
        b[n / 2] ^= 0xff;
        std::fs::write(&f, b).unwrap();
        let l = Store::load(&c).list();
        assert_eq!(l.len(), 1);
        assert!(matches!(&l[0].content, ClipContent::Text(t) if t == "keep"));
    }

    #[test]
    fn pinned_listed_first() {
        let mut s = Store::new(&cfg(None, 10, 1000));
        let a = s.add(text("a"), 1).unwrap();
        s.add(text("b"), 2).unwrap();
        s.pin(a.id, true);
        assert_eq!(s.list()[0].id, a.id);
    }

    #[test]
    fn persist_false_never_touches_disk() {
        let mut s = Store::new(&Config {
            persist: false,
            dir: Some(PathBuf::from("Z:\\nope")),
            ..Config::default()
        });
        s.add(text("a"), 1).unwrap();
        assert!(!s.is_dirty());
        assert!(s.prepare_save().is_none());
    }

    #[test]
    fn persist_false_deletes_an_old_history_folder() {
        let d = TempDir::new("purge");
        std::fs::create_dir_all(&d.0).unwrap();
        std::fs::write(d.0.join("index.json"), r#"{"version":1,"next_id":2,"items":[]}"#).unwrap();
        std::fs::write(d.0.join("1.png"), b"x").unwrap();
        let s = Store::load(&Config { persist: false, dir: Some(d.0.clone()), ..Config::default() });
        assert!(s.list().is_empty());
        assert!(!d.0.exists());
    }

    #[test]
    fn default_config_matches_spec() {
        let c = Config::default();
        assert_eq!(c.max_items, 100);
        assert_eq!(c.max_image_bytes, 64 * 1024 * 1024);
    }
}
