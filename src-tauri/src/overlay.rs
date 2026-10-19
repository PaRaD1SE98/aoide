//! 录音提示悬浮条：输入焦点所在屏幕底部居中的胶囊，录音时显示随音量跳动的波形，识别中显示波浪动画
//! 显示期间持续跟随焦点：切到另一块屏幕上的输入框，悬浮条也换到那块屏幕的同一位置
//!
//! 不用 WebView 窗口：Tauri（tao）的「无边框」窗口在 Windows 上仍带 WS_CAPTION 等样式，
//! 只是靠处理 WM_NCCALCSIZE 把边框藏起来；所在线程一卡（几秒不处理消息），系统的
//! 窗口幽灵化（ghosting）机制就会接管并画出带关闭按钮的默认外壳。这里改用纯 WS_POPUP
//! 分层窗口：根本没有非客户区可画；在独立线程里跑自己的消息循环，主线程卡顿也不影响；
//! 用 tiny-skia 绘制、UpdateLayeredWindow 逐像素透明输出。
//! 窗口不抢焦点（WS_EX_NOACTIVATE）、鼠标穿透（WS_EX_TRANSPARENT），不影响打字和粘贴。

use crate::audio;
use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tiny_skia::{FillRule, Paint, Path, PathBuilder, Pixmap, Transform};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC,
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, ReleaseDC, SelectObject, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP,
    HDC, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetForegroundWindow,
    GetGUIThreadInfo, GetMessageW, GetWindowThreadProcessId, KillTimer, GUITHREADINFO,
    PostMessageW, RegisterClassW, SetTimer, SetWindowPos, ShowWindow, TranslateMessage,
    UpdateLayeredWindow, HTTRANSPARENT, HWND_TOPMOST, MA_NOACTIVATE, MSG, SWP_NOACTIVATE,
    SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WM_APP, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_TIMER,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Hidden,
    /// 普通录音
    Recording,
    /// 语音修订录音
    Revising,
    /// 录音结束，转写中
    Processing,
    /// LLM 润色 / 修订中
    Polishing,
}

/// 胶囊的逻辑尺寸（按显示器 DPI 缩放）
const WIDTH: f32 = 120.0;
const HEIGHT: f32 = 36.0;
/// 距工作区（任务栏上沿）的距离
const BOTTOM_GAP: f32 = 16.0;
const N_BARS: usize = 12;
const FRAME_MS: u32 = 16;
const FADE_PER_FRAME: f32 = 0.16;

/// 显示期间检查焦点所在屏幕的间隔
const FOLLOW_MS: u128 = 200;

const WM_APP_UPDATE: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static MODE: Mutex<Mode> = Mutex::new(Mode::Hidden);

/// 创建悬浮窗线程（整个进程调用一次）
pub fn init() {
    std::thread::Builder::new()
        .name("overlay".into())
        .spawn(|| unsafe {
            let hinstance = GetModuleHandleW(None).unwrap_or_default();
            let class = w!("AoideOverlay");
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                ..Default::default()
            });
            let hwnd = match CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                class,
                w!("Aoide"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinstance.into()),
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    log::error!("Overlay window failed: {e}");
                    return;
                }
            };
            HWND_RAW.store(hwnd.0 as isize, Ordering::SeqCst);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .expect("spawn overlay thread");
}

/// 切换悬浮条状态（任意线程可调用）
pub fn set(mode: Mode) {
    *MODE.lock().unwrap() = mode;
    let raw = HWND_RAW.load(Ordering::SeqCst);
    if raw != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as _)), WM_APP_UPDATE, WPARAM(0), LPARAM(0));
        }
    }
}

/// 只在悬浮窗线程里访问的绘制状态
struct View {
    visible: bool,
    alpha: f32,
    /// 正在淡出时保留最后一个可见状态继续绘制
    shown_mode: Mode,
    pos: POINT,
    scale: f32,
    /// 当前所在显示器（HMONITOR）
    monitor: isize,
    last_follow: Instant,
    bars: [f32; N_BARS],
    last_shift: Instant,
    start: Instant,
    surface: Option<Surface>,
}

