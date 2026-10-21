//! 屏幕底部的小窗（结果卡片、词条入库提示）：定位、不抢焦点地显示、关闭时把焦点还回去（Windows）

use super::overlay;
use tauri::{PhysicalPosition, PhysicalSize, WebviewWindow};
use windows::Win32::Foundation::HWND;
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
