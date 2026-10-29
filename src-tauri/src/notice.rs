//! 提示小窗：代替系统通知，在输入焦点所在屏幕底部显示一条消息（出错、没录到声音、有新版本……），
//! 几秒后自动关闭（见 ui/notice.js）
//!
//! 不用系统通知：macOS 上要先申请权限，而且 Aoide 在前台时通知会被系统丢掉；Windows 上用户也可能关掉了通知。
//! 和结果卡片、词条入库提示一样是不抢焦点的 WebView 小窗；底部已有这两种小窗时放在它们上方

use crate::platform::popup;
use crate::{result_card, translate_card};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

pub const LABEL: &str = "notice";
/// 逻辑尺寸（含阴影留白），按显示器缩放
const WIDTH: f64 = 400.0;
const HEIGHT: f64 = 132.0;
const BOTTOM_GAP: f64 = 8.0;

#[derive(Default)]
pub struct Notice {
    pub message: String,
    /// 弹出前的前台窗口（macOS 上是前台程序的进程号），点过小窗后关闭时把焦点还给它
    previous: isize,
}

#[derive(Default)]
pub struct NoticeState(pub Mutex<Notice>);

/// 小窗是否开着（决定 Esc 是否用来关闭小窗）
static OPEN: AtomicBool = AtomicBool::new(false);

pub fn is_open() -> bool {
    OPEN.load(Ordering::SeqCst)
}

pub fn message(app: &AppHandle) -> String {
    app.state::<NoticeState>().0.lock().unwrap().message.clone()
}

pub fn show(app: &AppHandle, message: &str) {
    log::info!("Notice: {message}");
    let window = app.get_webview_window(LABEL);
    {
        let state = app.state::<NoticeState>();
        let mut notice = state.0.lock().unwrap();
        if window.is_none() {
            notice.previous = popup::foreground();
        }
        notice.message = message.to_string();
    }

    let window = match window {
        Some(w) => {
            let _ = w.emit("notice-updated", message); // 已打开：就地更新
            w
        }
        None => match WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("notice.html".into()))
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
                log::error!("Notice window failed: {e}");
                return;
            }
        },
    };
    OPEN.store(true, Ordering::SeqCst);
    crate::refresh_dismiss_key();
    place(&window);
}

/// 放到输入焦点所在屏幕的底部中间；那里已有翻译核对或结果卡片时，放在它上方
fn place(window: &tauri::WebviewWindow) {
    let below = if translate_card::is_review_open() {
        translate_card::REVIEW_HEIGHT
    } else if result_card::is_open() {
        result_card::HEIGHT
    } else {
        0.0
    };
    popup::show(window, WIDTH, HEIGHT, BOTTOM_GAP + below);
}

/// 翻译核对、结果卡片弹出后调用：提示窗还开着时挪到它们上方，避免重叠
pub fn make_room(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        place(&w);
    }
}

/// 关闭小窗；用户点过它（小窗成了焦点窗口）时把焦点还给之前的窗口
pub fn close(app: &AppHandle) {
    let previous = std::mem::take(&mut *app.state::<NoticeState>().0.lock().unwrap()).previous;
    if let Some(w) = app.get_webview_window(LABEL) {
        if previous != 0 && w.is_focused().unwrap_or(false) {
            popup::restore_foreground(previous);
        }
        let _ = w.close();
    }
}
