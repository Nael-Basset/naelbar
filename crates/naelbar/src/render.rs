//! D3D11 + composition swapchain + DirectComposition + Direct2D + DirectWrite.
//!
//! [`Core`] owns what must stay alive for the pill to remain on screen.
//! [`D2d`] is everything else and can be dropped while the island is closed.
//!
//! Merely holding a hardware D3D11 device keeps a driver thread waking at the
//! refresh rate on this machine, so the settled closed pill is rendered once
//! on WARP and the hardware device is dropped until the island opens again.

use std::collections::HashMap;
use std::mem::ManuallyDrop;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::core::{HRESULT, Interface, PCWSTR, Result, w};
use windows_numerics::{Matrix3x2, Vector2};

use crate::ui::icons::P;
use crate::ui::{Align, Cmd, Color, Icon, ImageRef, Pt, Rect, TextStyle};

struct Core {
    swap: IDXGISwapChain2,
    waitable: HANDLE,
    dxgi: IDXGIDevice1,
    _d3d: ID3D11Device,
    _dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
}

impl Drop for Core {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.waitable);
        }
    }
}

#[derive(PartialEq, Eq, Hash)]
struct TextKey {
    s: Box<str>,
    size: u32,
    weight: u16,
    w: u32,
    h: u32,
    align: Align,
    valign: Align,
    wrap: bool,
}

struct D2d {
    factory: ID2D1Factory1,
    dc: ID2D1DeviceContext,
    _device: ID2D1Device,
    _target: ID2D1Bitmap1,
    brush: ID2D1SolidColorBrush,
    stroke: ID2D1StrokeStyle1,
    dwrite: IDWriteFactory,
    wic: IWICImagingFactory,
    family: &'static PCWSTRStatic,
    formats: HashMap<(u32, u16), IDWriteTextFormat>,
    ellipsis: Option<IDWriteInlineObject>,
    layouts: HashMap<TextKey, IDWriteTextLayout>,
    icons: HashMap<Icon, ID2D1PathGeometry1>,
    images: Vec<(ImageRef, ID2D1Bitmap)>,
    max_edge: u32,
}

/// Wrapper so a font family name can live in a static.
struct PCWSTRStatic(PCWSTR);
unsafe impl Sync for PCWSTRStatic {}

static FAMILY_VARIABLE: PCWSTRStatic = PCWSTRStatic(w!("Segoe UI Variable Text"));
static FAMILY_FALLBACK: PCWSTRStatic = PCWSTRStatic(w!("Segoe UI"));

const MAX_LAYOUTS: usize = 400;
const MAX_IMAGES: usize = 12;
/// Decoded images are scaled to at most this edge length in pixels.
const MAX_IMAGE_EDGE: u32 = 320;

pub struct Renderer {
    hwnd: HWND,
    w: u32,
    h: u32,
    scale: f32,
    core: Option<Core>,
    d2d: Option<D2d>,
    max_edge: u32,
}

const D2DERR_RECREATE_TARGET: HRESULT = HRESULT(0x8899000C_u32 as i32);

fn is_device_lost(e: &windows::core::Error) -> bool {
    let c = e.code();
    c == DXGI_ERROR_DEVICE_REMOVED || c == DXGI_ERROR_DEVICE_RESET || c == DXGI_ERROR_DEVICE_HUNG || c == D2DERR_RECREATE_TARGET
}

fn rect_f(r: Rect) -> D2D_RECT_F {
    D2D_RECT_F { left: r.x, top: r.y, right: r.x + r.w, bottom: r.y + r.h }
}

fn color_f(c: Color) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: c.r, g: c.g, b: c.b, a: c.a }
}

fn rrect(r: Rect, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect: rect_f(r), radiusX: radius, radiusY: radius }
}

fn pt2(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

fn point(p: Pt) -> Vector2 {
    pt2(p.x, p.y)
}

const IDENTITY: Matrix3x2 = Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: 0.0, M32: 0.0 };

impl Renderer {
    pub fn new(hwnd: HWND) -> Self {
        Self { hwnd, w: 1, h: 1, scale: 1.0, core: None, d2d: None, max_edge: MAX_IMAGE_EDGE }
    }

