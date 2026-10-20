//! 悬浮条窗口（Windows）
//!
//! 不用 WebView 窗口：Tauri（tao）的「无边框」窗口在 Windows 上仍带 WS_CAPTION 等样式，
//! 只是靠处理 WM_NCCALCSIZE 把边框藏起来；所在线程一卡（几秒不处理消息），系统的
//! 窗口幽灵化（ghosting）机制就会接管并画出带关闭按钮的默认外壳。这里改用纯 WS_POPUP
//! 分层窗口：根本没有非客户区可画；在独立线程里跑自己的消息循环，主线程卡顿也不影响；
//! 用 UpdateLayeredWindow 逐像素透明输出。
//! 窗口不抢焦点（WS_EX_NOACTIVATE）、鼠标穿透（WS_EX_TRANSPARENT），不影响打字和粘贴。

use crate::overlay::{self, Anim, BOTTOM_GAP, FOLLOW_MS, FRAME_MS, HEIGHT, WIDTH};
use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Instant;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
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
    SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WM_APP, WM_MOUSEACTIVATE,
    WM_NCHITTEST, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const WM_APP_UPDATE: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);

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

/// 状态变了：通知悬浮窗线程（任意线程可调用）
pub fn refresh() {
    let raw = HWND_RAW.load(Ordering::SeqCst);
    if raw != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as _)), WM_APP_UPDATE, WPARAM(0), LPARAM(0));
        }
    }
}

/// 只在悬浮窗线程里访问的窗口状态
struct View {
    anim: Anim,
    pos: POINT,
    /// 当前所在显示器（HMONITOR）
    monitor: isize,
    last_follow: Instant,
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
        anim: Anim::new(),
        pos: POINT::default(),
        monitor: 0,
        last_follow: Instant::now(),
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
    if v.anim.show() {
        place(v, focus_monitor());
        render(hwnd, v);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            // 每次显示都重新置顶，压过后来出现的置顶窗口
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
            SetTimer(Some(hwnd), TIMER_ID, FRAME_MS, None);
        }
    }
}

fn on_frame(hwnd: HWND, v: &mut View) {
    if !v.anim.frame() {
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_ID);
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        return;
    }
    // 跟随输入焦点换屏
    if overlay::current() != overlay::Mode::Hidden && v.last_follow.elapsed().as_millis() >= FOLLOW_MS {
        v.last_follow = Instant::now();
        let mon = focus_monitor();
        if mon.0 as isize != v.monitor {
            place(v, mon);
        }
    }
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
pub fn focus_work_area() -> (RECT, f64) {
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
        v.anim.scale = dpi_x as f32 / 96.0;
        let work = info.rcWork;
        let w = (WIDTH * v.anim.scale).round() as i32;
        let h = (HEIGHT * v.anim.scale).round() as i32;
        v.pos = POINT {
            x: work.left + (work.right - work.left - w) / 2,
            y: work.bottom - h - (BOTTOM_GAP * v.anim.scale).round() as i32,
        };
    }
}

fn render(hwnd: HWND, v: &mut View) {
    let Some(pm) = v.anim.draw() else { return };
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
            SourceConstantAlpha: (v.anim.alpha * 255.0) as u8,
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
    use crate::overlay::Mode;
    use std::time::Duration;
    use windows::core::BOOL;
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
                    overlay::set(Mode::Recording);
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

            overlay::set(Mode::Hidden);
            std::thread::sleep(Duration::from_millis(500));
            let _ = DestroyWindow(target);
        }
    }
}
