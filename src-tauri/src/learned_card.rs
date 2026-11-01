//! 词条入库提示：从用户的修改中学到纠正后，在屏幕右下角弹出小条列出学到的词条。
//! 默认同意时词条已经入库，提供「撤销」；默认拒绝时先不入库，用户点「添加」才加入，关掉或超时就丢弃。
//! 放在角落、做得很小：用户这时多半还在输入框里打字，不能挡住它
//!
//! 和结果卡片一样是不抢焦点的 WebView 窗口；点按钮会激活它，所以关闭时把焦点还给之前的窗口，
//! 用户可以接着打字。页面在约 8 秒后自动关闭（鼠标悬停时暂停）

use crate::learn::Correction;
use crate::platform::popup;
use crate::vocab;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

pub const LABEL: &str = "learned";
/// 逻辑尺寸（含四周 8 像素的阴影留白），按显示器缩放；高度随词条数增加，超过 MAX_ROWS 条时滚动
const WIDTH: f64 = 304.0;
const BASE_HEIGHT: f64 = 52.0;
const ROW_HEIGHT: f64 = 20.0;
const MAX_ROWS: usize = 4;
/// 离屏幕右下角的距离（加上阴影留白，卡片离边缘约 12 像素）
const CORNER_GAP: f64 = 4.0;

fn height(rows: usize) -> f64 {
    BASE_HEIGHT + ROW_HEIGHT * rows.clamp(1, MAX_ROWS) as f64
}

#[derive(Clone, Serialize)]
pub struct Item {
    pub wrong: String,
    pub right: String,
    /// 这个纠正累计学到的次数（≥ 2 之后会直接替换）
    pub count: u32,
}

/// 页面显示的内容
#[derive(Clone, Serialize)]
pub struct View {
    pub items: Vec<Item>,
    /// 默认拒绝：词条还没入库，按钮是「添加」
    pub ask: bool,
}

#[derive(Default)]
pub struct Toast {
    pub items: Vec<Item>,
    /// 默认同意时已经写入词库的改动，「撤销」时回退
    pub changes: Vec<vocab::Change>,
    /// 默认拒绝：items 还没入库，等用户点「添加」
    pub ask: bool,
    /// 弹出前的前台窗口（macOS 上是前台程序的进程号），关闭时把焦点还给它
    pub previous: isize,
}

#[derive(Default)]
pub struct LearnedState(pub Mutex<Toast>);

/// 提示是否开着（决定 Esc 是否用来关闭小窗）
static OPEN: AtomicBool = AtomicBool::new(false);

pub fn is_open() -> bool {
    OPEN.load(Ordering::SeqCst)
}

/// 弹出学到的词条。ask = 默认拒绝：先不入库，等用户点「添加」；否则直接入库，提供撤销
pub fn show(app: &AppHandle, learned: Vec<Correction>, ask: bool) {
    let view = {
        let state = app.state::<LearnedState>();
        let mut toast = state.0.lock().unwrap();
        // 窗口还开着：追加，按钮对窗口里列出的全部生效（沿用这个窗口的方式）
        if app.get_webview_window(LABEL).is_none() {
            *toast = Toast { previous: popup::foreground(), ask, ..Default::default() };
        }
        let mut added = false;
        for c in learned {
            if !toast.ask {
                let Some(change) = vocab::learn(&c.wrong, &c.right) else { continue };
                toast.changes.push(change);
                added = true;
            }
            toast.items.push(Item { count: count(&c.wrong, &c.right), wrong: c.wrong, right: c.right });
        }
        if added {
            let _ = app.emit("vocab-changed", ());
        }
        View { items: toast.items.clone(), ask: toast.ask }
    };
    if view.items.is_empty() {
        return;
    }
    let items = &view.items;

    let window = match app.get_webview_window(LABEL) {
        Some(w) => {
            let _ = w.emit("learned-updated", &view);
            w
        }
        None => match WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("learned.html".into()))
            .title("Aoide")
            .inner_size(WIDTH, height(items.len()))
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
                log::error!("Learned card failed: {e}");
                return;
            }
        },
    };
    OPEN.store(true, Ordering::SeqCst);
    crate::refresh_dismiss_key();

    // 输入焦点所在屏幕的右下角
    popup::show_corner(&window, WIDTH, height(items.len()), CORNER_GAP);
}

/// 词库里这个纠正累计学到的次数；还没入库时为 1
fn count(wrong: &str, right: &str) -> u32 {
    vocab::all()
        .iter()
        .find(|e| e.term == right)
        .and_then(|e| e.variants.iter().find(|v| v.text == wrong))
        .map_or(1, |v| v.count)
}

pub fn view(app: &AppHandle) -> View {
    let state = app.state::<LearnedState>();
    let toast = state.0.lock().unwrap();
    View { items: toast.items.clone(), ask: toast.ask }
}

/// 默认拒绝时用户点了「添加」：把窗口里列出的全部入库
pub fn add(app: &AppHandle) {
    let items = {
        let state = app.state::<LearnedState>();
        let mut toast = state.0.lock().unwrap();
        if !toast.ask {
            return;
        }
        toast.ask = false;
        let items = toast.items.clone();
        toast.changes = items.iter().filter_map(|i| vocab::learn(&i.wrong, &i.right)).collect();
        items
    };
    log::info!("Learn: added {} suggested correction(s)", items.len());
    let _ = app.emit("vocab-changed", ());
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

/// 关闭窗口，把焦点还给弹出前的窗口；默认拒绝时没点「添加」的词条就此丢弃
pub fn close(app: &AppHandle) {
    let toast = std::mem::take(&mut *app.state::<LearnedState>().0.lock().unwrap());
    if toast.ask && !toast.items.is_empty() {
        log::info!("Learn: {} suggested correction(s) dismissed", toast.items.len());
    }
    let previous = toast.previous;
    // 先还焦点再关窗口：关闭是异步的，若先关，窗口销毁时系统会把激活转给 Z 序里的下一个窗口
    if previous != 0 {
        popup::restore_foreground(previous);
    }
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.close();
    }
}