struct Surface {
    w: i32,
    h: i32,
    dc: HDC,
    bitmap: HBITMAP,
    bits: *mut u8,
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

thread_local! {
    static VIEW: RefCell<View> = RefCell::new(View {
        visible: false,
        alpha: 0.0,
        shown_mode: Mode::Hidden,
        pos: POINT::default(),
        scale: 1.0,
        monitor: 0,
        last_follow: Instant::now(),
        bars: [0.0; N_BARS],
        last_shift: Instant::now(),
        start: Instant::now(),
        surface: None,
    });
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_UPDATE => {
            VIEW.with_borrow_mut(|v| on_update(hwnd, v));
            LRESULT(0)
        }
        WM_TIMER => {
            VIEW.with_borrow_mut(|v| on_frame(hwnd, v));
            LRESULT(0)
        }
        // 不接受点击、不被激活
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn on_update(hwnd: HWND, v: &mut View) {
    let mode = *MODE.lock().unwrap();
    if mode == Mode::Hidden {
        return; // 由逐帧动画淡出后隐藏
    }
    if mode != v.shown_mode && v.shown_mode != Mode::Processing {
        v.bars = [0.0; N_BARS];
    }
    v.shown_mode = mode;
    if !v.visible {
        place(v, focus_monitor());
        v.visible = true;
        v.alpha = 0.0;
        v.start = Instant::now();
        render(hwnd, v);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            // 每次显示都重新置顶，压过后来出现的置顶窗口
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE);
            SetTimer(Some(hwnd), TIMER_ID, FRAME_MS, None);
        }
    }
}

fn on_frame(hwnd: HWND, v: &mut View) {
    let mode = *MODE.lock().unwrap();
    if mode == Mode::Hidden {
        v.alpha -= FADE_PER_FRAME;
        if v.alpha <= 0.0 {
            v.alpha = 0.0;
            v.visible = false;
            v.shown_mode = Mode::Hidden;
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            return;
        }
    } else {
        v.alpha = (v.alpha + FADE_PER_FRAME).min(1.0);
        // 跟随输入焦点换屏
        if v.last_follow.elapsed().as_millis() >= FOLLOW_MS {
            v.last_follow = Instant::now();
            let mon = focus_monitor();
            if mon.0 as isize != v.monitor {
                place(v, mon);
            }
        }
    }
    update_bars(v);
    render(hwnd, v);
}

#[cfg(test)]
static TEST_TARGET: AtomicIsize = AtomicIsize::new(0);

/// 输入焦点所在的显示器：系统插入符（输入光标）→ 焦点控件所在窗口 → 前台窗口 → 鼠标
fn focus_monitor() -> HMONITOR {
    unsafe {
        #[cfg(test)]
        {
            let target = TEST_TARGET.load(Ordering::SeqCst);
            if target != 0 {
                return MonitorFromWindow(HWND(target as _), MONITOR_DEFAULTTONEAREST);
            }
        }
        let fg = GetForegroundWindow();
        if !fg.is_invalid() {
            let tid = GetWindowThreadProcessId(fg, None);
            let mut gti = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
            if GetGUIThreadInfo(tid, &mut gti).is_ok() {
                if !gti.hwndCaret.is_invalid() {
                    // 插入符坐标相对于其所在窗口的客户区
                    let mut pt = POINT { x: gti.rcCaret.left, y: gti.rcCaret.bottom };
                    if ClientToScreen(gti.hwndCaret, &mut pt).as_bool() {
                        return MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
                    }
                }
                if !gti.hwndFocus.is_invalid() {
                    return MonitorFromWindow(gti.hwndFocus, MONITOR_DEFAULTTONEAREST);
                }
            }
            return MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
    }
}

/// 输入焦点所在显示器的工作区（物理像素）和缩放比例，供结果卡片定位
pub fn focus_work_area() -> (windows::Win32::Foundation::RECT, f64) {
    let mon = focus_monitor();
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
    unsafe {
        let _ = GetMonitorInfoW(mon, &mut info);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
    }
    (info.rcWork, dpi_x as f64 / 96.0)
}

/// 定位到指定显示器工作区的底部中间，并按该显示器的 DPI 缩放
fn place(v: &mut View, mon: HMONITOR) {
    unsafe {
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut info);
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        v.monitor = mon.0 as isize;
        v.scale = dpi_x as f32 / 96.0;
        let work = info.rcWork;
        let w = (WIDTH * v.scale).round() as i32;
        let h = (HEIGHT * v.scale).round() as i32;
        v.pos = POINT {
            x: work.left + (work.right - work.left - w) / 2,
            y: work.bottom - h - (BOTTOM_GAP * v.scale).round() as i32,
        };
    }
}

