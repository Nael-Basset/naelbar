//! Media session service (Windows.Media.Control / GSMTC).
//!
//! Fully event driven: WinRT callbacks only post a message to one parked
//! worker thread, which rebuilds the state, deduplicates it and emits.
//! There is no polling and no timer.

mod meter;
pub use meter::{MAX_CHANNELS, PeakMeter};

use std::fmt;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use nb_core::{Event, MediaState, PlaybackStatus, Sink};
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as WinStatus,
};
use windows::Storage::Streams::DataReader;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

#[derive(Debug)]
pub enum Error {
    Windows(windows::core::Error),
    Thread(std::io::Error),
    /// The worker thread ended before reporting its start-up result.
    Stopped,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Windows(e) => write!(f, "windows error: {e}"),
            Error::Thread(e) => write!(f, "cannot spawn media thread: {e}"),
            Error::Stopped => write!(f, "media thread stopped unexpectedly"),
        }
    }
}

impl std::error::Error for Error {}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Windows(e)
    }
}

/// Which sessions may drive the island, and which one wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prefs {
    /// App ids (AUMID) that are never shown.
    pub disabled: Vec<String>,
    /// App id that wins while it plays. Empty: the most recent one wins.
    pub preferred: String,
}

enum Msg {
    /// `rebind`: the session set changed, re-register per-session handlers.
    /// `props`: media properties changed, artwork may be retried.
    Refresh { rebind: bool, props: bool },
    Prefs(Prefs),
    Toggle,
    Next,
    Previous,
    Seek(u64),
    Stop,
}

/// Dropping the handle stops the service.
pub struct Handle {
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl Handle {
    /// Play/pause toggle on the selected session. Never blocks.
    pub fn toggle(&self) {
        let _ = self.tx.send(Msg::Toggle);
    }
    pub fn next(&self) {
        let _ = self.tx.send(Msg::Next);
    }
    pub fn previous(&self) {
        let _ = self.tx.send(Msg::Previous);
    }
    /// Seek to an absolute position in milliseconds.
    pub fn seek(&self, position_ms: u64) {
        let _ = self.tx.send(Msg::Seek(position_ms));
    }
    /// Changes which session is shown. Never blocks.
    pub fn set_prefs(&self, prefs: Prefs) {
        let _ = self.tx.send(Msg::Prefs(prefs));
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn start(sink: Sink) -> Result<Handle, Error> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), Error>>();
    let worker_tx = tx.clone();
    let thread = std::thread::Builder::new()
        .name("nb-media".into())
        .spawn(move || run(sink, worker_tx, rx, ready_tx))
        .map_err(Error::Thread)?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Handle { tx, thread: Some(thread) }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => Err(Error::Stopped),
    }
}

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionInfo {
    app_id: String,
    status: PlaybackStatus,
    is_current: bool,
    /// Unix ms at which this app last started playing, 0 if never seen playing.
    started_ms: u64,
}

/// Picks the session to show among the apps that are not disabled:
/// the preferred app while it plays, else the playing session that started
/// last, else the preferred app, else the one that played last. Ties go to
/// the system's current session, then to the first one.
fn select_session(sessions: &[SessionInfo], prefs: &Prefs) -> Option<usize> {
    let allowed: Vec<usize> = (0..sessions.len()).filter(|&i| !prefs.disabled.iter().any(|d| *d == sessions[i].app_id)).collect();
    let playing: Vec<usize> = allowed.iter().copied().filter(|&i| sessions[i].status == PlaybackStatus::Playing).collect();
    let preferred = |set: &[usize]| {
        if prefs.preferred.is_empty() {
            None
        } else {
            set.iter().copied().find(|&i| sessions[i].app_id == prefs.preferred)
        }
    };
    let latest = |set: &[usize]| {
        let mut best: Option<usize> = None;
        for &i in set {
            let key = |j: usize| (sessions[j].started_ms, sessions[j].is_current);
            if best.is_none_or(|b| key(i) > key(b)) {
                best = Some(i);
            }
        }
        best
    };
    preferred(&playing).or_else(|| latest(&playing)).or_else(|| preferred(&allowed)).or_else(|| latest(&allowed))
}

