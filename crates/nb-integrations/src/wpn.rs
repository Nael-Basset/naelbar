//! Windows notification mirror without package identity.
//!
//! `UserNotificationListener` needs an MSIX package, so this reads the user's
//! own notification database instead, read-only and shared:
//! `%LOCALAPPDATA%\Microsoft\Windows\Notifications\wpndatabase.db`.
//!
//! Idle rule: one thread parked in `ReadDirectoryChangesW`, a kernel wait. It
//! wakes only when the folder changes, reads the rows newer than the last
//! one it has seen, and parks again. Nothing is ever written to the database.
//!
//! Schema relied on (names only): `Notification` (`Id`, `HandlerId`, `Type`,
//! `Payload`, `ArrivalTime`) joined to `NotificationHandler` (`RecordId`,
//! `PrimaryId`, the application user model id).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nb_core::{Event, Notification, Sink};
use rusqlite::{Connection, OpenFlags};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows::Win32::System::IO::CancelIoEx;
use windows::core::HSTRING;

use crate::{Error, now_ms};

const DB_NAME: &str = "wpndatabase.db";
/// Changes arrive in bursts (the file, then its log); wait for the burst to end.
const SETTLE: Duration = Duration::from_millis(150);
/// Windows epoch (1601) to Unix epoch (1970), in milliseconds.
const FILETIME_UNIX_OFFSET_MS: i64 = 11_644_473_600_000;

/// Dropping stops the watcher thread.
pub struct WpnMirror {
    stop: Arc<AtomicBool>,
    dir: isize,
}

impl Drop for WpnMirror {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Cancels the blocked call. The thread closes the handle itself.
        unsafe {
            let _ = CancelIoEx(HANDLE(self.dir as *mut _), None);
        }
    }
}

fn db_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Microsoft").join("Windows").join("Notifications"))
}

pub fn start_wpn_mirror(sink: Sink) -> Result<WpnMirror, Error> {
    let dir = db_dir().ok_or_else(|| Error::Unsupported("LOCALAPPDATA is not set".into()))?;
    let db = dir.join(DB_NAME);
    if !db.exists() {
        return Err(Error::Unsupported("notification database not found".into()));
    }
    let handle = unsafe {
        CreateFileW(
            &HSTRING::from(dir.as_os_str()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )?
    };
    let raw = handle.0 as isize;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("nb-wpn".into())
        .spawn(move || {
            let handle = HANDLE(raw as *mut _);
            watch(handle, &db, &flag, &sink);
            unsafe {
                let _ = CloseHandle(handle);
            }
        })
        .map_err(Error::Io)?;
    Ok(WpnMirror { stop, dir: raw })
}

fn watch(handle: HANDLE, db: &Path, stop: &AtomicBool, sink: &Sink) {
    let mut mark = HighWater::default();
    // Whatever is already there is old: remember where it ends.
    let _ = poll(db, &mut mark);
    // 4-byte aligned, as the API requires.
    let mut buf = vec![0u32; 4096];
    while !stop.load(Ordering::SeqCst) {
        let mut returned = 0u32;
        let ok = unsafe {
            ReadDirectoryChangesW(
                handle,
                buf.as_mut_ptr().cast(),
                (buf.len() * 4) as u32,
                false,
                FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE,
                Some(&mut returned),
                None,
                None,
            )
        };
        if stop.load(Ordering::SeqCst) || ok.is_err() {
            return;
        }
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len() * 4) };
        if !touches_database(bytes, returned as usize) {
            continue;
        }
        std::thread::sleep(SETTLE);
        let rows = poll(db, &mut mark).or_else(|_| {
            std::thread::sleep(Duration::from_millis(250));
            poll(db, &mut mark)
        });
        for row in rows.unwrap_or_default() {
            if let Some(n) = to_notification(&row) {
                sink(Event::Notification(n));
            }
        }
    }
}

/// True when the change list names the database or its log. The shared-memory
/// file is ignored: reading the database touches it, which would loop.
/// A zero length means the buffer overflowed, so assume the worst.
fn touches_database(buf: &[u8], len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let len = len.min(buf.len());
    let mut at = 0usize;
    while at + 12 <= len {
        let next = u32::from_le_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
        let name_len = u32::from_le_bytes(buf[at + 8..at + 12].try_into().unwrap()) as usize;
        let start = at + 12;
        if start + name_len <= len {
            let units: Vec<u16> = buf[start..start + name_len].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let name = String::from_utf16_lossy(&units).to_ascii_lowercase();
            if name == DB_NAME || name == format!("{DB_NAME}-wal") {
                return true;
            }
        }
        if next == 0 {
            break;
        }
        at += next;
    }
    false
}

