//! Battery levels of Bluetooth peripherals, as Windows Settings shows them.
//!
//! Windows stores the level in the `DEVPKEY_Bluetooth_Battery` property of a
//! device node (Bluetooth LE devices that expose the GATT battery service, and
//! classic devices through the hands-free profile). Nothing here runs by
//! itself: [`refresh`] reads once on a short-lived thread and emits
//! [`Event::Devices`]. The shell decides when (widget visible, at most once a
//! minute, on `WM_DEVICECHANGE`, after a resume, when the island opens, and,
//! for the HUD alerts, once every 10 min only while a device with a battery is
//! connected, since levels change without any notification).
//!
//! Peripherals behind a vendor dongle (SteelSeries 2.4 GHz receivers, for
//! instance) do not publish their level to Windows; only the vendor software
//! knows it, so they do not appear.

#![allow(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;

use nb_core::{DeviceBattery, DeviceKind, Event, Sink};
use windows::Devices::Bluetooth::{BluetoothConnectionStatus, BluetoothDevice, BluetoothLEDevice};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_ALLCLASSES, DIGCF_PRESENT, HDEVINFO, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiGetClassDevsW, SetupDiGetDevicePropertyW,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_ContainerId, DEVPKEY_Device_FriendlyName, DEVPKEY_DeviceContainer_Category, DEVPKEY_NAME, DEVPROPTYPE,
};
use windows::Win32::Foundation::DEVPROPKEY;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{GUID, PCWSTR};

/// `DEVPKEY_Bluetooth_Battery`: a byte, 0 to 100.
const DEVPKEY_BLUETOOTH_BATTERY: DEVPROPKEY = DEVPROPKEY { fmtid: GUID::from_u128(0x104ea319_6ee2_4701_bd47_8ddbf425bbe5), pid: 2 };
/// `DEVPKEY_Bluetooth_ClassOfDevice`: a 32-bit Bluetooth class of device.
const DEVPKEY_BLUETOOTH_COD: DEVPROPKEY = DEVPROPKEY { fmtid: GUID::from_u128(0x2bd67d8b_8beb_48d5_87e0_6cda3428040a), pid: 10 };
/// `DEVPKEY_Bluetooth_DeviceAddress`: twelve hex digits.
const DEVPKEY_BLUETOOTH_ADDRESS: DEVPROPKEY = DEVPROPKEY { fmtid: GUID::from_u128(0x2bd67d8b_8beb_48d5_87e0_6cda3428040a), pid: 1 };

/// Reads the levels on a short-lived thread and emits [`Event::Devices`].
pub fn refresh(sink: Sink) {
    let _ = std::thread::Builder::new().name("nb-devices".into()).spawn(move || sink(Event::Devices(read_batteries())));
}

struct Node {
    name: String,
    category: String,
    cod: Option<u32>,
    address: Option<u64>,
    container: Option<GUID>,
    percent: u8,
}

/// Every connected peripheral that reports a level. Blocking: a few ms of
/// SetupAPI work, plus one WinRT lookup per Bluetooth device.
pub fn read_batteries() -> Vec<DeviceBattery> {
    let nodes = unsafe { battery_nodes() };
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };
    let nodes: Vec<Node> = nodes.into_iter().filter(|n| n.address.is_none_or(is_connected)).collect();
    if com {
        unsafe { CoUninitialize() };
    }
    merge(nodes)
}

/// One entry per physical device: a headset exposes several device nodes
/// (hands-free, audio sink...) that share a container id.
fn merge(nodes: Vec<Node>) -> Vec<DeviceBattery> {
    let mut by_container: HashMap<GUID, usize> = HashMap::new();
    let mut out: Vec<DeviceBattery> = Vec::new();
    for n in nodes {
        let kind = classify(&n.category, n.cod, &n.name);
        let entry = DeviceBattery { name: clean_name(&n.name), kind, percent: n.percent.min(100) };
        match n.container.and_then(|c| by_container.get(&c).copied()) {
            Some(i) => {
                if out[i].kind == DeviceKind::Other {
                    out[i].kind = kind;
                }
            }
            None => {
                if let Some(c) = n.container {
                    by_container.insert(c, out.len());
                }
                out.push(entry);
            }
        }
    }
    out.sort_by(|a, b| (a.kind as u8, &a.name).cmp(&(b.kind as u8, &b.name)));
    out
}

