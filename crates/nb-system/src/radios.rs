//! Wi-Fi and Bluetooth radios through Windows.Devices.Radios.

use crate::Shared;
use nb_core::{Event, RadioState as CoreRadios};
use std::sync::{Arc, Mutex, Weak};
use windows::Devices::Radios::{Radio, RadioKind, RadioState};
use windows::Foundation::TypedEventHandler;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::{IInspectable, Ref};

#[derive(Default)]
pub struct Hub {
    radios: Mutex<Vec<(Radio, i64)>>,
    last: Mutex<Option<(Option<bool>, Option<bool>)>>,
}

/// Pure aggregation: a kind is "on" when any radio of that kind is on.
pub fn aggregate(radios: &[(RadioKind, bool)]) -> (Option<bool>, Option<bool>) {
    let mut wifi = None;
    let mut bt = None;
    for (k, on) in radios {
        let slot = match *k {
            RadioKind::WiFi => &mut wifi,
            RadioKind::Bluetooth => &mut bt,
            _ => continue,
        };
        *slot = Some(slot.unwrap_or(false) || *on);
    }
    (wifi, bt)
}

fn emit_if_changed(shared: &Shared) {
    let snapshot: Vec<(RadioKind, bool)> = {
        let r = shared.radios.radios.lock().unwrap();
        r.iter()
            .filter_map(|(r, _)| Some((r.Kind().ok()?, r.State().ok()? == RadioState::On)))
            .collect()
    };
    let now = aggregate(&snapshot);
    let mut last = shared.radios.last.lock().unwrap();
    if *last != Some(now) {
        *last = Some(now);
        drop(last);
        (shared.sink)(Event::Radios(CoreRadios { wifi: now.0, bluetooth: now.1 }));
    }
}

/// Enumerates radios and subscribes to StateChanged. Blocking; run it on a
/// transient worker thread. Emits the initial state.
pub fn init(shared: &Arc<Shared>) {
    // SAFETY: plain COM init for this worker thread.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let list = Radio::GetRadiosAsync().and_then(|op| op.join());
    let mut subscribed = Vec::new();
    if let Ok(list) = list {
        for radio in list {
            let weak: Weak<Shared> = Arc::downgrade(shared);
            let handler = TypedEventHandler::<Radio, IInspectable>::new(
                move |_s: Ref<Radio>, _a: Ref<IInspectable>| {
                    if let Some(sh) = weak.upgrade() {
                        emit_if_changed(&sh);
                    }
                    Ok(())
                },
            );
            if let Ok(token) = radio.StateChanged(&handler) {
                subscribed.push((radio, token));
            }
        }
    }
    *shared.radios.radios.lock().unwrap() = subscribed;
    emit_if_changed(shared);
}

/// Removes the event handlers.
pub fn shutdown(shared: &Shared) {
    for (radio, token) in shared.radios.radios.lock().unwrap().drain(..) {
        let _ = radio.RemoveStateChanged(token);
    }
}

/// Blocking; run on a transient worker thread.
pub fn set(shared: &Arc<Shared>, kind: RadioKind, on: bool) {
    // SAFETY: plain COM init for this worker thread.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let targets: Vec<Radio> = shared
        .radios
        .radios
        .lock()
        .unwrap()
        .iter()
        .filter(|(r, _)| r.Kind().ok() == Some(kind))
        .map(|(r, _)| r.clone())
        .collect();
    if targets.is_empty() {
        return;
    }
    let _ = Radio::RequestAccessAsync().and_then(|op| op.join());
    let state = if on { RadioState::On } else { RadioState::Off };
    for r in targets {
        let _ = r.SetStateAsync(state).and_then(|op| op.join());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_kinds() {
        assert_eq!(aggregate(&[]), (None, None));
        let v = [(RadioKind::WiFi, false), (RadioKind::Bluetooth, true), (RadioKind::Other, true)];
        assert_eq!(aggregate(&v), (Some(false), Some(true)));
        let v = [(RadioKind::WiFi, false), (RadioKind::WiFi, true)];
        assert_eq!(aggregate(&v), (Some(true), None));
    }
}