/// One row of the join, as stored.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    id: i64,
    /// Windows FILETIME: 100 ns ticks since 1601.
    arrival: i64,
    app_id: String,
    payload: Vec<u8>,
}

/// Remembers the last notification id seen, so each is emitted once.
#[derive(Debug, Default)]
struct HighWater {
    mark: Option<i64>,
}

impl HighWater {
    /// The id after which rows are new, `None` before the first read.
    fn after(&self) -> Option<i64> {
        self.mark
    }

    /// Takes the newest id in the table (`None` when empty) and the rows read
    /// after the previous mark. The first call only sets the mark and returns
    /// nothing. A table that restarted below the mark (database rebuilt)
    /// resets it. Rows come back oldest first.
    fn advance(&mut self, newest: Option<i64>, mut rows: Vec<Row>) -> Vec<Row> {
        let newest = newest.unwrap_or(0);
        let Some(mark) = self.mark else {
            self.mark = Some(newest);
            return Vec::new();
        };
        if newest < mark {
            self.mark = Some(newest);
            return Vec::new();
        }
        rows.retain(|r| r.id > mark);
        rows.sort_by_key(|r| r.id);
        self.mark = Some(newest.max(mark));
        rows
    }
}

fn open(db: &Path) -> rusqlite::Result<Connection> {
    // Read-only and shared: never writes, never holds a lock across calls.
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    c.busy_timeout(Duration::from_millis(300))?;
    c.execute_batch("PRAGMA query_only = 1;")?;
    Ok(c)
}

/// Reads what is new since `mark` and advances it.
fn poll(db: &Path, mark: &mut HighWater) -> rusqlite::Result<Vec<Row>> {
    let conn = open(db)?;
    let (newest, rows) = read_since(&conn, mark.after())?;
    Ok(mark.advance(newest, rows))
}