/// A paired device keeps its last level after it is switched off, so only
/// devices Windows sees as connected count. When the lookup fails, the device
/// is kept.
fn is_connected(address: u64) -> bool {
    if let Ok(d) = BluetoothLEDevice::FromBluetoothAddressAsync(address).and_then(|op| op.join()) {
        if let Ok(s) = d.ConnectionStatus() {
            return s == BluetoothConnectionStatus::Connected;
        }
    }
    match BluetoothDevice::FromBluetoothAddressAsync(address).and_then(|op| op.join()) {
        Ok(d) => d.ConnectionStatus().map_or(true, |s| s == BluetoothConnectionStatus::Connected),
        Err(_) => true,
    }
}

struct DevInfo(HDEVINFO);

impl Drop for DevInfo {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

unsafe fn battery_nodes() -> Vec<Node> {
    let mut out = Vec::new();
    let Ok(set) = SetupDiGetClassDevsW(None, PCWSTR::null(), None, DIGCF_ALLCLASSES | DIGCF_PRESENT) else { return out };
    let set = DevInfo(set);
    let mut i = 0;
    loop {
        let mut data = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        if SetupDiEnumDeviceInfo(set.0, i, &mut data).is_err() {
            break;
        }
        i += 1;
        let Some(level) = property(set.0, &data, &DEVPKEY_BLUETOOTH_BATTERY) else { continue };
        let Some(&percent) = level.first() else { continue };
        let name = property(set.0, &data, &DEVPKEY_Device_FriendlyName).or_else(|| property(set.0, &data, &DEVPKEY_NAME)).map(|b| utf16(&b)).unwrap_or_default();
        out.push(Node {
            name,
            category: property(set.0, &data, &DEVPKEY_DeviceContainer_Category).map(|b| utf16(&b)).unwrap_or_default(),
            cod: property(set.0, &data, &DEVPKEY_BLUETOOTH_COD).and_then(|b| Some(u32::from_le_bytes(b.get(..4)?.try_into().ok()?))),
            address: property(set.0, &data, &DEVPKEY_BLUETOOTH_ADDRESS).and_then(|b| parse_address(&utf16(&b))),
            container: property(set.0, &data, &DEVPKEY_Device_ContainerId).and_then(|b| guid_from(&b)),
            percent,
        });
    }
    out
}

/// A GUID as stored in memory.
fn guid_from(b: &[u8]) -> Option<GUID> {
    let b: [u8; 16] = b.get(..16)?.try_into().ok()?;
    let d1 = u32::from_le_bytes(b[0..4].try_into().ok()?);
    let d2 = u16::from_le_bytes(b[4..6].try_into().ok()?);
    let d3 = u16::from_le_bytes(b[6..8].try_into().ok()?);
    let d4 = u64::from_be_bytes(b[8..16].try_into().ok()?);
    Some(GUID::from_u128(((d1 as u128) << 96) | ((d2 as u128) << 80) | ((d3 as u128) << 64) | d4 as u128))
}

unsafe fn property(set: HDEVINFO, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<Vec<u8>> {
    let mut ty = DEVPROPTYPE::default();
    let mut size = 0u32;
    let _ = SetupDiGetDevicePropertyW(set, data, key, &mut ty, None, Some(&mut size), 0);
    if size == 0 || size > 4096 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    SetupDiGetDevicePropertyW(set, data, key, &mut ty, Some(&mut buf), None, 0).ok()?;
    Some(buf)
}

/// NUL-terminated UTF-16 to a string. A string list keeps its first entry.
fn utf16(b: &[u8]) -> String {
    let w: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
    String::from_utf16_lossy(&w)
}

/// `3498f54b9a28` or `34:98:f5:4b:9a:28`.
pub fn parse_address(s: &str) -> Option<u64> {
    let hex: String = s.chars().filter(|c| *c != ':').collect();
    if hex.len() != 12 {
        return None;
    }
    u64::from_str_radix(&hex, 16).ok()
}

/// Drops the profile suffix Windows appends to some device nodes.
pub fn clean_name(name: &str) -> String {
    const SUFFIXES: [&str; 6] = [" Hands-Free AG", " Hands-Free", " Avrcp Transport", " Stereo", " LE", " Mains libres"];
    let mut n = name.trim();
    for s in SUFFIXES {
        if let Some(rest) = n.strip_suffix(s) {
            n = rest.trim_end();
        }
    }
    n.to_string()
}

/// Container category first (`Input.Mouse`, `Audio.Headphone`...), then the
/// Bluetooth class of device, then words in the name.
pub fn classify(category: &str, cod: Option<u32>, name: &str) -> DeviceKind {
    let c = category.to_ascii_lowercase();
    if c.contains("mouse") || c.contains("pointing") {
        return DeviceKind::Mouse;
    }
    if c.contains("keyboard") {
        return DeviceKind::Keyboard;
    }
    if c.contains("gaming") || c.contains("gamepad") || c.contains("joystick") {
        return DeviceKind::Controller;
    }
    if c.contains("head") || c.contains("audio") || c.contains("earbud") {
        return DeviceKind::Headset;
    }
    if let Some(cod) = cod {
        let major = (cod >> 8) & 0x1F;
        let minor = (cod >> 2) & 0x3F;
        match major {
            4 => return DeviceKind::Headset,
            5 => match (minor >> 4) & 0x3 {
                1 => return DeviceKind::Keyboard,
                2 | 3 => return DeviceKind::Mouse,
                _ if matches!(minor & 0xF, 1 | 2) => return DeviceKind::Controller,
                _ => {}
            },
            _ => {}
        }
    }
    let n = name.to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| n.contains(w));
    if any(&["mouse", "souris", "aerox", "rival", "prime", "mx master", "mx anywhere", "deathadder", "viper", "g pro", "pulsefire"]) {
        DeviceKind::Mouse
    } else if any(&["keyboard", "clavier", "keys", "apex"]) {
        DeviceKind::Keyboard
    } else if any(&["controller", "manette", "gamepad", "dualsense", "dualshock", "xbox"]) {
        DeviceKind::Controller
    } else if any(&["headset", "casque", "headphone", "buds", "airpods", "arctis", "hands-free", "soundcore", "wh-", "wf-", "jabra", "bose"]) {
        DeviceKind::Headset
    } else {
        DeviceKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_suffixes_are_dropped() {
        assert_eq!(clean_name("soundcore P31i Hands-Free AG"), "soundcore P31i");
        assert_eq!(clean_name("Aerox 5 WL"), "Aerox 5 WL");
        assert_eq!(clean_name("  WH-1000XM4 Stereo "), "WH-1000XM4");
    }

    #[test]
    fn kinds_come_from_category_then_class_then_name() {
        assert_eq!(classify("Input.Mouse", None, "Whatever"), DeviceKind::Mouse);
        assert_eq!(classify("Input.Keyboard", None, ""), DeviceKind::Keyboard);
        // soundcore P31i: audio/video, wearable headset.
        assert_eq!(classify("", Some(2360324), "soundcore P31i"), DeviceKind::Headset);
        // Peripheral, pointing device.
        assert_eq!(classify("", Some(0x000580), "x"), DeviceKind::Mouse);
        assert_eq!(classify("", Some(0x000540), "x"), DeviceKind::Keyboard);
        assert_eq!(classify("", Some(0x000508), "x"), DeviceKind::Controller);
        assert_eq!(classify("", None, "SteelSeries Arctis Nova 7"), DeviceKind::Headset);
        assert_eq!(classify("", None, "Aerox 5 WL"), DeviceKind::Mouse);
        assert_eq!(classify("", None, "Xbox Wireless Controller"), DeviceKind::Controller);
        assert_eq!(classify("", None, "Thermometer"), DeviceKind::Other);
    }

    #[test]
    fn addresses_parse_with_or_without_colons() {
        assert_eq!(parse_address("3498f54b9a28"), Some(0x3498f54b9a28));
        assert_eq!(parse_address("34:98:f5:4b:9a:28"), Some(0x3498f54b9a28));
        assert_eq!(parse_address("3498"), None);
        assert_eq!(parse_address("zz98f54b9a28"), None);
    }

    #[test]
    fn nodes_of_one_device_are_merged() {
        let g = GUID::from_u128(7);
        let node = |name: &str, cod, percent| Node { name: name.into(), category: String::new(), cod, address: None, container: Some(g), percent };
        let out = merge(vec![node("soundcore P31i Hands-Free AG", Some(2360324), 80), node("soundcore P31i", None, 80)]);
        assert_eq!(out, vec![DeviceBattery { name: "soundcore P31i".into(), kind: DeviceKind::Headset, percent: 80 }]);
    }

    #[test]
    fn guids_read_from_memory_order() {
        let g = GUID::from_u128(0x22e787f3_ebce_54a6_87ae_66e51fcb0672);
        let raw: [u8; 16] = unsafe { std::mem::transmute(g) };
        assert_eq!(guid_from(&raw), Some(g));
    }

    /// Lists what this PC exposes. `cargo test -p nb-system devices_on_this_pc -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn devices_on_this_pc() {
        for n in unsafe { battery_nodes() } {
            println!("reported: {} {} % (connected: {:?})", n.name, n.percent, n.address.map(is_connected));
        }
        for d in read_batteries() {
            println!("{:?} {} {} %", d.kind, d.name, d.percent);
        }
    }
}