    /// Thumbnails stay small; a full-size preview raises this limit.
    pub fn set_max_image_edge(&mut self, edge: u32) {
        self.max_edge = edge.max(1);
    }

    /// Sets the backing size. Any change discards the swapchain.
    pub fn set_size(&mut self, w: u32, h: u32, scale: f32) {
        if (w, h) != (self.w, self.h) || scale != self.scale {
            self.w = w;
            self.h = h;
            self.scale = scale;
            self.reset();
        }
    }

    /// Drops every GPU object. The next render recreates them.
    pub fn reset(&mut self) {
        self.d2d = None;
        self.core = None;
    }

    /// Closed and nothing changing: drops the Direct2D objects. The swapchain
    /// keeps the last frame on screen.
    pub fn enter_idle(&mut self) {
        self.d2d = None;
    }

    pub fn waitable(&self) -> Option<HANDLE> {
        self.core.as_ref().map(|c| c.waitable)
    }

    fn ensure(&mut self) -> Result<()> {
        unsafe {
            if self.core.is_none() {
                self.core = Some(create_core(self.hwnd, self.w, self.h)?);
            }
            if self.d2d.is_none() {
                let core = self.core.as_ref().unwrap();
                let mut d2d = create_d2d(core, self.scale)?;
                d2d.max_edge = self.max_edge;
                self.d2d = Some(d2d);
            }
        }
        Ok(())
    }

    /// Draws and presents one frame. On device loss every object is dropped
    /// and an error returned; the caller redraws later.
    pub fn render(&mut self, cmds: &[Cmd]) -> Result<()> {
        let result = self.ensure().and_then(|()| unsafe {
            let d2d = self.d2d.as_mut().unwrap();
            draw(d2d, cmds)?;
            self.core.as_ref().unwrap().swap.Present(1, DXGI_PRESENT(0)).ok()
        });
        if let Err(e) = &result {
            if is_device_lost(e) {
                self.reset();
            }
        }
        result
    }

}

/// Always WARP. The island is small enough for software rendering to hold
/// vsync, and merely holding a hardware device keeps a driver thread waking
/// tens of times per second.
unsafe fn create_device() -> Result<ID3D11Device> {
    let levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
    let mut device = None;
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    D3D11CreateDevice(None, D3D_DRIVER_TYPE_WARP, HMODULE::default(), flags, Some(&levels), D3D11_SDK_VERSION, Some(&mut device), None, None)?;
    Ok(device.unwrap())
}

unsafe fn create_core(hwnd: HWND, w: u32, h: u32) -> Result<Core> {
    let d3d = create_device()?;
    let dxgi: IDXGIDevice1 = d3d.cast()?;
    let adapter = dxgi.GetAdapter()?;
    let factory: IDXGIFactory2 = adapter.GetParent()?;
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: w,
        Height: h,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
        Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
        ..Default::default()
    };
    let swap1 = factory.CreateSwapChainForComposition(&d3d, &desc, None)?;
    let swap: IDXGISwapChain2 = swap1.cast()?;
    swap.SetMaximumFrameLatency(1)?;
    let waitable = swap.GetFrameLatencyWaitableObject();

    let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
    let target = dcomp.CreateTargetForHwnd(hwnd, true)?;
    let visual = dcomp.CreateVisual()?;
    visual.SetContent(&swap)?;
    target.SetRoot(&visual)?;
    dcomp.Commit()?;

    Ok(Core { swap, waitable, dxgi, _d3d: d3d, _dcomp: dcomp, _target: target, _visual: visual })
}

