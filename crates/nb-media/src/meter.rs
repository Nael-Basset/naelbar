//! Peak levels of the default output device, for the pill visualizer.
//!
//! `IAudioMeterInformation` only reads values the audio engine already keeps:
//! no capture stream and no thread. The caller samples it while the result is
//! on screen and drops the meter when it is not.

use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};

/// Most channels read per sample. Extra channels are ignored.
pub const MAX_CHANNELS: usize = 8;

pub struct PeakMeter {
    meter: IAudioMeterInformation,
    channels: u32,
}

impl PeakMeter {
    /// Opens the meter of the default render endpoint. The calling thread
    /// must have initialised COM.
    pub fn open() -> windows::core::Result<Self> {
        // SAFETY: plain COM activation on a thread that initialised COM.
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let meter: IAudioMeterInformation = device.Activate(CLSCTX_ALL, None)?;
            let channels = meter.GetMeteringChannelCount()?.min(MAX_CHANNELS as u32);
            Ok(Self { meter, channels })
        }
    }

    /// Overall peak and per-channel peaks, 0.0 to 1.0. Fails once the device
    /// is gone; the caller then opens a new meter.
    pub fn read(&self, channels: &mut [f32; MAX_CHANNELS]) -> windows::core::Result<(f32, usize)> {
        // SAFETY: the slice holds at least `self.channels` values.
        unsafe {
            let peak = self.meter.GetPeakValue()?;
            let n = self.channels as usize;
            if n > 0 {
                self.meter.GetChannelsPeakValues(&mut channels[..n])?;
            }
            Ok((peak, n))
        }
    }
}
