//! On-demand weather from Open-Meteo over WinHTTP. Never runs by itself.

use nb_core::{Event, Sink, Weather};
use serde_json::Value;
use windows::Win32::Networking::WinHttp::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::Error;

/// French label for a WMO weather interpretation code.
pub fn wmo_label_fr(code: i64) -> &'static str {
    match code {
        0 => "Ciel dégagé",
        1 => "Plutôt dégagé",
        2 => "Partiellement nuageux",
        3 => "Couvert",
        45 | 48 => "Brouillard",
        51 | 53 | 55 => "Bruine",
        56 | 57 => "Bruine verglaçante",
        61 => "Pluie faible",
        63 => "Pluie",
        65 => "Pluie forte",
        66 | 67 => "Pluie verglaçante",
        71 => "Neige faible",
        73 => "Neige",
        75 => "Neige forte",
        77 => "Grains de neige",
        80 => "Averses faibles",
        81 => "Averses",
        82 => "Averses violentes",
        85 | 86 => "Averses de neige",
        95 => "Orage",
        96 | 99 => "Orage et grêle",
        _ => "Inconnu",
    }
}

/// Fetches the weather on a short-lived thread and delivers `Event::Weather`
/// through `sink` on success. Returns immediately; errors go to `on_error`
/// when given. The caller decides when to call this: there is no timer.
pub fn fetch_weather(sink: Sink, lat: f64, lon: f64, on_error: Option<Box<dyn FnOnce(Error) + Send>>) {
    let r = std::thread::Builder::new().name("nb-integrations-weather".into()).spawn(move || {
        match fetch_weather_blocking(lat, lon) {
            Ok(w) => sink(Event::Weather(w)),
            Err(e) => {
                if let Some(f) = on_error {
                    f(e);
                }
            }
        }
    });
    let _ = r;
}

/// Blocking variant. Do not call from the UI thread.
pub fn fetch_weather_blocking(lat: f64, lon: f64) -> Result<Weather, Error> {
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(Error::Network("latitude/longitude out of range".into()));
    }
    let path = format!(
        "/v1/forecast?latitude={lat:.4}&longitude={lon:.4}&current=temperature_2m,weather_code&daily=temperature_2m_max,temperature_2m_min&timezone=auto&forecast_days=1"
    );
    let body = https_get("api.open-meteo.com", &path)?;
    parse_weather(&body)
}

/// Looks up a place by name. Blocking: do not call from the UI thread.
/// Returns latitude, longitude and the display name.
pub fn geocode_blocking(name: &str) -> Result<(f64, f64, String), Error> {
    let encoded = percent_encode(name.trim());
    if encoded.is_empty() || encoded.len() > 300 {
        return Err(Error::Network("invalid place name".into()));
    }
    let path = format!("/v1/search?name={encoded}&count=1&language=fr&format=json");
    parse_geocode(&https_get("geocoding-api.open-meteo.com", &path)?)
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub(crate) fn parse_geocode(body: &[u8]) -> Result<(f64, f64, String), Error> {
    let v: Value = serde_json::from_slice(body).map_err(|e| Error::Json(e.to_string()))?;
    let first = &v["results"][0];
    match (first["latitude"].as_f64(), first["longitude"].as_f64(), first["name"].as_str()) {
        (Some(lat), Some(lon), Some(name)) => Ok((lat, lon, name.to_string())),
        _ => Err(Error::Network("place not found".into())),
    }
}

pub(crate) fn parse_weather(body: &[u8]) -> Result<Weather, Error> {
    let v: Value = serde_json::from_slice(body).map_err(|e| Error::Json(e.to_string()))?;
    let num = |p: &Value| p.as_f64().map(|f| f as f32);
    let temp = num(&v["current"]["temperature_2m"]);
    let code = v["current"]["weather_code"].as_i64();
    let min = num(&v["daily"]["temperature_2m_min"][0]);
    let max = num(&v["daily"]["temperature_2m_max"][0]);
    match (temp, code, min, max) {
        (Some(t), Some(c), Some(mn), Some(mx)) => {
            Ok(Weather { temperature_c: t, min_c: mn, max_c: mx, condition: wmo_label_fr(c).to_string() })
        }
        _ => Err(Error::Json("unexpected Open-Meteo response".into())),
    }
}

struct Internet(*mut core::ffi::c_void);
impl Drop for Internet {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn https_get(host: &str, path: &str) -> Result<Vec<u8>, Error> {
    https_get_max(host, path, 64 * 1024)
}

/// GET over HTTPS, following redirects, refusing any status but 200 and any
/// body larger than `max`.
pub(crate) fn https_get_max(host: &str, path: &str, max: usize) -> Result<Vec<u8>, Error> {
    unsafe {
        let session = Internet(WinHttpOpen(
            w!("naelbar"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(Error::Os(windows::core::Error::from_thread()));
        }
        WinHttpSetTimeouts(session.0, 5000, 5000, 5000, 8000)?;
        let host_w = HSTRING::from(host);
        let conn = Internet(WinHttpConnect(session.0, &host_w, 443, 0));
        if conn.0.is_null() {
            return Err(Error::Os(windows::core::Error::from_thread()));
        }
        let path_w = HSTRING::from(path);
        let req = Internet(WinHttpOpenRequest(
            conn.0,
            w!("GET"),
            &path_w,
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            return Err(Error::Os(windows::core::Error::from_thread()));
        }
        WinHttpSendRequest(req.0, None, None, 0, 0, 0).map_err(|e| Error::Network(e.to_string()))?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut()).map_err(|e| Error::Network(e.to_string()))?;
        let mut status = 0u32;
        let mut len = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(req.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some((&mut status as *mut u32).cast()), &mut len, std::ptr::null_mut())
            .map_err(|e| Error::Network(e.to_string()))?;
        if status != 200 {
            return Err(Error::Network(format!("HTTP {status}")));
        }
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let mut n = 0u32;
            WinHttpReadData(req.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut n)
                .map_err(|e| Error::Network(e.to_string()))?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
            if out.len() > max {
                return Err(Error::Network("response too large".into()));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(wmo_label_fr(0), "Ciel dégagé");
        assert_eq!(wmo_label_fr(61), "Pluie faible");
        assert_eq!(wmo_label_fr(95), "Orage");
        assert_eq!(wmo_label_fr(1234), "Inconnu");
    }

    #[test]
    fn parses_response() {
        let j = br#"{"current":{"temperature_2m":12.3,"weather_code":3},"daily":{"temperature_2m_max":[15.0],"temperature_2m_min":[7.5]}}"#;
        let w = parse_weather(j).unwrap();
        assert_eq!((w.temperature_c, w.min_c, w.max_c), (12.3, 7.5, 15.0));
        assert_eq!(w.condition, "Couvert");
        assert!(parse_weather(b"{}").is_err());
    }

    #[test]
    fn rejects_bad_coordinates() {
        assert!(fetch_weather_blocking(91.0, 0.0).is_err());
        assert_eq!(percent_encode("Saint-Étienne du Bois"), "Saint-%C3%89tienne%20du%20Bois");
        let found = parse_geocode(br#"{"results":[{"name":"Blois","latitude":47.59,"longitude":1.33}]}"#).unwrap();
        assert_eq!(found.2, "Blois");
        assert!(parse_geocode(br#"{"generationtime_ms":0.2}"#).is_err());
    }
}