unsafe fn create_d2d(core: &Core, scale: f32) -> Result<D2d> {
    let options = D2D1_FACTORY_OPTIONS::default();
    let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&options))?;
    let device = factory.CreateDevice(&core.dxgi)?;
    let dc = device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
    dc.SetDpi(96.0 * scale, 96.0 * scale);
    dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
    dc.SetUnitMode(D2D1_UNIT_MODE_DIPS);

    let surface: IDXGISurface = core.swap.GetBuffer(0)?;
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0 * scale,
        dpiY: 96.0 * scale,
        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
        colorContext: ManuallyDrop::new(None),
    };
    let target = dc.CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
    dc.SetTarget(&target);

    let brush = dc.CreateSolidColorBrush(&color_f(Color::rgb(255, 255, 255)), None)?;
    let stroke_props = D2D1_STROKE_STYLE_PROPERTIES1 {
        startCap: D2D1_CAP_STYLE_ROUND,
        endCap: D2D1_CAP_STYLE_ROUND,
        dashCap: D2D1_CAP_STYLE_ROUND,
        lineJoin: D2D1_LINE_JOIN_ROUND,
        miterLimit: 10.0,
        dashStyle: D2D1_DASH_STYLE_SOLID,
        dashOffset: 0.0,
        transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
    };
    let stroke = factory.CreateStrokeStyle(&stroke_props, None)?;
    let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
    let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;

    let mut collection = None;
    dwrite.GetSystemFontCollection(&mut collection, false)?;
    let mut index = 0;
    let mut exists = windows::core::BOOL(0);
    if let Some(c) = &collection {
        c.FindFamilyName(FAMILY_VARIABLE.0, &mut index, &mut exists)?;
    }
    let family = if exists.as_bool() { &FAMILY_VARIABLE } else { &FAMILY_FALLBACK };

    Ok(D2d {
        factory,
        dc,
        _device: device,
        _target: target,
        brush,
        stroke,
        dwrite,
        wic,
        family,
        formats: HashMap::new(),
        ellipsis: None,
        layouts: HashMap::new(),
        icons: HashMap::new(),
        images: Vec::new(),
        max_edge: MAX_IMAGE_EDGE,
    })
}

unsafe fn draw(d: &mut D2d, cmds: &[Cmd]) -> Result<()> {
    d.dc.BeginDraw();
    d.dc.Clear(Some(&color_f(Color::default())));
    let mut clips: Vec<bool> = Vec::with_capacity(4);
    for cmd in cmds {
        match cmd {
            Cmd::FillRR { r, radius, c } => {
                d.brush.SetColor(&color_f(*c));
                if *radius > 0.0 {
                    d.dc.FillRoundedRectangle(&rrect(*r, *radius), &d.brush);
                } else {
                    d.dc.FillRectangle(&rect_f(*r), &d.brush);
                }
            }
            Cmd::StrokeRR { r, radius, width, c } => {
                d.brush.SetColor(&color_f(*c));
                d.dc.DrawRoundedRectangle(&rrect(*r, *radius), &d.brush, *width, None);
            }
            Cmd::Text { r, s, style } => {
                if let Ok(layout) = text_layout(d, r, s, style) {
                    d.brush.SetColor(&color_f(style.color));
                    d.dc.DrawTextLayout(point(Pt { x: r.x, y: r.y }), &layout, &d.brush, D2D1_DRAW_TEXT_OPTIONS_CLIP);
                }
            }
            Cmd::Icon { icon, r, c, width } => {
                if let Ok(geo) = icon_geometry(d, *icon) {
                    let k = r.w / 24.0;
                    let m = Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: r.x, M32: r.y };
                    d.dc.SetTransform(&m);
                    d.brush.SetColor(&color_f(*c));
                    if icon.filled() {
                        d.dc.FillGeometry(&geo, &d.brush, None);
                    } else {
                        d.dc.DrawGeometry(&geo, &d.brush, *width / k, &d.stroke);
                    }
                    d.dc.SetTransform(&IDENTITY);
                }
            }
            Cmd::Image { r, radius, image, opacity, contain } => {
                if let Some(bmp) = image_bitmap(d, image) {
                    let sz = bmp.GetPixelSize();
                    let (sw, sh) = (sz.width as f32, sz.height as f32);
                    let (dest, src) = if *contain {
                        let k = (r.w / sw).min(r.h / sh);
                        (r.centered(sw * k, sh * k), D2D_RECT_F { left: 0.0, top: 0.0, right: sw, bottom: sh })
                    } else {
                        let cover = (r.w / sw).max(r.h / sh);
                        let (cw, ch) = (r.w / cover, r.h / cover);
                        (*r, D2D_RECT_F { left: (sw - cw) * 0.5, top: (sh - ch) * 0.5, right: (sw + cw) * 0.5, bottom: (sh + ch) * 0.5 })
                    };
                    let layer = push_clip(d, &dest, *radius);
                    d.dc.DrawBitmap(&bmp, Some(&rect_f(dest)), *opacity, D2D1_INTERPOLATION_MODE_LINEAR, Some(&src), None);
                    pop_clip(d, layer);
                }
            }
            Cmd::PushClip { r, radius } => clips.push(push_clip(d, r, *radius)),
            Cmd::PopClip => {
                if let Some(layer) = clips.pop() {
                    pop_clip(d, layer);
                }
            }
        }
    }
    while let Some(layer) = clips.pop() {
        pop_clip(d, layer);
    }
    d.dc.EndDraw(None, None)
}

