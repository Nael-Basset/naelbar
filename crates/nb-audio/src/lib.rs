//! Core Audio service for naelbar: output/input volume, mute, default devices
//! and "microphone in use", entirely driven by COM callbacks.
//!
//! # Threading model
//!
//! One worker thread enters the COM multithreaded apartment, owns every COM
//! object and blocks on a channel. Core Audio callbacks (which arrive on
//! arbitrary RPC/audio threads) only push a small message on that channel and
//! return, so they never block and never re-enter Core Audio. Handle methods
//! also only enqueue a command, so they never block the caller. Dropping the
//! [`Handle`] enqueues a stop message and joins the worker, which unregisters
//! every callback before releasing anything.
//!
//! The worker parks in `Receiver::recv`: with no audio activity there is no
//! periodic work at all.

mod policy;

use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle, ThreadId};

use nb_core::{AudioDevice, AudioState, Event, Sink};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::{
    AUDIO_VOLUME_NOTIFICATION_DATA, AudioSessionDisconnectReason, AudioSessionState,
    AudioSessionStateActive, DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole,
    IAudioSessionControl, IAudioSessionControl2, IAudioSessionEvents, IAudioSessionEvents_Impl,
    IAudioSessionManager2, IAudioSessionNotification, IAudioSessionNotification_Impl, IMMDevice,
    IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
    eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::core::{GUID, PCWSTR, PWSTR, Result as WinResult, implement};

/// Errors from the audio service.
#[derive(Debug, Clone)]
pub enum Error {
    /// A Windows API call failed.
    Com { code: i32, message: String },
    /// The worker thread is gone (service stopped).
    Stopped,
    /// The requested operation is not available on this system.
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Com { code, message } => write!(f, "audio COM error {code:#010x}: {message}"),
            Error::Stopped => f.write_str("audio service stopped"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Com { code: e.code().0, message: e.message() }
    }
}

// ---------------------------------------------------------------------------
// Pure logic (unit tested)
// ---------------------------------------------------------------------------

/// Clamp to 0.0..=1.0, mapping NaN to 0.0.
fn clamp01(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// Whether a capture session counts as "microphone in use".
fn counts_as_mic_use(pid: u32, own_pid: u32, is_system_sounds: bool, active: bool) -> bool {
    active && !is_system_sounds && pid != own_pid
}

fn device_eq(a: &Option<AudioDevice>, b: &Option<AudioDevice>) -> bool {
    a == b
}

/// Field-by-field equality, since `AudioState` does not implement `PartialEq`.
fn state_eq(a: &AudioState, b: &AudioState) -> bool {
    a.output_volume == b.output_volume
        && a.output_muted == b.output_muted
        && a.mic_volume == b.mic_volume
        && a.mic_muted == b.mic_muted
        && a.mic_in_use == b.mic_in_use
        && device_eq(&a.output, &b.output)
        && device_eq(&a.input, &b.input)
        && a.outputs == b.outputs
        && a.inputs == b.inputs
}

/// Remembers the last emitted state and tells whether a new one differs.
#[derive(Default)]
struct Dedup {
    last: Option<AudioState>,
}

impl Dedup {
    fn changed(&mut self, s: &AudioState) -> bool {
        if self.last.as_ref().is_some_and(|l| state_eq(l, s)) {
            return false;
        }
        self.last = Some(s.clone());
        true
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Wrapper to move a COM pointer created in a callback to the worker. The
/// process uses the MTA, so the object is free-threaded and may be used from
/// any thread of it.
struct SendCom<T>(T);
// SAFETY: see above; the worker thread is in the MTA and every callback
// thread delivering these objects is an MTA thread too.
unsafe impl<T> Send for SendCom<T> {}

enum Cmd {
    OutputVolume(f32),
    OutputMuted(bool),
    MicVolume(f32),
    MicMuted(bool),
    DefaultOutput(String),
    DefaultInput(String),
}

enum Msg {
    /// Default device or device list changed.
    Devices,
    /// An endpoint volume or mute changed.
    Volume,
    SessionCreated(SendCom<IAudioSessionControl>),
    SessionState(String, bool),
    SessionGone(String),
    Cmd(Cmd),
    Stop,
}

// ---------------------------------------------------------------------------
// COM callback objects: each only enqueues a message.
// ---------------------------------------------------------------------------

#[implement(IMMNotificationClient)]
struct DeviceNotifier {
    tx: Sender<Msg>,
}

impl IMMNotificationClient_Impl for DeviceNotifier_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> WinResult<()> {
        let _ = self.tx.send(Msg::Devices);
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> WinResult<()> {
        let _ = self.tx.send(Msg::Devices);
        Ok(())
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> WinResult<()> {
        let _ = self.tx.send(Msg::Devices);
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, _flow: EDataFlow, role: ERole, _id: &PCWSTR) -> WinResult<()> {
        // The notification fires once per role; one is enough.
        if role == eConsole {
            let _ = self.tx.send(Msg::Devices);
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, key: &PROPERTYKEY) -> WinResult<()> {
        // Only a rename matters; other property churn is ignored.
        if key.fmtid == PKEY_Device_FriendlyName.fmtid && key.pid == PKEY_Device_FriendlyName.pid {
            let _ = self.tx.send(Msg::Devices);
        }
        Ok(())
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeNotifier {
    tx: Sender<Msg>,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeNotifier_Impl {
    fn OnNotify(&self, _data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> WinResult<()> {
        let _ = self.tx.send(Msg::Volume);
        Ok(())
    }
}

#[implement(IAudioSessionNotification)]
struct SessionCreatedNotifier {
    tx: Sender<Msg>,
}

impl IAudioSessionNotification_Impl for SessionCreatedNotifier_Impl {
    fn OnSessionCreated(&self, new_session: windows_core::Ref<'_, IAudioSessionControl>) -> WinResult<()> {
        if let Some(s) = new_session.as_ref() {
            let _ = self.tx.send(Msg::SessionCreated(SendCom(s.clone())));
        }
        Ok(())
    }
}

#[implement(IAudioSessionEvents)]
struct SessionEvents {
    tx: Sender<Msg>,
    key: String,
}

impl IAudioSessionEvents_Impl for SessionEvents_Impl {
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> WinResult<()> {
        Ok(())
    }
    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> WinResult<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(&self, _: f32, _: windows::core::BOOL, _: *const GUID) -> WinResult<()> {
        Ok(())
    }
    fn OnChannelVolumeChanged(&self, _: u32, _: *const f32, _: u32, _: *const GUID) -> WinResult<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> WinResult<()> {
        Ok(())
    }
    fn OnStateChanged(&self, new_state: AudioSessionState) -> WinResult<()> {
        let _ = self.tx.send(Msg::SessionState(self.key.clone(), new_state == AudioSessionStateActive));
        Ok(())
    }
    fn OnSessionDisconnected(&self, _: AudioSessionDisconnectReason) -> WinResult<()> {
        let _ = self.tx.send(Msg::SessionGone(self.key.clone()));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Worker-owned state
// ---------------------------------------------------------------------------

/// Free a `CoTaskMemAlloc`ed wide string and return it as `String`.
unsafe fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
    s
}

fn describe(dev: &IMMDevice) -> WinResult<AudioDevice> {
    unsafe {
        let id = take_pwstr(dev.GetId()?);
        let name = (|| -> WinResult<String> {
            let store = dev.OpenPropertyStore(STGM_READ)?;
            let pv = store.GetValue(&PKEY_Device_FriendlyName)?;
            Ok(take_pwstr(PropVariantToStringAlloc(&pv)?))
        })()
        .unwrap_or_else(|_| id.clone());
        Ok(AudioDevice { id, name })
    }
}

/// An endpoint we hold a volume callback on.
struct Endpoint {
    info: AudioDevice,
    volume: IAudioEndpointVolume,
    callback: IAudioEndpointVolumeCallback,
    sessions: Option<Sessions>,
}

impl Endpoint {
    fn bind(device: &IMMDevice, capture: bool, tx: &Sender<Msg>, own_pid: u32) -> WinResult<Self> {
        let info = describe(device)?;
        let volume: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None)? };
        let callback: IAudioEndpointVolumeCallback = VolumeNotifier { tx: tx.clone() }.into();
        unsafe { volume.RegisterControlChangeNotify(&callback)? };
        let sessions = if capture {
            // Missing session tracking only degrades mic_in_use.
            Sessions::bind(device, tx, own_pid).ok()
        } else {
            None
        };
        Ok(Endpoint { info, volume, callback, sessions })
    }

    fn read(&self) -> (f32, bool) {
        unsafe {
            let v = self.volume.GetMasterVolumeLevelScalar().unwrap_or(0.0);
            let m = self.volume.GetMute().map(|b| b.as_bool()).unwrap_or(false);
            (clamp01(v), m)
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        // Waits for an in-flight callback; ours never block, so no deadlock.
        unsafe {
            let _ = self.volume.UnregisterControlChangeNotify(&self.callback);
        }
    }
}

struct Tracked {
    control: IAudioSessionControl,
    events: IAudioSessionEvents,
    active: bool,
}

/// Capture-session tracking for one capture endpoint.
struct Sessions {
    manager: IAudioSessionManager2,
    notifier: IAudioSessionNotification,
    map: HashMap<String, Tracked>,
    own_pid: u32,
    tx: Sender<Msg>,
}

impl Sessions {
    fn bind(device: &IMMDevice, tx: &Sender<Msg>, own_pid: u32) -> WinResult<Self> {
        let manager: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None)? };
        let notifier: IAudioSessionNotification = SessionCreatedNotifier { tx: tx.clone() }.into();
        unsafe { manager.RegisterSessionNotification(&notifier)? };
        let mut s = Sessions { manager, notifier, map: HashMap::new(), own_pid, tx: tx.clone() };
        // Enumerating is also required for Core Audio to start delivering
        // OnSessionCreated.
        unsafe {
            let list = s.manager.GetSessionEnumerator()?;
            for i in 0..list.GetCount()? {
                if let Ok(c) = list.GetSession(i) {
                    s.track(c);
                }
            }
        }
        Ok(s)
    }

    fn track(&mut self, control: IAudioSessionControl) {
        let _ = (|| -> WinResult<()> {
            unsafe {
                let c2: IAudioSessionControl2 = windows_core::Interface::cast(&control)?;
                let system = c2.IsSystemSoundsSession().0 == 0;
                let pid = c2.GetProcessId().unwrap_or(0);
                let key = take_pwstr(c2.GetSessionInstanceIdentifier()?);
                if self.map.contains_key(&key) {
                    return Ok(());
                }
                let active = control.GetState()? == AudioSessionStateActive;
                // Own and system-sounds sessions never count; skip tracking.
                if system || pid == self.own_pid {
                    return Ok(());
                }
                let events: IAudioSessionEvents = SessionEvents { tx: self.tx.clone(), key: key.clone() }.into();
                control.RegisterAudioSessionNotification(&events)?;
                self.map.insert(key, Tracked { control, events, active });
            }
            Ok(())
        })();
    }

    fn set_active(&mut self, key: &str, active: bool) {
        if let Some(t) = self.map.get_mut(key) {
            t.active = active;
        }
    }

    fn remove(&mut self, key: &str) {
        if let Some(t) = self.map.remove(key) {
            unsafe {
                let _ = t.control.UnregisterAudioSessionNotification(&t.events);
            }
        }
    }

    fn in_use(&self) -> bool {
        // Tracked sessions were already filtered for system/own process.
        self.map.values().any(|t| counts_as_mic_use(1, 0, false, t.active))
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        unsafe {
            for (_, t) in self.map.drain() {
                let _ = t.control.UnregisterAudioSessionNotification(&t.events);
            }
            let _ = self.manager.UnregisterSessionNotification(&self.notifier);
        }
    }
}

struct Worker {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
    tx: Sender<Msg>,
    own_pid: u32,
    output: Option<Endpoint>,
    input: Option<Endpoint>,
    outputs: Vec<AudioDevice>,
    inputs: Vec<AudioDevice>,
    dedup: Dedup,
    sink: Sink,
    last_error: Arc<Mutex<Option<String>>>,
}

fn default_device(e: &IMMDeviceEnumerator, capture: bool) -> Option<IMMDevice> {
    let flow = if capture { eCapture } else { eRender };
    unsafe { e.GetDefaultAudioEndpoint(flow, eConsole).ok() }
}

fn list_devices(e: &IMMDeviceEnumerator, capture: bool) -> Vec<AudioDevice> {
    let flow = if capture { eCapture } else { eRender };
    let mut out = Vec::new();
    unsafe {
        if let Ok(coll) = e.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) {
            for i in 0..coll.GetCount().unwrap_or(0) {
                if let Ok(d) = coll.Item(i).and_then(|d| describe(&d)) {
                    out.push(d);
                }
            }
        }
    }
    out
}

impl Worker {
    fn new(sink: Sink, tx: Sender<Msg>, last_error: Arc<Mutex<Option<String>>>) -> WinResult<Self> {
        let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let client: IMMNotificationClient = DeviceNotifier { tx: tx.clone() }.into();
        unsafe { enumerator.RegisterEndpointNotificationCallback(&client)? };
        let mut w = Worker {
            enumerator,
            client,
            tx,
            own_pid: unsafe { GetCurrentProcessId() },
            output: None,
            input: None,
            outputs: Vec::new(),
            inputs: Vec::new(),
            dedup: Dedup::default(),
            sink,
            last_error,
        };
        w.refresh_devices();
        Ok(w)
    }

    fn refresh_devices(&mut self) {
        self.outputs = list_devices(&self.enumerator, false);
        self.inputs = list_devices(&self.enumerator, true);
        self.output = self.rebind(false);
        self.input = self.rebind(true);
    }

    /// Keep the current endpoint if it is still the default, else bind anew.
    fn rebind(&mut self, capture: bool) -> Option<Endpoint> {
        let dev = default_device(&self.enumerator, capture);
        let current = if capture { self.input.take() } else { self.output.take() };
        let id = dev.as_ref().and_then(|d| describe(d).ok()).map(|d| d.id);
        match (current, id) {
            (Some(c), Some(id)) if c.info.id == id => Some(c),
            (_, _) => {
                // Old endpoint (if any) was moved out above and drops here.
                dev.and_then(|d| Endpoint::bind(&d, capture, &self.tx, self.own_pid).ok())
            }
        }
    }

    fn snapshot(&self) -> AudioState {
        let (output_volume, output_muted) = self.output.as_ref().map_or((0.0, false), Endpoint::read);
        let (mic_volume, mic_muted) = self.input.as_ref().map_or((0.0, false), Endpoint::read);
        let mic_in_use = self.input.as_ref().and_then(|e| e.sessions.as_ref()).is_some_and(Sessions::in_use);
        AudioState {
            output_volume,
            output_muted,
            mic_volume,
            mic_muted,
            mic_in_use,
            output: self.output.as_ref().map(|e| e.info.clone()),
            input: self.input.as_ref().map(|e| e.info.clone()),
            outputs: self.outputs.clone(),
            inputs: self.inputs.clone(),
        }
    }

    fn emit_if_changed(&mut self) {
        let s = self.snapshot();
        if self.dedup.changed(&s) {
            (self.sink)(Event::Audio(s));
        }
    }

    fn fail(&self, what: &str, e: impl fmt::Display) {
        let msg = format!("{what}: {e}");
        if let Ok(mut g) = self.last_error.lock() {
            *g = Some(msg);
        }
    }

    fn command(&mut self, cmd: Cmd) {
        unsafe {
            match cmd {
                Cmd::OutputVolume(v) => match &self.output {
                    Some(e) => {
                        if let Err(x) = e.volume.SetMasterVolumeLevelScalar(clamp01(v), std::ptr::null()) {
                            self.fail("set output volume", x);
                        }
                    }
                    None => self.fail("set output volume", "no output device"),
                },
                Cmd::OutputMuted(m) => match &self.output {
                    Some(e) => {
                        if let Err(x) = e.volume.SetMute(m, std::ptr::null()) {
                            self.fail("set output mute", x);
                        }
                    }
                    None => self.fail("set output mute", "no output device"),
                },
                Cmd::MicVolume(v) => match &self.input {
                    Some(e) => {
                        if let Err(x) = e.volume.SetMasterVolumeLevelScalar(clamp01(v), std::ptr::null()) {
                            self.fail("set mic volume", x);
                        }
                    }
                    None => self.fail("set mic volume", "no input device"),
                },
                Cmd::MicMuted(m) => match &self.input {
                    Some(e) => {
                        if let Err(x) = e.volume.SetMute(m, std::ptr::null()) {
                            self.fail("set mic mute", x);
                        }
                    }
                    None => self.fail("set mic mute", "no input device"),
                },
                Cmd::DefaultOutput(id) => {
                    if let Err(x) = policy::set_default(&id) {
                        self.fail("set default output", x);
                    }
                }
                Cmd::DefaultInput(id) => {
                    if let Err(x) = policy::set_default(&id) {
                        self.fail("set default input", x);
                    }
                }
            }
        }
    }

    /// Handle one message. Returns `false` to stop.
    fn handle(&mut self, m: Msg, devices_dirty: &mut bool) -> bool {
        match m {
            Msg::Devices => *devices_dirty = true,
            Msg::Volume => {}
            Msg::SessionCreated(SendCom(c)) => {
                if let Some(s) = self.input.as_mut().and_then(|e| e.sessions.as_mut()) {
                    s.track(c);
                }
            }
            Msg::SessionState(k, a) => {
                if let Some(s) = self.input.as_mut().and_then(|e| e.sessions.as_mut()) {
                    s.set_active(&k, a);
                }
            }
            Msg::SessionGone(k) => {
                if let Some(s) = self.input.as_mut().and_then(|e| e.sessions.as_mut()) {
                    s.remove(&k);
                }
            }
            Msg::Cmd(c) => self.command(c),
            Msg::Stop => return false,
        }
        true
    }

    fn run(&mut self, rx: &Receiver<Msg>) {
        self.emit_if_changed();
        // Blocks in the kernel until a callback or command arrives.
        while let Ok(first) = rx.recv() {
            let mut dirty = false;
            let mut go = self.handle(first, &mut dirty);
            // Coalesce bursts (slider drags, triple default-changed).
            while go {
                match rx.try_recv() {
                    Ok(m) => go = self.handle(m, &mut dirty),
                    Err(_) => break,
                }
            }
            if !go {
                break;
            }
            if dirty {
                self.refresh_devices();
            }
            self.emit_if_changed();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        unsafe {
            let _ = self.enumerator.UnregisterEndpointNotificationCallback(&self.client);
        }
        self.output = None;
        self.input = None;
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Running audio service. Dropping it stops the worker and releases all COM
/// objects. Do not drop it from inside the sink callback (that is detected and
/// the join is skipped, leaving the worker to exit on its own).
pub struct Handle {
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
    worker_id: ThreadId,
    last_error: Arc<Mutex<Option<String>>>,
}

/// Start the service. Blocks briefly while the worker initialises COM and
/// takes the first snapshot, then emits [`Event::Audio`] once and afterwards
/// only on change.
pub fn start(sink: Sink) -> Result<Handle, Error> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), Error>>(1);
    let last_error = Arc::new(Mutex::new(None));
    let le = last_error.clone();
    let wtx = tx.clone();
    let thread = thread::Builder::new()
        .name("nb-audio".into())
        .spawn(move || {
            // MTA so callbacks run on their own threads and need no pump.
            if let Err(e) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
                let _ = ready_tx.send(Err(e.into()));
                return;
            }
            match Worker::new(sink, wtx, le) {
                Ok(mut w) => {
                    let _ = ready_tx.send(Ok(()));
                    w.run(&rx);
                    drop(w);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.into()));
                }
            }
            unsafe { CoUninitialize() };
        })
        .map_err(|e| Error::Unsupported(format!("cannot spawn thread: {e}")))?;
    let worker_id = thread.thread().id();
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Handle { tx, thread: Some(thread), worker_id, last_error }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => Err(Error::Stopped),
    }
}

impl Handle {
    fn send(&self, c: Cmd) -> Result<(), Error> {
        self.tx.send(Msg::Cmd(c)).map_err(|_| Error::Stopped)
    }

    /// Set master output volume (0.0 to 1.0). Asynchronous.
    pub fn set_output_volume(&self, v: f32) -> Result<(), Error> {
        self.send(Cmd::OutputVolume(v))
    }
    pub fn set_output_muted(&self, m: bool) -> Result<(), Error> {
        self.send(Cmd::OutputMuted(m))
    }
    pub fn set_mic_volume(&self, v: f32) -> Result<(), Error> {
        self.send(Cmd::MicVolume(v))
    }
    pub fn set_mic_muted(&self, m: bool) -> Result<(), Error> {
        self.send(Cmd::MicMuted(m))
    }

    /// Make `device_id` (an [`AudioDevice::id`]) the default render device
    /// for all roles. Uses the undocumented `IPolicyConfig` interface, which
    /// may change or fail on any Windows update. Queued: `Ok` only means it was
    /// queued; a failure appears in [`Handle::last_error`] and the state
    /// simply does not change.
    pub fn set_default_output(&self, device_id: &str) -> Result<(), Error> {
        self.send(Cmd::DefaultOutput(device_id.to_owned()))
    }

    /// Same as [`Handle::set_default_output`] for capture devices.
    pub fn set_default_input(&self, device_id: &str) -> Result<(), Error> {
        self.send(Cmd::DefaultInput(device_id.to_owned()))
    }

    /// Take the most recent asynchronous command failure, if any.
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|mut g| g.take())
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            if thread::current().id() != self.worker_id {
                let _ = t.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> AudioState {
        AudioState {
            output_volume: 0.5,
            output_muted: false,
            mic_volume: 0.7,
            mic_muted: false,
            mic_in_use: false,
            output: Some(AudioDevice { id: "a".into(), name: "A".into() }),
            input: None,
            outputs: vec![AudioDevice { id: "a".into(), name: "A".into() }],
            inputs: vec![],
        }
    }

    #[test]
    fn clamp() {
        assert_eq!(clamp01(-1.0), 0.0);
        assert_eq!(clamp01(2.0), 1.0);
        assert_eq!(clamp01(f32::NAN), 0.0);
        assert_eq!(clamp01(0.25), 0.25);
    }

    #[test]
    fn mic_use_rules() {
        assert!(counts_as_mic_use(10, 99, false, true));
        assert!(!counts_as_mic_use(99, 99, false, true));
        assert!(!counts_as_mic_use(10, 99, true, true));
        assert!(!counts_as_mic_use(10, 99, false, false));
    }

    #[test]
    fn dedup_emits_only_on_change() {
        let mut d = Dedup::default();
        let mut s = st();
        assert!(d.changed(&s));
        assert!(!d.changed(&s));
        s.mic_in_use = true;
        assert!(d.changed(&s));
        assert!(!d.changed(&s));
        s.inputs.push(AudioDevice { id: "m".into(), name: "M".into() });
        assert!(d.changed(&s));
    }
}
