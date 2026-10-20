//! 结果卡片：结束录音时焦点不在输入框（无处可粘贴），把识别结果显示在
//! 输入焦点所在屏幕底部的卡片里，提供复制 / 关闭
//!
//! 卡片要能点按钮、选中文字，所以是普通 WebView 窗口（不像录音悬浮条那样鼠标穿透）。
//! 显示时不抢焦点（见各平台的 popup.rs）；Windows 上进程启动时已调用 DisableProcessWindowsGhosting，
//! 界面卡顿也不会冒出系统外壳。

use crate::platform::popup;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

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

/// 卡片是否开着（决定 Esc 是否用来关闭小窗）
static OPEN: AtomicBool = AtomicBool::new(false);

pub fn is_open() -> bool {
    OPEN.load(Ordering::SeqCst)
}

pub fn close(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.close();
    }
}

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
            .accept_first_mouse(true)
            .visible_on_all_workspaces(true)
            .build()
        {
            Ok(w) => {
                w.on_window_event(|e| {
                    if let tauri::WindowEvent::Destroyed = e {
                        OPEN.store(false, Ordering::SeqCst);
                        crate::refresh_dismiss_key();
                    }
                });
                w
            }
            Err(e) => {
                log::error!("Result card failed: {e}");
                return;
            }
        },
    };
    OPEN.store(true, Ordering::SeqCst);
    crate::refresh_dismiss_key();

    // 输入焦点所在屏幕的底部中间
    popup::show(&window, WIDTH, HEIGHT, BOTTOM_GAP);
}