/// Remembers when each app last started playing. Apps that vanished are forgotten.
#[derive(Default)]
struct StartTracker {
    /// app id, playing now, started at.
    apps: Vec<(String, bool, u64)>,
}

impl StartTracker {
    /// Updates the record from the sessions seen now and fills `started_ms`.
    fn update(&mut self, sessions: &mut [SessionInfo], now_ms: u64) {
        // Several sessions of one app (browser tabs) share one record.
        let mut next: Vec<(String, bool, u64)> = Vec::with_capacity(sessions.len());
        for s in sessions.iter() {
            let playing = s.status == PlaybackStatus::Playing;
            match next.iter_mut().find(|n| n.0 == s.app_id) {
                Some(n) => n.1 |= playing,
                None => next.push((s.app_id.clone(), playing, 0)),
            }
        }
        for n in &mut next {
            let (was_playing, started) = self.apps.iter().find(|a| a.0 == n.0).map_or((false, 0), |a| (a.1, a.2));
            n.2 = if n.1 && !was_playing { now_ms } else { started };
        }
        for s in sessions.iter_mut() {
            if let Some(n) = next.iter().find(|n| n.0 == s.app_id) {
                s.started_ms = n.2;
            }
        }
        self.apps = next;
    }
}

/// Sorted, deduplicated app ids.
fn app_list(sessions: &[SessionInfo]) -> Vec<String> {
    let mut v: Vec<String> = sessions.iter().map(|s| s.app_id.clone()).filter(|a| !a.is_empty()).collect();
    v.sort();
    v.dedup();
    v
}

/// A short readable name for an app id (AUMID).
pub fn app_label(app_id: &str) -> String {
    const KNOWN: &[(&str, &str)] = &[
        ("spotify", "Spotify"),
        ("chrome", "Chrome"),
        ("msedge", "Edge"),
        ("brave", "Brave"),
        ("firefox", "Firefox"),
        ("308046b0af4a39cb", "Firefox"),
        ("opera", "Opera"),
        ("vivaldi", "Vivaldi"),
        ("zunemusic", "Media Player"),
        ("vlc", "VLC"),
        ("deezer", "Deezer"),
        ("tidal", "TIDAL"),
        ("applemusic", "Apple Music"),
        ("foobar2000", "foobar2000"),
        ("musicbee", "MusicBee"),
        ("discord", "Discord"),
        ("youtube music", "YouTube Music"),
        ("music.youtube.com", "YouTube Music"),
    ];
    let lower = app_id.to_ascii_lowercase();
    // A packaged app is `Family_hash!App`; a desktop app is often a path or `name.exe`.
    let tail = lower.rsplit(['\\', '/']).next().unwrap_or(&lower);
    let tail = tail.split('!').next_back().unwrap_or(tail);
    for (key, name) in KNOWN {
        if tail.contains(key) || lower.contains(key) {
            return (*name).to_string();
        }
    }
    let original_tail = app_id.rsplit(['\\', '/']).next().unwrap_or(app_id);
    let base = original_tail.split('!').next().unwrap_or(original_tail);
    let base = base.split('_').next().unwrap_or(base);
    let base = base.strip_suffix(".exe").or_else(|| base.strip_suffix(".EXE")).unwrap_or(base);
    // `Publisher.App` keeps the last part.
    let base = base.rsplit('.').next().filter(|s| !s.is_empty()).unwrap_or(base);
    if base.is_empty() { app_id.to_string() } else { base.to_string() }
}

fn same_artwork(a: &Option<Arc<[u8]>>, b: &Option<Arc<[u8]>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a == b,
        _ => false,
    }
}

