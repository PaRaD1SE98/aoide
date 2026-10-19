//! 结果卡片：结束录音时焦点不在输入框（无处可粘贴），把识别结果显示在
//! 输入焦点所在屏幕底部的卡片里，提供复制 / 关闭
//!
//! 卡片要能点按钮、选中文字，所以是普通 WebView 窗口（不像录音悬浮条那样鼠标穿透）。
//! 显示时不抢焦点；进程启动时已调用 DisableProcessWindowsGhosting，界面卡顿也不会冒出系统外壳。

use crate::overlay;
use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder};
use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNOACTIVATE};

pub const LABEL: &str = "result";
/// 逻辑尺寸（含阴影留白），按显示器缩放
const WIDTH: f64 = 440.0;
const HEIGHT: f64 = 220.0;
const BOTTOM_GAP: f64 = 8.0;

#[derive(Clone, Default, Serialize)]
pub struct CardContent {
    pub title: String,
    pub text: String,
}

/// 当前卡片内容，页面加载时通过 get_result 命令读取
#[derive(Default)]
pub struct ResultState(pub Mutex<CardContent>);

pub fn show(app: &AppHandle, title: &str, text: &str) {
    let content = CardContent { title: title.into(), text: text.into() };
    *app.state::<ResultState>().0.lock().unwrap() = content.clone();

    let window = match app.get_webview_window(LABEL) {
        Some(w) => {
            let _ = w.emit("result-updated", &content); // 已打开：就地更新
            w
        }
        None => match WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("result.html".into()))
            .title("Aoide")
            .inner_size(WIDTH, HEIGHT)
            .decorations(false)
            .transparent(true)
            .shadow(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .focused(false)
            .visible(false)
            .build()
        {
            Ok(w) => w,
            Err(e) => {
                log::error!("Result card failed: {e}");
                return;
            }
        },
    };

    // 输入焦点所在屏幕的底部中间
    let (work, scale) = overlay::focus_work_area();
    let (w, h) = ((WIDTH * scale).round() as i32, (HEIGHT * scale).round() as i32);
    let x = work.left + (work.right - work.left - w) / 2;
    let y = work.bottom - h - (BOTTOM_GAP * scale).round() as i32;
    // 先移到目标屏幕再定尺寸，避免跨 DPI 移动时被系统按旧缩放改尺寸
    let _ = window.set_position(PhysicalPosition::new(x, y));
    let _ = window.set_size(PhysicalSize::new(w as u32, h as u32));
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE); // 不抢走当前窗口的焦点
        }
    }
}