/// Returns true when a layer was pushed, false for an axis-aligned clip.
unsafe fn push_clip(d: &D2d, r: &Rect, radius: f32) -> bool {
    if radius <= 0.0 {
        d.dc.PushAxisAlignedClip(&rect_f(*r), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        return false;
    }
    let Ok(geo) = d.factory.CreateRoundedRectangleGeometry(&rrect(*r, radius)) else {
        d.dc.PushAxisAlignedClip(&rect_f(*r), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        return false;
    };
    let mask: ID2D1Geometry = geo.cast().unwrap();
    let params = D2D1_LAYER_PARAMETERS1 {
        contentBounds: D2D_RECT_F { left: f32::MIN, top: f32::MIN, right: f32::MAX, bottom: f32::MAX },
        geometricMask: ManuallyDrop::new(Some(mask)),
        maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
        maskTransform: IDENTITY,
        opacity: 1.0,
        opacityBrush: ManuallyDrop::new(None),
        layerOptions: D2D1_LAYER_OPTIONS1_NONE,
    };
    d.dc.PushLayer(&params, None);
    drop(ManuallyDrop::into_inner(params.geometricMask));
    true
}

unsafe fn pop_clip(d: &D2d, layer: bool) {
    if layer {
        d.dc.PopLayer();
    } else {
        d.dc.PopAxisAlignedClip();
    }
}

unsafe fn text_format(d: &mut D2d, size: f32, weight: u16) -> Result<IDWriteTextFormat> {
    let key = ((size * 100.0) as u32, weight);
    if let Some(f) = d.formats.get(&key) {
        return Ok(f.clone());
    }
    let f = d.dwrite.CreateTextFormat(
        d.family.0,
        None,
        DWRITE_FONT_WEIGHT(weight as i32),
        DWRITE_FONT_STYLE_NORMAL,
        DWRITE_FONT_STRETCH_NORMAL,
        size,
        w!("en-us"),
    )?;
    d.formats.insert(key, f.clone());
    Ok(f)
}

unsafe fn text_layout(d: &mut D2d, r: &Rect, s: &str, style: &TextStyle) -> Result<IDWriteTextLayout> {
    let key = TextKey {
        s: s.into(),
        size: (style.size * 100.0) as u32,
        weight: style.weight,
        w: (r.w * 4.0) as u32,
        h: (r.h * 4.0) as u32,
        align: style.align,
        valign: style.valign,
        wrap: style.wrap,
    };
    if let Some(l) = d.layouts.get(&key) {
        return Ok(l.clone());
    }
    if d.layouts.len() >= MAX_LAYOUTS {
        d.layouts.clear();
    }
    let format = text_format(d, style.size, style.weight)?;
    let wide: Vec<u16> = s.encode_utf16().collect();
    let layout = d.dwrite.CreateTextLayout(&wide, &format, r.w.max(1.0), r.h.max(1.0))?;
    layout.SetTextAlignment(match style.align {
        Align::Start => DWRITE_TEXT_ALIGNMENT_LEADING,
        Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
        Align::End => DWRITE_TEXT_ALIGNMENT_TRAILING,
    })?;
    layout.SetParagraphAlignment(match style.valign {
        Align::Start => DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
        Align::Center => DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        Align::End => DWRITE_PARAGRAPH_ALIGNMENT_FAR,
    })?;
    layout.SetWordWrapping(if style.wrap { DWRITE_WORD_WRAPPING_WRAP } else { DWRITE_WORD_WRAPPING_NO_WRAP })?;
    if d.ellipsis.is_none() {
        d.ellipsis = d.dwrite.CreateEllipsisTrimmingSign(&format).ok();
    }
    let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
    layout.SetTrimming(&trimming, d.ellipsis.as_ref())?;
    d.layouts.insert(key, layout.clone());
    Ok(layout)
}

unsafe fn icon_geometry(d: &mut D2d, icon: Icon) -> Result<ID2D1PathGeometry1> {
    if let Some(g) = d.icons.get(&icon) {
        return Ok(g.clone());
    }
    let geo = d.factory.CreatePathGeometry()?;
    let sink = geo.Open()?;
    let filled = icon.filled();
    let begin = if filled { D2D1_FIGURE_BEGIN_FILLED } else { D2D1_FIGURE_BEGIN_HOLLOW };
    let mut open = false;
    let arc = |x: f32, y: f32, r: f32| D2D1_ARC_SEGMENT {
        point: pt2(x, y),
        size: D2D_SIZE_F { width: r, height: r },
        rotationAngle: 0.0,
        sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
        arcSize: D2D1_ARC_SIZE_SMALL,
    };
    for op in icon.path() {
        match *op {
            P::M(x, y) => {
                if open {
                    sink.EndFigure(D2D1_FIGURE_END_OPEN);
                }
                sink.BeginFigure(pt2(x, y), begin);
                open = true;
            }
            P::L(x, y) => sink.AddLine(pt2(x, y)),
            P::C(x1, y1, x2, y2, x, y) => sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: pt2(x1, y1), point2: pt2(x2, y2), point3: pt2(x, y) }),
            P::Z => {
                sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                open = false;
            }
            P::End => {
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
                open = false;
            }
            P::Circle(cx, cy, r) => {
                sink.BeginFigure(pt2(cx + r, cy), begin);
                sink.AddArc(&arc(cx - r, cy, r));
                sink.AddArc(&arc(cx + r, cy, r));
                sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            }
            P::RR(x, y, w, h, r) => {
                sink.BeginFigure(pt2(x + r, y), begin);
                sink.AddLine(pt2(x + w - r, y));
                sink.AddArc(&arc(x + w, y + r, r));
                sink.AddLine(pt2(x + w, y + h - r));
                sink.AddArc(&arc(x + w - r, y + h, r));
                sink.AddLine(pt2(x + r, y + h));
                sink.AddArc(&arc(x, y + h - r, r));
                sink.AddLine(pt2(x, y + r));
                sink.AddArc(&arc(x + r, y, r));
                sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            }
        }
    }
    if open {
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
    }
    sink.Close()?;
    d.icons.insert(icon, geo.clone());
    Ok(geo)
}