/// Equality used for deduplication. The sample timestamp only matters while
/// playing (it anchors the shell's extrapolation).
fn same_state(a: &MediaState, b: &MediaState) -> bool {
    a.app_id == b.app_id
        && a.title == b.title
        && a.artist == b.artist
        && a.album == b.album
        && a.status == b.status
        && a.position_ms == b.position_ms
        && a.duration_ms == b.duration_ms
        && (a.status != PlaybackStatus::Playing
            || a.position_sampled_at_ms == b.position_sampled_at_ms)
        && a.can_previous == b.can_previous
        && a.can_next == b.can_next
        && a.can_toggle == b.can_toggle
        && same_artwork(&a.artwork, &b.artwork)
}

/// Remembers the last emitted value; the first call always reports a change.
#[derive(Default)]
struct Dedup {
    last: Option<Option<MediaState>>,
}

impl Dedup {
    fn changed(&mut self, new: &Option<MediaState>) -> bool {
        let same = match (&self.last, new) {
            (Some(None), None) => true,
            (Some(Some(a)), Some(b)) => same_state(a, b),
            _ => false,
        };
        if !same {
            self.last = Some(new.clone());
        }
        !same
    }
}

fn track_key(app: &str, title: &str, artist: &str, album: &str) -> String {
    format!("{app}\u{1}{title}\u{1}{artist}\u{1}{album}")
}

