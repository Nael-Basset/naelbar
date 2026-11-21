//! Image conversion: clipboard DIB to PNG (WIC) and back. Pure byte helpers
//! are separated from the WIC calls so they can be tested directly.

use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::*;
use windows::Win32::System::Com::*;
use windows::core::Result as WinResult;

/// Upper bound for a raw clipboard image payload.
pub const MAX_DIB_BYTES: usize = 256 * 1024 * 1024;

const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Wraps a packed DIB (as found on the clipboard) in a BMP file header so a
/// WIC decoder can read it. `None` for unsupported or malformed input.
pub fn bmp_file_from_dib(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let hdr = u32_at(dib, 0) as usize;
    if hdr < 40 || hdr > dib.len() {
        return None;
    }
    let bit_count = u16_at(dib, 14) as u32;
    let compression = u32_at(dib, 16);
    if compression == 4 || compression == 5 {
        return None; // embedded JPEG/PNG
    }
    let clr_used = u32_at(dib, 32);
    let masks = match (hdr, compression) {
        (40, BI_BITFIELDS) => 12,
        (40, BI_ALPHABITFIELDS) => 16,
        _ => 0,
    };
    let colors = if clr_used > 0 {
        clr_used.min(256) as usize
    } else if bit_count <= 8 {
        1usize << bit_count
    } else {
        0
    };
    let off = hdr + masks + colors * 4;
    if off > dib.len() {
        return None;
    }
    let total = 14 + dib.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&((14 + off) as u32).to_le_bytes());
    out.extend_from_slice(dib);
    Some(out)
}

