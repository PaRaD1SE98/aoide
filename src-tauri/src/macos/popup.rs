//! 屏幕底部的小窗（结果卡片、词条入库提示）：定位、不抢焦点地显示、关闭时把焦点还回去（macOS）

use dispatch2::DispatchQueue;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use std::sync::Mutex;
use std::time::Duration;
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

/// 各窗口的有效形状（窗口标签，逻辑像素的矩形，相对窗口左上角）
static SHAPES: Mutex<Vec<(String, Vec<[f64; 4]>)>> = Mutex::new(Vec::new());

/// 窗口的有效形状：只有这些矩形会接收鼠标，其余透明部分的点击落到下面的程序。
/// macOS 没有窗口区域，WebView 窗口的透明部分也会接住点击：每 40 ms 看一次鼠标，
/// 不在有效形状里时让窗口忽略鼠标（只改这个，不改窗口大小，不会闪烁）
pub fn set_shape(window: &WebviewWindow, rects: &[[f64; 4]]) {
    let label = window.label().to_string();
    let mut shapes = SHAPES.lock().unwrap();
    let first = !shapes.iter().any(|(l, _)| *l == label);
    shapes.retain(|(l, _)| *l != label);
    shapes.push((label.clone(), rects.to_vec()));
    drop(shapes);
    if first {
        let window = window.clone();
        std::thread::spawn(move || follow_cursor(window, label));
    }
}

/// 跟着鼠标切换窗口是否忽略鼠标，直到窗口关闭
fn follow_cursor(window: WebviewWindow, label: String) {
    let mut ignoring = None;
    loop {
        std::thread::sleep(Duration::from_millis(40));
        // 窗口关闭后这些调用会失败
        let (Ok(origin), Ok(cursor), Ok(scale)) = (window.inner_position(), window.cursor_position(), window.scale_factor()) else {
            break;
        };
        let (x, y) = ((cursor.x - origin.x as f64) / scale, (cursor.y - origin.y as f64) / scale);
        let inside = SHAPES
            .lock()
            .unwrap()
            .iter()
            .find(|(l, _)| *l == label)
            .is_some_and(|(_, rects)| rects.iter().any(|r| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]));
        if ignoring != Some(!inside) {
            if window.set_ignore_cursor_events(!inside).is_err() {
                break;
            }
            ignoring = Some(!inside);
        }
    }
    SHAPES.lock().unwrap().retain(|(l, _)| *l != label);
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