/// Windows FILETIME-based ticks (100 ns since 1601) to unix milliseconds.
fn filetime_ticks_to_unix_ms(ticks: i64) -> Option<u64> {
    const EPOCH_DIFF_MS: i64 = 11_644_473_600_000;
    let ms = ticks / 10_000 - EPOCH_DIFF_MS;
    u64::try_from(ms).ok().filter(|&v| v > 0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn ticks_to_ms(ticks: i64) -> u64 {
    u64::try_from(ticks / 10_000).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

type Revoker = Box<dyn FnOnce()>;

struct Worker {
    sink: Sink,
    tx: Sender<Msg>,
    manager: Manager,
    selected: Option<Session>,
    session_revokers: Vec<Revoker>,
    manager_revokers: Vec<Revoker>,
    dedup: Dedup,
    art_key: String,
    art: Option<Arc<[u8]>>,
    prefs: Prefs,
    starts: StartTracker,
    /// Last app list sent, so it is only emitted when it changes.
    apps: Option<Vec<String>>,
}

fn run(sink: Sink, tx: Sender<Msg>, rx: Receiver<Msg>, ready: Sender<Result<(), Error>>) {
    // SAFETY: plain COM initialisation of this dedicated thread.
    let init = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    if let Err(e) = init {
        let _ = ready.send(Err(e.into()));
        return;
    }
    let manager = match Manager::RequestAsync().and_then(|op| op.join()) {
        Ok(m) => m,
        Err(e) => {
            let _ = ready.send(Err(e.into()));
            return;
        }
    };
    let mut w = Worker {
        sink,
        tx,
        manager,
        selected: None,
        session_revokers: Vec::new(),
        manager_revokers: Vec::new(),
        dedup: Dedup::default(),
        art_key: String::new(),
        art: None,
        prefs: Prefs::default(),
        starts: StartTracker::default(),
        apps: None,
    };
    if let Err(e) = w.register_manager() {
        let _ = ready.send(Err(e.into()));
        return;
    }
    w.rebind();
    w.refresh(true);
    let _ = ready.send(Ok(()));

    // Parked in the kernel until a WinRT callback or a command arrives.
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        let (mut rebind, mut props, mut refresh) = (false, false, false);
        for m in batch {
            match m {
                Msg::Stop => {
                    w.shutdown();
                    return;
                }
                Msg::Refresh { rebind: r, props: p } => {
                    rebind |= r;
                    props |= p;
                    refresh = true;
                }
                Msg::Prefs(p) => {
                    refresh |= w.prefs != p;
                    w.prefs = p;
                }
                Msg::Toggle => w.command(|s| s.TryTogglePlayPauseAsync().and_then(|o| o.join())),
                Msg::Next => w.command(|s| s.TrySkipNextAsync().and_then(|o| o.join())),
                Msg::Previous => w.command(|s| s.TrySkipPreviousAsync().and_then(|o| o.join())),
                Msg::Seek(ms) => {
                    let ticks = (ms as i64).saturating_mul(10_000);
                    w.command(|s| s.TryChangePlaybackPositionAsync(ticks).and_then(|o| o.join()));
                }
            }
        }
        if rebind {
            w.rebind();
        }
        if refresh {
            w.refresh(props);
        }
    }
    w.shutdown();
}

impl Worker {
    fn post(tx: &Sender<Msg>, rebind: bool, props: bool) {
        let _ = tx.send(Msg::Refresh { rebind, props });
    }

    fn register_manager(&mut self) -> windows::core::Result<()> {
        let tx = self.tx.clone();
        let h = TypedEventHandler::new(move |_, _| {
            Self::post(&tx, true, false);
            Ok(())
        });
        let t = self.manager.CurrentSessionChanged(&h)?;
        let m = self.manager.clone();
        self.manager_revokers.push(Box::new(move || {
            let _ = m.RemoveCurrentSessionChanged(t);
        }));

        let tx = self.tx.clone();
        let h = TypedEventHandler::new(move |_, _| {
            Self::post(&tx, true, false);
            Ok(())
        });
        let t = self.manager.SessionsChanged(&h)?;
        let m = self.manager.clone();
        self.manager_revokers.push(Box::new(move || {
            let _ = m.RemoveSessionsChanged(t);
        }));
        Ok(())
    }

    fn sessions(&self) -> Vec<Session> {
        match self.manager.GetSessions() {
            Ok(v) => v.into_iter().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Drops old per-session handlers and registers new ones on every session.
    fn rebind(&mut self) {
        for r in self.session_revokers.drain(..) {
            r();
        }
        for s in self.sessions() {
            // A session whose app just vanished may fail here; skip it.
            let _ = self.register_session(&s);
        }
    }

    fn register_session(&mut self, s: &Session) -> windows::core::Result<()> {
        let tx = self.tx.clone();
        let h = TypedEventHandler::new(move |_, _| {
            Self::post(&tx, false, true);
            Ok(())
        });
        let t = s.MediaPropertiesChanged(&h)?;
        let ss = s.clone();
        self.session_revokers.push(Box::new(move || {
            let _ = ss.RemoveMediaPropertiesChanged(t);
        }));

        let tx = self.tx.clone();
        let h = TypedEventHandler::new(move |_, _| {
            Self::post(&tx, false, false);
            Ok(())
        });
        let t = s.PlaybackInfoChanged(&h)?;
        let ss = s.clone();
        self.session_revokers.push(Box::new(move || {
            let _ = ss.RemovePlaybackInfoChanged(t);
        }));

        let tx = self.tx.clone();
        let h = TypedEventHandler::new(move |_, _| {
            Self::post(&tx, false, false);
            Ok(())
        });
        let t = s.TimelinePropertiesChanged(&h)?;
        let ss = s.clone();
        self.session_revokers.push(Box::new(move || {
            let _ = ss.RemoveTimelinePropertiesChanged(t);
        }));
        Ok(())
    }

    fn shutdown(&mut self) {
        for r in self.session_revokers.drain(..) {
            r();
        }
        for r in self.manager_revokers.drain(..) {
            r();
        }
    }

    fn command<F>(&self, f: F)
    where
        F: FnOnce(&Session) -> windows::core::Result<bool>,
    {
        if let Some(s) = &self.selected {
            // The app may have gone away; failures are not actionable.
            let _ = f(s);
        }
    }

    fn refresh(&mut self, props_changed: bool) {
        let sessions = self.sessions();
        let current_app = self
            .manager
            .GetCurrentSession()
            .and_then(|s| s.SourceAppUserModelId())
            .map(|h| h.to_string())
            .ok();
        let mut infos: Vec<SessionInfo> = sessions
            .iter()
            .map(|s| {
                let app_id = s.SourceAppUserModelId().map(|h| h.to_string()).unwrap_or_default();
                SessionInfo {
                    status: session_status(s).unwrap_or(PlaybackStatus::Stopped),
                    is_current: current_app.as_ref().is_some_and(|c| *c == app_id),
                    app_id,
                    started_ms: 0,
                }
            })
            .collect();
        self.starts.update(&mut infos, now_ms());
        let apps = app_list(&infos);
        if self.apps.as_ref() != Some(&apps) {
            self.apps = Some(apps.clone());
            (self.sink)(Event::MediaApps(apps));
        }
        let state = match select_session(&infos, &self.prefs) {
            None => {
                self.selected = None;
                None
            }
            Some(i) => {
                let s = &sessions[i];
                self.selected = Some(s.clone());
                match self.build_state(s, props_changed) {
                    Ok(st) => Some(st),
                    // Transient failure (app vanishing): keep the last state;
                    // a SessionsChanged event follows if it is really gone.
                    Err(_) => return,
                }
            }
        };
        if self.dedup.changed(&state) {
            (self.sink)(Event::Media(state));
        }
    }

    fn build_state(&mut self, s: &Session, props_changed: bool) -> windows::core::Result<MediaState> {
        let app_id = s.SourceAppUserModelId()?.to_string();
        let props = s.TryGetMediaPropertiesAsync()?.join()?;
        let title = props.Title()?.to_string();
        let artist = props.Artist()?.to_string();
        let album = props.AlbumTitle()?.to_string();

        let key = track_key(&app_id, &title, &artist, &album);
        if key != self.art_key {
            self.art_key = key;
            self.art = None;
            self.art = read_artwork(&props);
        } else if self.art.is_none() && props_changed {
            self.art = read_artwork(&props);
        }

        let info = s.GetPlaybackInfo()?;
        let status = convert_status(info.PlaybackStatus()?);
        let controls = info.Controls()?;
        let can_toggle = controls.IsPlayPauseToggleEnabled().unwrap_or(false)
            || controls.IsPlayEnabled().unwrap_or(false)
            || controls.IsPauseEnabled().unwrap_or(false);
        let can_next = controls.IsNextEnabled().unwrap_or(false);
        let can_previous = controls.IsPreviousEnabled().unwrap_or(false);

        let (mut position_ms, mut duration_ms, mut sampled) = (0, 0, now_ms());
        if let Ok(t) = s.GetTimelineProperties() {
            let start = t.StartTime().map_or(0, |d| d.Duration);
            let end = t.EndTime().map_or(0, |d| d.Duration);
            let pos = t.Position().map_or(0, |d| d.Duration);
            duration_ms = ticks_to_ms(end - start);
            position_ms = ticks_to_ms(pos - start);
            if let Some(ms) = t
                .LastUpdatedTime()
                .ok()
                .and_then(|d| filetime_ticks_to_unix_ms(d.UniversalTime))
            {
                sampled = ms;
            }
        }

        Ok(MediaState {
            app_id,
            title,
            artist,
            album,
            status,
            position_ms,
            duration_ms,
            position_sampled_at_ms: sampled,
            artwork: self.art.clone(),
            can_previous,
            can_next,
            can_toggle,
        })
    }
}

fn session_status(s: &Session) -> windows::core::Result<PlaybackStatus> {
    Ok(convert_status(s.GetPlaybackInfo()?.PlaybackStatus()?))
}

fn convert_status(s: WinStatus) -> PlaybackStatus {
    if s == WinStatus::Playing {
        PlaybackStatus::Playing
    } else if s == WinStatus::Paused {
        PlaybackStatus::Paused
    } else {
        PlaybackStatus::Stopped
    }
}

fn read_artwork(
    props: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties,
) -> Option<Arc<[u8]>> {
    let read = || -> windows::core::Result<Vec<u8>> {
        let stream = props.Thumbnail()?.OpenReadAsync()?.join()?;
        let size = u32::try_from(stream.Size()?).unwrap_or(0);
        if size == 0 || size > 32 * 1024 * 1024 {
            return Ok(Vec::new());
        }
        let reader = DataReader::CreateDataReader(&stream)?;
        reader.LoadAsync(size)?.join()?;
        let mut buf = vec![0u8; size as usize];
        reader.ReadBytes(&mut buf)?;
        Ok(buf)
    };
    match read() {
        Ok(b) if !b.is_empty() => Some(Arc::from(b)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(status: PlaybackStatus, is_current: bool) -> SessionInfo {
        SessionInfo { app_id: String::new(), status, is_current, started_ms: 0 }
    }

    fn app(id: &str, status: PlaybackStatus, started_ms: u64) -> SessionInfo {
        SessionInfo { app_id: id.into(), status, is_current: false, started_ms }
    }

    fn select(s: &[SessionInfo]) -> Option<usize> {
        select_session(s, &Prefs::default())
    }

    fn prefs(disabled: &[&str], preferred: &str) -> Prefs {
        Prefs { disabled: disabled.iter().map(|s| s.to_string()).collect(), preferred: preferred.into() }
    }

    #[test]
    fn select_most_recent_playing() {
        let s = [app("Spotify.exe", PlaybackStatus::Playing, 100), app("Chrome", PlaybackStatus::Playing, 200)];
        assert_eq!(select(&s), Some(1));
    }

    #[test]
    fn select_skips_disabled_apps() {
        let s = [app("Spotify.exe", PlaybackStatus::Paused, 100), app("Chrome", PlaybackStatus::Playing, 200)];
        assert_eq!(select_session(&s, &prefs(&["Chrome"], "")), Some(0));
        assert_eq!(select_session(&s, &prefs(&["Chrome", "Spotify.exe"], "")), None);
    }

    #[test]
    fn select_preferred_wins_only_while_playing() {
        let mut s = [app("Spotify.exe", PlaybackStatus::Playing, 100), app("Chrome", PlaybackStatus::Playing, 200)];
        let p = prefs(&[], "Spotify.exe");
        assert_eq!(select_session(&s, &p), Some(0));
        s[0].status = PlaybackStatus::Paused;
        assert_eq!(select_session(&s, &p), Some(1));
        s[1].status = PlaybackStatus::Paused;
        assert_eq!(select_session(&s, &p), Some(0));
        // A disabled preferred app is never shown.
        assert_eq!(select_session(&s, &prefs(&["Spotify.exe"], "Spotify.exe")), Some(1));
    }

    #[test]
    fn select_paused_falls_back_to_last_played() {
        let s = [app("a", PlaybackStatus::Paused, 300), app("b", PlaybackStatus::Paused, 500), app("c", PlaybackStatus::Stopped, 0)];
        assert_eq!(select(&s), Some(1));
    }

    #[test]
    fn tracker_records_start_of_playback() {
        let mut t = StartTracker::default();
        let mut s = vec![app("a", PlaybackStatus::Playing, 0), app("b", PlaybackStatus::Paused, 0)];
        t.update(&mut s, 10);
        assert_eq!((s[0].started_ms, s[1].started_ms), (10, 0));
        s[1].status = PlaybackStatus::Playing;
        t.update(&mut s, 20);
        // Still playing keeps its start; a new start is newer.
        assert_eq!((s[0].started_ms, s[1].started_ms), (10, 20));
        assert_eq!(select(&s), Some(1));
        s[0].status = PlaybackStatus::Paused;
        t.update(&mut s, 30);
        s[0].status = PlaybackStatus::Playing;
        t.update(&mut s, 40);
        assert_eq!(select(&s), Some(0));
    }

    #[test]
    fn tracker_shares_one_record_per_app() {
        let mut t = StartTracker::default();
        let mut s = vec![app("Chrome", PlaybackStatus::Paused, 0), app("Chrome", PlaybackStatus::Playing, 0)];
        t.update(&mut s, 7);
        assert_eq!((s[0].started_ms, s[1].started_ms), (7, 7));
        assert_eq!(app_list(&s), vec!["Chrome".to_string()]);
    }

    #[test]
    fn labels_are_readable() {
        assert_eq!(app_label("Spotify.exe"), "Spotify");
        assert_eq!(app_label("308046B0AF4A39CB"), "Firefox");
        assert_eq!(app_label("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"), "Media Player");
        assert_eq!(app_label("C:\\Program Files\\Foo\\Bar.exe"), "Bar");
        assert_eq!(app_label("Contoso.Player_abc!App"), "Player");
        assert_eq!(app_label(""), "");
    }

    fn state(title: &str) -> MediaState {
        MediaState {
            app_id: "app".into(),
            title: title.into(),
            artist: "a".into(),
            album: "b".into(),
            status: PlaybackStatus::Playing,
            position_ms: 1000,
            duration_ms: 5000,
            position_sampled_at_ms: 42,
            artwork: None,
            can_previous: true,
            can_next: true,
            can_toggle: true,
        }
    }

    #[test]
    fn select_empty() {
        assert_eq!(select(&[]), None);
    }

    #[test]
    fn select_prefers_playing_over_current() {
        let s = [
            info(PlaybackStatus::Paused, true),
            info(PlaybackStatus::Playing, false),
        ];
        assert_eq!(select(&s), Some(1));
    }

    #[test]
    fn select_prefers_current_when_both_playing() {
        let s = [
            info(PlaybackStatus::Playing, false),
            info(PlaybackStatus::Playing, true),
        ];
        assert_eq!(select(&s), Some(1));
    }

    #[test]
    fn select_falls_back_to_current_then_first() {
        let s = [
            info(PlaybackStatus::Paused, false),
            info(PlaybackStatus::Stopped, true),
        ];
        assert_eq!(select(&s), Some(1));
        let s = [info(PlaybackStatus::Paused, false)];
        assert_eq!(select(&s), Some(0));
    }

    #[test]
    fn dedup_first_always_emits_then_suppresses() {
        let mut d = Dedup::default();
        assert!(d.changed(&None));
        assert!(!d.changed(&None));
        let a = Some(state("x"));
        assert!(d.changed(&a));
        assert!(!d.changed(&Some(state("x"))));
        assert!(d.changed(&Some(state("y"))));
        assert!(d.changed(&None));
    }

    #[test]
    fn dedup_sample_time_matters_only_when_playing() {
        let mut d = Dedup::default();
        let mut s = state("x");
        assert!(d.changed(&Some(s.clone())));
        s.position_sampled_at_ms = 99;
        assert!(d.changed(&Some(s.clone())));
        s.status = PlaybackStatus::Paused;
        assert!(d.changed(&Some(s.clone())));
        s.position_sampled_at_ms = 100;
        assert!(!d.changed(&Some(s)));
    }

    #[test]
    fn dedup_detects_artwork_change() {
        let mut d = Dedup::default();
        let mut s = state("x");
        assert!(d.changed(&Some(s.clone())));
        s.artwork = Some(Arc::from(vec![1u8, 2, 3]));
        assert!(d.changed(&Some(s.clone())));
        let mut t = s.clone();
        t.artwork = Some(Arc::from(vec![1u8, 2, 3]));
        assert!(!d.changed(&Some(t)));
    }

    #[test]
    fn filetime_conversion() {
        // 2020-01-01T00:00:00Z
        let ticks = (1_577_836_800_000i64 + 11_644_473_600_000) * 10_000;
        assert_eq!(filetime_ticks_to_unix_ms(ticks), Some(1_577_836_800_000));
        assert_eq!(filetime_ticks_to_unix_ms(0), None);
    }
}
