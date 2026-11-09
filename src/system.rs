//! Windows integration: the user's accent color and a title bar painted to match the app.

use eframe::egui::Color32;
use elegance::Theme;

const HKEY_CURRENT_USER: isize = 0x8000_0001_u32 as i32 as isize;
const RRF_RT_REG_BINARY: u32 = 0x0000_0008;
const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
const DWMWA_CAPTION_COLOR: u32 = 35;
const DWMWA_TEXT_COLOR: u32 = 36;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        hkey: isize,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut core::ffi::c_void,
        len: *mut u32,
    ) -> i32;
}

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmSetWindowAttribute(hwnd: isize, attr: u32, value: *const core::ffi::c_void, size: u32) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Accent shades Windows derives from the chosen accent color:
/// [light3, light2, light1, accent, dark1, dark2, dark3].
#[derive(Clone, Copy, PartialEq)]
pub struct Accent([Color32; 7]);

impl Accent {
    pub fn read() -> Self {
        let mut buf = [0u8; 32];
        let mut len = buf.len() as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                wide(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent").as_ptr(),
                wide("AccentPalette").as_ptr(),
                RRF_RT_REG_BINARY,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if status != 0 || len < 28 {
            // Windows default blue.
            return Self([0x99EBFF, 0x4CC2FF, 0x0091F8, 0x0078D4, 0x005FB8, 0x004298, 0x002C6C].map(|c| {
                Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
            }));
        }
        Self(std::array::from_fn(|i| Color32::from_rgb(buf[i * 4], buf[i * 4 + 1], buf[i * 4 + 2])))
    }
}

/// Neutral Windows 11 surfaces with the system accent, instead of the stock slate/blue palette.
pub fn theme(dark: bool, accent: Accent) -> Theme {
    let rgb = |c: u32| Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8);
    let [_light3, light2, light1, base, dark1, _dark2, _dark3] = accent.0;
    let mut t = if dark { Theme::charcoal() } else { Theme::paper() };
    let p = &mut t.palette;
    if dark {
        p.bg = rgb(0x202020);
        p.card = rgb(0x2b2b2b);
        p.input_bg = rgb(0x1f1f1f);
        p.border = rgb(0x3d3d3d);
        p.text = rgb(0xffffff);
        p.text_muted = rgb(0xc5c5c5);
        p.text_faint = rgb(0x8b8b8b);
        p.blue = base;
        p.blue_hover = light1;
        p.focus = light2;
    } else {
        p.bg = rgb(0xf3f3f3);
        p.card = rgb(0xfbfbfb);
        p.input_bg = rgb(0xffffff);
        p.border = rgb(0xd1d1d1);
        p.text = rgb(0x1b1b1b);
        p.text_muted = rgb(0x5d5d5d);
        p.text_faint = rgb(0x8f8f8f);
        p.blue = base;
        p.blue_hover = dark1;
        p.focus = base;
    }
    t
}

/// Paints the native title bar in the app background color so the two blend (Windows 11+).
pub fn style_title_bar(hwnd: isize, dark: bool, caption: Color32, text: Color32) {
    let colorref = |c: Color32| c.r() as u32 | (c.g() as u32) << 8 | (c.b() as u32) << 16;
    let dark_mode = dark as i32;
    let (caption, text) = (colorref(caption), colorref(text));
    unsafe {
        DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&raw const dark_mode).cast(), 4);
        DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, (&raw const caption).cast(), 4);
        DwmSetWindowAttribute(hwnd, DWMWA_TEXT_COLOR, (&raw const text).cast(), 4);
    }
}
