//! 屏幕底部的小窗（结果卡片、提示、词条入库提示）：定位、不抢焦点地显示、关闭时把焦点还回去（Windows）

use super::overlay;
use tauri::{PhysicalPosition, PhysicalSize, WebviewWindow};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{CombineRgn, CreateRectRgn, DeleteObject, SetWindowRgn, RGN_OR};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, SetForegroundWindow, SetWindowPos, ShowWindow, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNOACTIVATE,
};

/// 放到输入焦点所在屏幕的底部中间（逻辑尺寸，按显示器缩放），显示但不抢焦点
pub fn show(window: &WebviewWindow, width: f64, height: f64, bottom_gap: f64) {
    let (work, scale) = overlay::focus_work_area();
    let (w, h) = ((width * scale).round() as i32, (height * scale).round() as i32);
    let x = work.left + (work.right - work.left - w) / 2;
    let y = work.bottom - h - (bottom_gap * scale).round() as i32;
    present(window, x, y, w, h);
}

/// 放到输入焦点所在屏幕的右下角，离边缘 gap（逻辑尺寸），显示但不抢焦点：不挡住屏幕中间的输入框
pub fn show_corner(window: &WebviewWindow, width: f64, height: f64, gap: f64) {
    let (work, scale) = overlay::focus_work_area();
    let (w, h) = ((width * scale).round() as i32, (height * scale).round() as i32);
    let gap = (gap * scale).round() as i32;
    present(window, work.right - w - gap, work.bottom - h - gap, w, h);
}

/// 放到 (x, y)、定好尺寸（物理像素），显示但不抢焦点
fn present(window: &WebviewWindow, x: i32, y: i32, w: i32, h: i32) {
    // 先移到目标屏幕再定尺寸，避免跨 DPI 移动时被系统按旧缩放改尺寸
    let _ = window.set_position(PhysicalPosition::new(x, y));
    let _ = window.set_size(PhysicalSize::new(w as u32, h as u32));
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            // tao 的无边框窗口保留着标题栏样式，靠 WM_NCCALCSIZE 把边框算成 0；用原生 ShowWindow 显示时
            // 边框不会重新计算，系统标题栏会露出来。强制重算一次（不抢走当前窗口的焦点）
            let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
}

/// 窗口的有效形状：只有这些矩形（逻辑像素，相对窗口左上角）会显示、接收鼠标，其余部分的点击落到下面的程序。
/// 用窗口区域实现：只改形状、不改窗口大小，WebView 不用重新排版，不会闪烁
pub fn set_shape(window: &WebviewWindow, rects: &[[f64; 4]]) {
    let Ok(hwnd) = window.hwnd() else { return };
    let scale = window.scale_factor().unwrap_or(1.0);
    let px = |v: f64| (v * scale).round() as i32;
    unsafe {
        let region = CreateRectRgn(0, 0, 0, 0);
        for r in rects {
            let part = CreateRectRgn(px(r[0]), px(r[1]), px(r[0] + r[2]), px(r[1] + r[3]));
            CombineRgn(Some(region), Some(region), Some(part), RGN_OR);
            let _ = DeleteObject(part.into());
        }
        // 区域交给系统管理，之后不能再释放
        SetWindowRgn(hwnd, Some(region), true);
    }
}

/// 当前的前台窗口
pub fn foreground() -> isize {
    unsafe { GetForegroundWindow() }.0 as isize
}

/// 把焦点还给之前记下的前台窗口
pub fn restore_foreground(previous: isize) {
    unsafe {
        let ok = SetForegroundWindow(HWND(previous as _)).as_bool();
        log::info!("Restore focus to {previous:#x}: {ok}, foreground now {:#x}", GetForegroundWindow().0 as isize);
    }
}