/// Width and height from a PNG's IHDR, `None` if it is not a PNG.
pub fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if png.len() < 24 || png[..8] != SIG || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// Builds a CF_HDROP payload (DROPFILES + double-nul-terminated wide paths).
pub fn build_hdrop(paths: &[std::path::PathBuf]) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let mut out = Vec::new();
    out.extend_from_slice(&20u32.to_le_bytes()); // pFiles
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    out.extend_from_slice(&0u32.to_le_bytes()); // fNC
    out.extend_from_slice(&1u32.to_le_bytes()); // fWide
    for p in paths {
        for u in p.as_os_str().encode_wide() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Initialises COM on the current thread for its lifetime.
pub struct ComGuard(bool);
impl ComGuard {
    pub fn new() -> Self {
        // S_FALSE (already initialised) also needs a matching uninitialise.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        ComGuard(hr.is_ok())
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn factory() -> WinResult<IWICImagingFactory> {
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
}

/// Decodes any WIC-supported image from memory into a 24bpp BGR source.
fn decode_bgr24(
    f: &IWICImagingFactory,
    data: &[u8],
) -> WinResult<(IWICFormatConverter, u32, u32)> {
    unsafe {
        let stream = f.CreateStream()?;
        stream.InitializeFromMemory(data)?;
        let dec = f.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let frame = dec.GetFrame(0)?;
        let conv = f.CreateFormatConverter()?;
        conv.Initialize(
            &frame,
            &GUID_WICPixelFormat24bppBGR,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let (mut w, mut h) = (0, 0);
        conv.GetSize(&mut w, &mut h)?;
        Ok((conv, w, h))
    }
}

/// Encodes a clipboard DIB to an opaque PNG. Returns (png, width, height).
pub fn dib_to_png(dib: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let bmp = bmp_file_from_dib(dib)?;
    dib_to_png_inner(&bmp).ok()
}

fn dib_to_png_inner(bmp: &[u8]) -> WinResult<(Vec<u8>, u32, u32)> {
    unsafe {
        let f = factory()?;
        let (conv, w, h) = decode_bgr24(&f, bmp)?;
        let mem = CreateStreamOnHGlobal(Default::default(), true)?;
        let enc = f.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        enc.Initialize(&mem, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        enc.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.ok_or_else(windows::core::Error::empty)?;
        frame.Initialize(None)?;
        frame.SetSize(w, h)?;
        let mut fmt = GUID_WICPixelFormat24bppBGR;
        frame.SetPixelFormat(&mut fmt)?;
        frame.WriteSource(&conv, std::ptr::null())?;
        frame.Commit()?;
        enc.Commit()?;

        let mut stat = STATSTG::default();
        mem.Stat(&mut stat, STATFLAG_NONAME)?;
        let len = stat.cbSize as usize;
        let mut buf = vec![0u8; len];
        mem.Seek(0, STREAM_SEEK_SET, None)?;
        let mut read = 0u32;
        mem.Read(buf.as_mut_ptr().cast(), len as u32, Some(&mut read)).ok()?;
        buf.truncate(read as usize);
        Ok((buf, w, h))
    }
}

/// Decodes a PNG into a bottom-up 24bpp packed DIB (BITMAPINFOHEADER + pixels)
/// suitable for CF_DIB.
pub fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
    png_to_dib_inner(png).ok()
}

fn png_to_dib_inner(png: &[u8]) -> WinResult<Vec<u8>> {
    unsafe {
        let f = factory()?;
        let (conv, w, h) = decode_bgr24(&f, png)?;
        let stride = (w as usize * 3).div_ceil(4) * 4;
        let mut px = vec![0u8; stride * h as usize];
        conv.CopyPixels(std::ptr::null(), stride as u32, &mut px)?;
        let mut out = Vec::with_capacity(40 + px.len());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&(w as i32).to_le_bytes());
        out.extend_from_slice(&(h as i32).to_le_bytes()); // positive: bottom-up
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        out.extend_from_slice(&(px.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 16]);
        for row in px.chunks_exact(stride).rev() {
            out.extend_from_slice(row);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dib_2x2() -> Vec<u8> {
        // 2x2, 24bpp, bottom-up. Row stride 8 (6 + 2 pad).
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&24u16.to_le_bytes());
        d.extend_from_slice(&[0u8; 24]);
        d.extend_from_slice(&[255, 0, 0, 0, 255, 0, 0, 0]); // bottom row: blue, green
        d.extend_from_slice(&[0, 0, 255, 255, 255, 255, 0, 0]); // top row: red, white
        d
    }

    #[test]
    fn bmp_wrapper_offsets() {
        let dib = dib_2x2();
        let bmp = bmp_file_from_dib(&dib).unwrap();
        assert_eq!(&bmp[..2], b"BM");
        assert_eq!(u32_at(&bmp, 2) as usize, bmp.len());
        assert_eq!(u32_at(&bmp, 10), 14 + 40);

        // 8bpp without clr_used: 256 palette entries.
        let mut d8 = dib.clone();
        d8[14..16].copy_from_slice(&8u16.to_le_bytes());
        d8.resize(40 + 1024 + 8, 0);
        assert_eq!(u32_at(&bmp_file_from_dib(&d8).unwrap(), 10), 14 + 40 + 1024);

        // 32bpp BI_BITFIELDS with a 40-byte header: 3 masks.
        let mut d32 = dib.clone();
        d32[14..16].copy_from_slice(&32u16.to_le_bytes());
        d32[16..20].copy_from_slice(&3u32.to_le_bytes());
        d32.resize(40 + 12 + 16, 0);
        assert_eq!(u32_at(&bmp_file_from_dib(&d32).unwrap(), 10), 14 + 40 + 12);

        // V5 header carries its own masks.
        let mut v5 = vec![0u8; 124 + 16];
        v5[0..4].copy_from_slice(&124u32.to_le_bytes());
        v5[14..16].copy_from_slice(&32u16.to_le_bytes());
        v5[16..20].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(u32_at(&bmp_file_from_dib(&v5).unwrap(), 10), 14 + 124);
    }

    #[test]
    fn bmp_wrapper_rejects_bad_input() {
        assert!(bmp_file_from_dib(&[]).is_none());
        assert!(bmp_file_from_dib(&[0u8; 39]).is_none());
        let mut d = dib_2x2();
        d[0..4].copy_from_slice(&12u32.to_le_bytes());
        assert!(bmp_file_from_dib(&d).is_none());
        let mut d = dib_2x2();
        d[16..20].copy_from_slice(&5u32.to_le_bytes());
        assert!(bmp_file_from_dib(&d).is_none());
        let mut d = dib_2x2();
        d[14..16].copy_from_slice(&8u16.to_le_bytes()); // palette larger than data
        assert!(bmp_file_from_dib(&d).is_none());
    }

    #[test]
    fn png_header_parsing() {
        let mut p = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        p.extend_from_slice(b"IHDR");
        p.extend_from_slice(&7u32.to_be_bytes());
        p.extend_from_slice(&9u32.to_be_bytes());
        assert_eq!(png_dimensions(&p), Some((7, 9)));
        assert_eq!(png_dimensions(&p[..20]), None);
        assert_eq!(png_dimensions(b"not a png at all, definitely"), None);
    }

    #[test]
    fn hdrop_layout() {
        let b = build_hdrop(&[r"C:\a.txt".into(), r"D:\b".into()]);
        assert_eq!(u32_at(&b, 0), 20);
        assert_eq!(u32_at(&b, 16), 1);
        let wide: Vec<u16> = b[20..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let s = String::from_utf16(&wide).unwrap();
        assert_eq!(s, "C:\\a.txt\0D:\\b\0\0");
    }

    #[test]
    fn wic_roundtrip_dib_png_dib() {
        let _com = ComGuard::new();
        let (png, w, h) = dib_to_png(&dib_2x2()).expect("encode");
        assert_eq!((w, h), (2, 2));
        assert_eq!(png_dimensions(&png), Some((2, 2)));
        let back = png_to_dib(&png).expect("decode");
        assert_eq!(&back[40..], &dib_2x2()[40..], "pixels survive the round trip");
    }
}
