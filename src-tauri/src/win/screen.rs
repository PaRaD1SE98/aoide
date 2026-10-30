//! 截取前台窗口的内容（屏幕上下文，见 screen.rs）（Windows）
//!
//! 用 PrintWindow(PW_RENDERFULLCONTENT) 让窗口把自己画进位图：只有这个窗口，盖在上面的其他窗口、
//! Aoide 的悬浮条都不会进来，也不需要把悬浮条排除出所有截屏（那样用户自己录屏时也看不到它）。
//! 少数程序画不出来（得到全黑的图）时，退而从屏幕上截取窗口所在的区域

use crate::screen::Shot;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC, SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, IsIconic};

/// 让 DirectComposition 渲染的内容（Chromium、UWP 等）也画出来
const PW_RENDERFULLCONTENT: u32 = 2;

/// 截图不需要额外的权限（和 macOS 的接口一致）
pub fn has_access() -> bool {
    true
}

pub fn request_access() -> bool {
    true
}

pub fn capture_window() -> Option<Shot> {
    capture(unsafe { GetForegroundWindow() })
}

fn capture(hwnd: HWND) -> Option<Shot> {
    unsafe {
        if hwnd.is_invalid() || IsIconic(hwnd).as_bool() {
            return None;
        }
        // 前台是 Aoide 自己的窗口（设置页）时没有可参考的内容
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == std::process::id() {
            return None;
        }
        let mut outer = RECT::default();
        GetWindowRect(hwnd, &mut outer).ok()?;
        // 实际看得见的边框（GetWindowRect 含 Win10 起看不见的缩放边框）
        let mut frame = outer;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut frame as *mut RECT as *mut _, size_of::<RECT>() as u32);
        let (w, h) = (frame.right - frame.left, frame.bottom - frame.top);
        if w < 50 || h < 50 {
            return None;
        }
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let rgb = draw(hwnd, screen, mem, outer, frame);
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        Shot::from_rgb(w as u32, h as u32, rgb?)
    }
}

/// 窗口看得见的部分（frame）的 RGB 像素：先让窗口自己画（PrintWindow 按含看不见边框的 outer 画，再裁出 frame），
/// 画不出来时从屏幕上截取
unsafe fn draw(hwnd: HWND, screen: HDC, mem: HDC, outer: RECT, frame: RECT) -> Option<Vec<u8>> {
    let (w, h) = (frame.right - frame.left, frame.bottom - frame.top);
    let (ow, oh) = (outer.right - outer.left, outer.bottom - outer.top);
    let full = CreateCompatibleBitmap(screen, ow, oh);
    let old = SelectObject(mem, full.into());
    let printed = PrintWindow(hwnd, mem, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool();
    SelectObject(mem, old);
    let drawn = printed.then(|| pixels(mem, full, ow, oh)).flatten().map(|p| crop(&p, ow, frame.left - outer.left, frame.top - outer.top, w, h));
    let _ = DeleteObject(full.into());
    if let Some(p) = drawn.filter(|p| !all_black(p)) {
        return Some(p);
    }
    log::info!("Screen context: the window didn't draw itself, copying it from the screen");
    let bmp = CreateCompatibleBitmap(screen, w, h);
    let old = SelectObject(mem, bmp.into());
    let ok = BitBlt(mem, 0, 0, w, h, Some(screen), frame.left, frame.top, SRCCOPY | CAPTUREBLT).is_ok();
    SelectObject(mem, old);
    let p = ok.then(|| pixels(mem, bmp, w, h)).flatten();
    let _ = DeleteObject(bmp.into());
    p
}

/// 位图的 RGB 像素（从上到下逐行）
unsafe fn pixels(dc: HDC, bmp: HBITMAP, w: i32, h: i32) -> Option<Vec<u8>> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // 负数：从上到下
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    let lines = GetDIBits(dc, bmp, 0, h as u32, Some(bgra.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
    if lines != h {
        return None;
    }
    Some(bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect())
}

/// 从 RGB 像素（每行 full_w 像素）里裁出 (x, y, w, h)
fn crop(rgb: &[u8], full_w: i32, x: i32, y: i32, w: i32, h: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for row in y..y + h {
        let start = ((row * full_w + x) * 3) as usize;
        out.extend_from_slice(&rgb[start..start + (w * 3) as usize]);
    }
    out
}

/// 抽样看是不是整张都是黑的（窗口没能把自己画出来）
fn all_black(rgb: &[u8]) -> bool {
    rgb.iter().step_by(97).all(|&v| v < 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手动运行：截取前台窗口存成 JPEG，看看发给 LLM 的是什么。DELAY=秒数 可先等一会儿，留时间切到目标程序
    /// cargo test --lib save_foreground_window -- --ignored --nocapture
    #[test]
    #[ignore]
    fn save_foreground_window() {
        let delay = std::env::var("DELAY").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
        std::thread::sleep(std::time::Duration::from_secs(delay));
        let start = std::time::Instant::now();
        let shot = capture_window().expect("no screenshot");
        let path = std::env::temp_dir().join("aoide-screen-context.jpg");
        let url = shot.data_url();
        let jpeg = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, url.split(',').nth(1).unwrap()).unwrap();
        std::fs::write(&path, jpeg).unwrap();
        println!("{}x{} in {:?} -> {}", shot.width, shot.height, start.elapsed(), path.display());
    }

    #[test]
    fn crops_pixels() {
        // 3x2 的图，每个像素 RGB 都是它的编号
        let rgb: Vec<u8> = (0..6u8).flat_map(|i| [i, i, i]).collect();
        assert_eq!(crop(&rgb, 3, 1, 0, 2, 2), [1, 1, 1, 2, 2, 2, 4, 4, 4, 5, 5, 5]);
    }
}
