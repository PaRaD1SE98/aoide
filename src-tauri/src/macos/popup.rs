//! 屏幕底部的小窗（结果卡片、词条入库提示）：定位、不抢焦点地显示、关闭时把焦点还回去（macOS）

use dispatch2::DispatchQueue;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use tauri::{LogicalPosition, LogicalSize, WebviewWindow};

const ACTIVATE_IGNORING_OTHER_APPS: usize = 1 << 1;

/// 放到输入焦点所在屏幕的底部中间（逻辑尺寸），显示但不抢焦点
pub fn show(window: &WebviewWindow, width: f64, height: f64, bottom_gap: f64) {
    if let Some((screen, primary_height)) = super::focus_screen() {
        let visible = screen.visible;
        let x = visible.origin.x + (visible.size.width - width) / 2.0;
        // Tauri 的窗口坐标以主屏左上角为原点、y 轴向下
        let top = primary_height - (visible.origin.y + bottom_gap + height);
        let _ = window.set_size(LogicalSize::new(width, height));
        let _ = window.set_position(LogicalPosition::new(x.round(), top.round()));
    }
    // Tauri 的 show() 会让窗口成为焦点窗口、激活 Aoide；只把窗口放到最前
    let Ok(ns_window) = window.ns_window() else { return };
    let ns_window = ns_window as usize;
    DispatchQueue::main().exec_async(move || unsafe {
        let _: () = msg_send![ns_window as *mut AnyObject, orderFrontRegardless];
    });
}

/// 当前前台程序的进程号
pub fn foreground() -> isize {
    unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let app: *mut AnyObject = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return 0;
        }
        let pid: i32 = msg_send![app, processIdentifier];
        pid as isize
    }
}

/// 把焦点还给之前记下的前台程序
pub fn restore_foreground(previous: isize) {
    unsafe {
        let app: *mut AnyObject =
            msg_send![class!(NSRunningApplication), runningApplicationWithProcessIdentifier: previous as i32];
        if app.is_null() {
            return;
        }
        let ok: bool = msg_send![app, activateWithOptions: ACTIVATE_IGNORING_OTHER_APPS];
        log::info!("Restore focus to pid {previous}: {ok}");
    }
}