fn read_since(conn: &Connection, after: Option<i64>) -> rusqlite::Result<(Option<i64>, Vec<Row>)> {
    let newest: Option<i64> = conn.query_row("SELECT MAX(Id) FROM Notification", [], |r| r.get(0))?;
    let Some(after) = after else { return Ok((newest, Vec::new())) };
    let mut stmt = conn.prepare(
        "SELECT n.Id, n.ArrivalTime, h.PrimaryId, n.Payload \
         FROM Notification n JOIN NotificationHandler h ON h.RecordId = n.HandlerId \
         WHERE n.Type = 'toast' AND n.Id > ?1 ORDER BY n.Id",
    )?;
    let rows = stmt
        .query_map([after], |r| {
            Ok(Row {
                id: r.get(0)?,
                arrival: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                app_id: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                payload: r.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((newest, rows))
}

fn to_notification(row: &Row) -> Option<Notification> {
    if is_own(&row.app_id) {
        return None;
    }
    let xml = decode_payload(&row.payload);
    let (title, body) = parse_toast(&xml);
    if title.is_empty() && body.is_empty() {
        return None;
    }
    let now = now_ms();
    let arrived = filetime_to_unix_ms(row.arrival).filter(|&t| t <= now + 60_000 && now.saturating_sub(t) < 86_400_000).unwrap_or(now);
    Some(Notification { source: app_display_name(&row.app_id), title, body, received_at_ms: arrived, app_id: row.app_id.clone() })
}

fn is_own(app_id: &str) -> bool {
    app_id.to_ascii_lowercase().contains("naelbar")
}

/// True for naelbar itself and for the applications the user muted.
pub fn is_ignored_app(app_id: &str, ignore: &[String]) -> bool {
    is_own(app_id) || (!app_id.is_empty() && ignore.iter().any(|i| i.eq_ignore_ascii_case(app_id)))
}

fn filetime_to_unix_ms(ft: i64) -> Option<u64> {
    if ft <= 0 {
        return None;
    }
    let ms = ft / 10_000 - FILETIME_UNIX_OFFSET_MS;
    (ms > 0).then_some(ms as u64)
}

fn decode_payload(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && ((bytes[0] == 0xFF && bytes[1] == 0xFE) || (bytes[1] == 0 && bytes[0] != 0)) {
        let body = if bytes[0] == 0xFF { &bytes[2..] } else { bytes };
        let units: Vec<u16> = body.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)).into_owned()
}

/// Title and body: the first and second non-empty `<text>` of the toast,
/// leaving out the attribution line.
fn parse_toast(xml: &str) -> (String, String) {
    let mut texts = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<text") {
        rest = &rest[i + 5..];
        let Some(c) = rest.chars().next() else { break };
        if !(c.is_whitespace() || c == '>' || c == '/') {
            continue; // <textbox>, <textarea>...
        }
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        if tag.ends_with('/') {
            continue;
        }
        let close = rest.find("</text>").unwrap_or(rest.len());
        let inner = &rest[..close];
        rest = &rest[(close + 7).min(rest.len())..];
        if tag.contains("attribution") {
            continue;
        }
        let t = decode_entities(&strip_tags(inner)).split_whitespace().collect::<Vec<_>>().join(" ");
        if !t.is_empty() {
            texts.push(t);
        }
    }
    let mut it = texts.into_iter();
    (it.next().unwrap_or_default(), it.next().unwrap_or_default())
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = false;
    for c in s.chars() {
        match c {
            '<' => depth = true,
            '>' if depth => depth = false,
            _ if !depth => out.push(c),
            _ => {}
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';').filter(|&p| p <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .or_else(|| ent.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A readable name from an application user model id: the package name of
/// `Vendor.App_hash!Entry`, the last word of `com.vendor.App`, or the id.
fn app_display_name(aumid: &str) -> String {
    let id = aumid.split('!').next().unwrap_or(aumid);
    let id = id.split('_').next().unwrap_or(id);
    let id = id.rsplit(['\\', '/']).next().unwrap_or(id);
    let id = id.strip_suffix(".exe").or_else(|| id.strip_suffix(".EXE")).unwrap_or(id);
    let word = if id.contains('.') && !id.contains(' ') { id.rsplit('.').find(|w| !w.is_empty()).unwrap_or(id) } else { id };
    // Split CamelCase: "WindowsStore" -> "Windows Store".
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in word.chars() {
        if c.is_uppercase() && prev.is_some_and(|p| p.is_lowercase()) {
            out.push(' ');
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_title_and_body_in_order() {
        let xml = r#"<toast launch="x"><visual><binding template="ToastGeneric"><text id="1">Anna</text><text id="2">Are you free at 5 &amp; later?</text></binding></visual></toast>"#;
        assert_eq!(parse_toast(xml), ("Anna".into(), "Are you free at 5 & later?".into()));
    }

    #[test]
    fn skips_attribution_empty_and_lookalike_tags() {
        let xml = r#"<binding><text/><textbox id="a"/><text>Build done</text><text placement="attribution">via Mail</text><text>All 12 tests passed</text></binding>"#;
        assert_eq!(parse_toast(xml), ("Build done".into(), "All 12 tests passed".into()));
    }

    #[test]
    fn strips_nested_markup_and_numeric_entities() {
        let xml = "<text>caf&#233; <b>bold</b> &#x1F600;</text>";
        assert_eq!(parse_toast(xml).0, "café bold \u{1F600}");
        assert_eq!(parse_toast("<toast></toast>"), (String::new(), String::new()));
        assert_eq!(parse_toast("<text>a & b</text>").0, "a & b");
    }

    #[test]
    fn decodes_utf16_and_bom_payloads() {
        let s = "<text>Hé</text>";
        let mut le = vec![0xFF, 0xFE];
        for u in s.encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_payload(&le), s);
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(s.as_bytes());
        assert_eq!(decode_payload(&bom), s);
        assert_eq!(decode_payload(s.as_bytes()), s);
    }

    #[test]
    fn filetime_converts_to_unix_ms() {
        assert_eq!(filetime_to_unix_ms(0), None);
        // 2021-01-01T00:00:00Z
        assert_eq!(filetime_to_unix_ms(132_539_328_000_000_000), Some(1_609_459_200_000));
    }

    #[test]
    fn names_applications_readably() {
        assert_eq!(app_display_name("Microsoft.WindowsStore_8wekyb3d8bbwe!App"), "Windows Store");
        assert_eq!(app_display_name("com.squirrel.Discord.Discord"), "Discord");
        assert_eq!(app_display_name("Chrome"), "Chrome");
        assert_eq!(app_display_name("C:\\Tools\\MyApp.exe"), "My App");
    }

    #[test]
    fn ignores_itself_and_muted_apps() {
        let list = vec!["Spotify.Music".to_string()];
        assert!(is_ignored_app("naelbar.app", &[]));
        assert!(is_ignored_app("spotify.music", &list));
        assert!(!is_ignored_app("Other", &list));
        assert!(!is_ignored_app("", &["".to_string()]));
    }

    fn row(id: i64) -> Row {
        Row { id, arrival: 0, app_id: "a".into(), payload: Vec::new() }
    }

    #[test]
    fn high_water_emits_nothing_old_then_only_new_once() {
        let mut hw = HighWater::default();
        assert!(hw.advance(Some(10), vec![row(9), row(10)]).is_empty(), "startup emits nothing");
        assert_eq!(hw.after(), Some(10));
        let got = hw.advance(Some(13), vec![row(12), row(11), row(10)]);
        assert_eq!(got.iter().map(|r| r.id).collect::<Vec<_>>(), vec![11, 12]);
        assert_eq!(hw.after(), Some(13), "also covers rows that are not toasts");
        assert!(hw.advance(Some(13), vec![row(12)]).is_empty());
    }

    #[test]
    fn high_water_survives_empty_and_rebuilt_tables() {
        let mut hw = HighWater::default();
        assert!(hw.advance(None, vec![]).is_empty());
        assert_eq!(hw.after(), Some(0));
        assert_eq!(hw.advance(Some(1), vec![row(1)]).len(), 1);
        assert!(hw.advance(Some(0), vec![]).is_empty());
        assert_eq!(hw.advance(Some(2), vec![row(1), row(2)]).len(), 2, "after a reset ids count again");
    }

    #[test]
    fn change_list_only_matters_for_the_database_and_its_log() {
        fn entry(name: &str, last: bool) -> Vec<u8> {
            let units: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            let total = (12 + units.len()).next_multiple_of(4);
            let mut v = vec![0u8; total];
            let next = if last { 0 } else { total as u32 };
            v[0..4].copy_from_slice(&next.to_le_bytes());
            v[4..8].copy_from_slice(&3u32.to_le_bytes());
            v[8..12].copy_from_slice(&(units.len() as u32).to_le_bytes());
            v[12..12 + units.len()].copy_from_slice(&units);
            v
        }
        let shm = entry("wpndatabase.db-shm", true);
        assert!(!touches_database(&shm, shm.len()));
        let mut both = entry("WPNPRMRY.tmp", false);
        both.extend(entry("wpndatabase.db-wal", true));
        assert!(touches_database(&both, both.len()));
        assert!(touches_database(&shm, 0), "overflow means check anyway");
    }

    fn temp_db(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("nb-wpn-{tag}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE NotificationHandler (RecordId INTEGER PRIMARY KEY, PrimaryId TEXT);
             CREATE TABLE Notification (\"Order\" INTEGER, Id INTEGER PRIMARY KEY, HandlerId INTEGER, Type TEXT, Payload BLOB, ArrivalTime INT64);
             INSERT INTO NotificationHandler VALUES (1, 'Vendor.Mail_abc!App'), (2, 'Other.App');
             INSERT INTO Notification (Id, HandlerId, Type, Payload, ArrivalTime) VALUES
               (1, 1, 'toast', X'3C746578743E4F6C643C2F746578743E', 134000000000000000),
               (2, 2, 'tile', X'3C746578743E54696C653C2F746578743E', 134000000000000000);",
        )
        .unwrap();
        p
    }

    #[test]
    fn reads_only_new_toasts_from_a_database_with_the_same_schema() {
        let p = temp_db("read");
        let mut hw = HighWater::default();
        assert!(poll(&p, &mut hw).unwrap().is_empty(), "existing rows are old");
        assert_eq!(hw.after(), Some(2));

        let w = Connection::open(&p).unwrap();
        let toast = "<toast><visual><binding><text>Hello</text><text>World</text></binding></visual></toast>";
        w.execute("INSERT INTO Notification (Id, HandlerId, Type, Payload, ArrivalTime) VALUES (3, 1, 'toast', ?1, 134000000000000000)", [toast.as_bytes()]).unwrap();
        w.execute("INSERT INTO Notification (Id, HandlerId, Type, Payload, ArrivalTime) VALUES (4, 2, 'badge', X'00', 0)", []).unwrap();
        drop(w);

        let rows = poll(&p, &mut hw).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].app_id, "Vendor.Mail_abc!App");
        let n = to_notification(&rows[0]).unwrap();
        assert_eq!((n.title.as_str(), n.body.as_str(), n.source.as_str()), ("Hello", "World", "Mail"));
        assert_eq!(n.app_id, "Vendor.Mail_abc!App");
        assert!(poll(&p, &mut hw).unwrap().is_empty(), "nothing is emitted twice");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_reader_cannot_write() {
        let p = temp_db("ro");
        let c = open(&p).unwrap();
        assert!(c.execute("DELETE FROM Notification", []).is_err());
        let _ = std::fs::remove_file(&p);
    }
}