/// 录音时：每 60 ms 把当前音量推入波形（从右往左滚动）；转写中：波浪动画；润色中：高光扫过
fn update_bars(v: &mut View) {
    match v.shown_mode {
        Mode::Recording | Mode::Revising => {
            if v.last_shift.elapsed().as_millis() >= 60 {
                v.last_shift = Instant::now();
                v.bars.rotate_left(1);
                // 语音 RMS 通常在 0.01~0.2，开方压缩动态范围
                let level = (audio::input_level() * 8.0).sqrt().min(1.0);
                v.bars[N_BARS - 1] = level;
            }
        }
        Mode::Processing => {
            let t = v.start.elapsed().as_secs_f32();
            for (i, b) in v.bars.iter_mut().enumerate() {
                let wave = (t * 6.0 - i as f32 * 0.55).sin().max(0.0);
                *b = 0.15 + 0.45 * wave;
            }
        }
        Mode::Polishing => {
            // 一道高光从左往右扫过
            let t = v.start.elapsed().as_secs_f32();
            let span = N_BARS as f32 + 6.0;
            let pos = (t * 9.0) % span - 3.0;
            for (i, b) in v.bars.iter_mut().enumerate() {
                let d = i as f32 - pos;
                *b = 0.22 + 0.55 * (-d * d / 3.0).exp();
            }
        }
        Mode::Hidden => {}
    }
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = r * 0.5523; // 三次贝塞尔近似四分之一圆
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

fn paint(r: u8, g: u8, b: u8, a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(r, g, b, a);
    p.anti_alias = true;
    p
}

fn draw(v: &View) -> Option<Pixmap> {
    let s = v.scale;
    let (w, h) = ((WIDTH * s).round(), (HEIGHT * s).round());
    let mut pm = Pixmap::new(w as u32, h as u32)?;
    let id = Transform::identity();

    let polishing = v.shown_mode == Mode::Polishing;
    let t = v.start.elapsed().as_secs_f32();
    let (dot, bar) = match v.shown_mode {
        Mode::Revising => ([167, 139, 250], [221, 214, 254]),
        Mode::Processing => ([96, 205, 255], [186, 230, 253]),
        Mode::Polishing => ([251, 191, 36], [253, 230, 138]),
        _ => ([255, 77, 79], [255, 255, 255]),
    };

    // 胶囊底 + 细描边（润色中描边换成琥珀色）
    let pill = rounded_rect(0.5, 0.5, w - 1.0, h - 1.0, h / 2.0)?;
    pm.fill_path(&pill, &paint(24, 24, 27, 235), FillRule::Winding, id, None);
    let border = if polishing { paint(dot[0], dot[1], dot[2], 120) } else { paint(255, 255, 255, 40) };
    pm.stroke_path(&pill, &border, &tiny_skia::Stroke { width: 1.0 * s.max(1.0), ..Default::default() }, id, None);

    // 左侧状态标记：录音时呼吸闪烁的圆点；转写中常亮圆点；润色中换成星芒 ✦
    let (cx, cy) = (18.0 * s, h / 2.0);
    if polishing {
        let pulse = 0.75 + 0.25 * (t * 5.0).sin();
        let mark = sparkle(cx, cy, 6.5 * s * pulse, 1.6 * s)?;
        pm.fill_path(&mark, &paint(dot[0], dot[1], dot[2], 255), FillRule::Winding, id, None);
    } else {
        let pulse = if v.shown_mode == Mode::Processing { 1.0 } else { 0.65 + 0.35 * (t * 4.0).sin().abs() };
        let mark = PathBuilder::from_circle(cx, cy, 4.5 * s)?;
        pm.fill_path(&mark, &paint(dot[0], dot[1], dot[2], (255.0 * pulse) as u8), FillRule::Winding, id, None);
    }

    // 波形条：中间的条更高，看起来更自然
    let (left, right) = (32.0 * s, w - 16.0 * s);
    let bar_w = 3.0 * s;
    let step = (right - left - bar_w) / (N_BARS - 1) as f32;
    let (min_h, max_h) = (3.0 * s, h - 14.0 * s);
    for (i, &value) in v.bars.iter().enumerate() {
        let profile = 0.55 + 0.45 * (std::f32::consts::PI * (i as f32 + 0.5) / N_BARS as f32).sin();
        let bh = min_h + (max_h - min_h) * value * profile;
        let x = left + step * i as f32;
        // 润色中：被高光扫到的条更亮
        let alpha = if polishing { (110.0 + 145.0 * ((value - 0.22) / 0.55).clamp(0.0, 1.0)) as u8 } else { 230 };
        if let Some(path) = rounded_rect(x, (h - bh) / 2.0, bar_w, bh, bar_w / 2.0) {
            pm.fill_path(&path, &paint(bar[0], bar[1], bar[2], alpha), FillRule::Winding, id, None);
        }
    }
    Some(pm)
}

/// 四角星芒（外半径 r，内半径 inner）
fn sparkle(cx: f32, cy: f32, r: f32, inner: f32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    for k in 0..8 {
        let angle = std::f32::consts::FRAC_PI_4 * k as f32 - std::f32::consts::FRAC_PI_2;
        let radius = if k % 2 == 0 { r } else { inner };
        let (x, y) = (cx + radius * angle.cos(), cy + radius * angle.sin());
        if k == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.close();
    pb.finish()
}

fn render(hwnd: HWND, v: &mut View) {
    let Some(pm) = draw(v) else { return };
    let (w, h) = (pm.width() as i32, pm.height() as i32);
    unsafe {
        if v.surface.as_ref().is_none_or(|s| s.w != w || s.h != h) {
            v.surface = None;
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // 自上而下
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let Ok(bitmap) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
                let _ = DeleteDC(dc);
                return;
            };
            SelectObject(dc, bitmap.into());
            v.surface = Some(Surface { w, h, dc, bitmap, bits: bits as *mut u8 });
        }
        let surface = v.surface.as_ref().unwrap();

        // tiny-skia 输出预乘 RGBA，分层窗口要预乘 BGRA
        let dst = std::slice::from_raw_parts_mut(surface.bits, (w * h * 4) as usize);
        for (d, s) in dst.chunks_exact_mut(4).zip(pm.data().chunks_exact(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = s[3];
        }

        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: (v.alpha * 255.0) as u8,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let size = SIZE { cx: w, cy: h };
        let src = POINT::default();
        let _ = UpdateLayeredWindow(
            hwnd,
            None,
            Some(&v.pos),
            Some(&size),
            Some(surface.dc),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use windows::core::BOOL;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::EnumDisplayMonitors;
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, GetWindowRect, SWP_NOZORDER};

    unsafe extern "system" fn collect(mon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        (*(data.0 as *mut Vec<HMONITOR>)).push(mon);
        true.into()
    }

    fn work_area(mon: HMONITOR) -> RECT {
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        unsafe {
            let _ = GetMonitorInfoW(mon, &mut info);
        }
        info.rcWork
    }

    /// README 配图：用与程序相同的绘制代码把各状态的胶囊输出成 PNG（2 倍分辨率），不涉及真实屏幕。
    /// 由 scripts/readme-images.ps1 调用：VT_IMG_DIR=… cargo test --lib export_capsule_images -- --ignored
    #[test]
    #[ignore]
    fn export_capsule_images() {
        let dir = std::path::PathBuf::from(std::env::var("VT_IMG_DIR").unwrap_or_else(|_| "target/readme".into()));
        std::fs::create_dir_all(&dir).unwrap();
        // 一段像说话的波形
        let speech = [0.18, 0.4, 0.72, 0.95, 0.6, 0.38, 0.82, 1.0, 0.58, 0.3, 0.52, 0.22];
        let at = |secs: f32| Instant::now() - Duration::from_secs_f32(secs);
        for (name, mode, t) in [
            ("recording", Mode::Recording, 0.4),
            ("revising", Mode::Revising, 0.4),
            ("transcribing", Mode::Processing, 0.35),
            ("polishing", Mode::Polishing, 0.6),
        ] {
            let mut v = View {
                visible: true,
                alpha: 1.0,
                shown_mode: mode,
                pos: POINT::default(),
                scale: 2.0,
                monitor: 0,
                last_follow: Instant::now(),
                bars: speech,
                last_shift: Instant::now(),
                start: at(t),
                surface: None,
            };
            update_bars(&mut v);
            if matches!(mode, Mode::Recording | Mode::Revising) {
                v.bars = speech; // 录音态的波形来自麦克风，这里用固定样例
            }
            let path = dir.join(format!("capsule-{name}.png"));
            draw(&v).unwrap().save_png(&path).unwrap();
            println!("wrote {}", path.display());
        }
    }

    /// 手动运行：cargo test --lib overlay -- --ignored --nocapture
    /// 用一个测试窗口充当「焦点所在窗口」，依次挪到每块屏幕，检查悬浮条跟着换屏
    #[test]
    #[ignore]
    fn follows_focus_across_monitors() {
        unsafe {
            // 与 Tauri 应用一致的 DPI 感知，坐标才是真实物理像素
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let mut mons: Vec<HMONITOR> = Vec::new();
            let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut mons as *mut _ as isize));
            println!("monitors: {}", mons.len());
            assert!(mons.len() >= 2, "需要至少两块屏幕");

            let target = CreateWindowExW(
                Default::default(), w!("STATIC"), w!("overlay follow test"), WS_POPUP,
                0, 0, 200, 100, None, None, None, None,
            )
            .unwrap();
            TEST_TARGET.store(target.0 as isize, Ordering::SeqCst);
            init();
            std::thread::sleep(Duration::from_millis(300));

            for (i, &mon) in mons.iter().enumerate() {
                let wa = work_area(mon);
                SetWindowPos(target, None, wa.left + 100, wa.top + 100, 200, 100, SWP_NOACTIVATE | SWP_NOZORDER).unwrap();
                if i == 0 {
                    set(Mode::Recording);
                }
                std::thread::sleep(Duration::from_millis(800));

                let overlay = HWND(HWND_RAW.load(Ordering::SeqCst) as _);
                let mut r = RECT::default();
                GetWindowRect(overlay, &mut r).unwrap();
                let (mut dpi, mut dpi_y) = (96u32, 96u32);
                let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
                let scale = dpi as f32 / 96.0;
                println!(
                    "screen {i}: work=({},{})-({},{}) scale={scale} overlay=({},{})-({},{})",
                    wa.left, wa.top, wa.right, wa.bottom, r.left, r.top, r.right, r.bottom
                );
                // 在这块屏幕的工作区内、水平居中、底部留出 BOTTOM_GAP
                assert!(r.left >= wa.left && r.right <= wa.right && r.top >= wa.top && r.bottom <= wa.bottom);
                assert!(((r.left + r.right) / 2 - (wa.left + wa.right) / 2).abs() <= 1);
                let gap = (BOTTOM_GAP * scale).round() as i32;
                assert!((wa.bottom - r.bottom - gap).abs() <= 1);
                assert_eq!(r.right - r.left, (WIDTH * scale).round() as i32);
                // 留时间给外部截图
                std::thread::sleep(Duration::from_millis(2500));
            }

            set(Mode::Hidden);
            std::thread::sleep(Duration::from_millis(500));
            let _ = DestroyWindow(target);
        }
    }
}