/// Decodes on first use. Holding the `ImageRef` keeps the allocation identity
/// stable for the lifetime of the cache entry.
unsafe fn image_bitmap(d: &mut D2d, image: &ImageRef) -> Option<ID2D1Bitmap> {
    if let Some((_, b)) = d.images.iter().find(|(k, _)| k == image) {
        return Some(b.clone());
    }
    let bitmap = decode_image(d, &image.0).ok()?;
    if d.images.len() >= MAX_IMAGES {
        d.images.remove(0);
    }
    d.images.push((image.clone(), bitmap.clone()));
    Some(bitmap)
}

unsafe fn decode_image(d: &D2d, bytes: &[u8]) -> Result<ID2D1Bitmap> {
    let stream = SHCreateMemStream(Some(bytes)).ok_or_else(windows::core::Error::from_thread)?;
    let decoder = d.wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
    let frame = decoder.GetFrame(0)?;
    let (mut w, mut h) = (0, 0);
    frame.GetSize(&mut w, &mut h)?;
    let mut source: IWICBitmapSource = frame.cast()?;
    let largest = w.max(h);
    if largest > d.max_edge {
        let k = d.max_edge as f32 / largest as f32;
        let scaler = d.wic.CreateBitmapScaler()?;
        let (tw, th) = (((w as f32 * k) as u32).max(1), ((h as f32 * k) as u32).max(1));
        scaler.Initialize(&source, tw, th, WICBitmapInterpolationModeFant)?;
        source = scaler.cast()?;
    }
    let conv = d.wic.CreateFormatConverter()?;
    conv.Initialize(&source, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
    d.dc.CreateBitmapFromWicBitmap(&conv, None)?.cast()
}
