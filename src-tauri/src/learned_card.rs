//! 词条入库提示：从用户的修改中学到纠正后，在屏幕底部弹出小窗列出学到的词条，提供「撤销」
//!
//! 和结果卡片一样是不抢焦点的 WebView 窗口；点按钮会激活它，所以关闭时把焦点还给之前的窗口，
//! 用户可以接着打字。页面在约 8 秒后自动关闭（鼠标悬停时暂停）

use crate::learn::Correction;
use crate::{overlay, vocab};
use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow, SetWindowPos, ShowWindow, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNOACTIVATE};

pub const LABEL: &str = "learned";
/// 逻辑尺寸（含阴影留白），按显示器缩放
const WIDTH: f64 = 400.0;
const HEIGHT: f64 = 156.0;
const BOTTOM_GAP: f64 = 8.0;

#[derive(Clone, Serialize)]
pub struct Item {
    pub wrong: String,
    pub right: String,
    /// 这个纠正累计学到的次数（≥ 2 之后会直接替换）
    pub count: u32,
}

#[derive(Default)]
pub struct Toast {
    pub items: Vec<Item>,
    pub changes: Vec<vocab::Change>,
    /// 弹出前的前台窗口，关闭时把焦点还给它
    pub previous: isize,
}

#[derive(Default)]
pub struct LearnedState(pub Mutex<Toast>);

pub fn show(app: &AppHandle, learned: Vec<(Correction, vocab::Change)>) {
    let entries = vocab::all();
    let count = |wrong: &str, right: &str| {
        entries
            .iter()
            .find(|e| e.term == right)
            .and_then(|e| e.variants.iter().find(|v| v.text == wrong))
            .map_or(1, |v| v.count)
    };
    let items = {
        let state = app.state::<LearnedState>();
        let mut toast = state.0.lock().unwrap();
        // 窗口还开着：追加，「撤销」对窗口里列出的全部生效
        if app.get_webview_window(LABEL).is_none() {
            *toast = Toast { previous: unsafe { GetForegroundWindow() }.0 as isize, ..Default::default() };
        }
        for (c, change) in learned {
            toast.items.push(Item { count: count(&c.wrong, &c.right), wrong: c.wrong, right: c.right });
            toast.changes.push(change);
        }
        toast.items.clone()
    };

    let window = match app.get_webview_window(LABEL) {
        Some(w) => {
            let _ = w.emit("learned-updated", &items);
            w
        }
        None => match WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("learned.html".into()))
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
                log::error!("Learned card failed: {e}");
                return;
            }
        },
    };

    // 输入焦点所在屏幕的底部中间
    let (work, scale) = overlay::focus_work_area();
    let (w, h) = ((WIDTH * scale).round() as i32, (HEIGHT * scale).round() as i32);
    let x = work.left + (work.right - work.left - w) / 2;
    let y = work.bottom - h - (BOTTOM_GAP * scale).round() as i32;
    let _ = window.set_position(PhysicalPosition::new(x, y));
    let _ = window.set_size(PhysicalSize::new(w as u32, h as u32));
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            // tao 的无边框窗口保留着标题栏样式，靠 WM_NCCALCSIZE 把边框算成 0；用原生 ShowWindow 显示时
            // 边框不会重新计算，系统标题栏会露出来。强制重算一次
            let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
}

pub fn items(app: &AppHandle) -> Vec<Item> {
    app.state::<LearnedState>().0.lock().unwrap().items.clone()
}

/// 撤销窗口里列出的全部学习
pub fn undo(app: &AppHandle) {
    let changes = std::mem::take(&mut app.state::<LearnedState>().0.lock().unwrap().changes);
    if !changes.is_empty() {
        vocab::undo(&changes);
        log::info!("Learn: undone {} change(s)", changes.len());
        let _ = app.emit("vocab-changed", ());
    }
}

/// 关闭窗口，把焦点还给弹出前的窗口
pub fn close(app: &AppHandle) {
    let previous = std::mem::take(&mut *app.state::<LearnedState>().0.lock().unwrap()).previous;
    // 先还焦点再关窗口：关闭是异步的，若先关，窗口销毁时系统会把激活转给 Z 序里的下一个窗口
    if previous != 0 {
        unsafe {
            let ok = SetForegroundWindow(HWND(previous as _)).as_bool();
            log::info!("Learned card: restore focus to {previous:#x}: {ok}, foreground now {:#x}", GetForegroundWindow().0 as isize);
        }
    }
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.close();
    }
}
