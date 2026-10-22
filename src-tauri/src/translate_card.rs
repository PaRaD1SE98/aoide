//! 语音翻译的小窗，两种样子：
//! - 选语言：翻译录音时贴在悬浮条上方，显示目标语言，点开可以搜索、选择或直接填写
//! - 核对：翻译输出后显示在悬浮条的位置，列出识别出的原文（可以修改）和译文；
//!   改完点「重新翻译」，把刚才粘贴进输入框的译文换成新的（见 Controller::retranslate）
//!
//! 悬浮条本身是鼠标穿透的原生窗口，点不了也打不了字，所以这里用一个 WebView 小窗。
//! 和结果卡片一样显示时不抢焦点；用户点进来（选语言、改原文）后，再把焦点还给原来的输入框

use crate::controller::Controller;
use crate::overlay;
use crate::platform::popup;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

pub const LABEL: &str = "translate";
/// 逻辑尺寸（含四周 8 的阴影留白），按显示器缩放
const PICKER_WIDTH: f64 = 280.0;
const PICKER_HEIGHT: f64 = 60.0;
/// 候选语言列表展开时，选语言的小窗向上长出的高度
const PICKER_LIST_HEIGHT: f64 = 232.0;
const REVIEW_WIDTH: f64 = 480.0;
pub const REVIEW_HEIGHT: f64 = 320.0;
const BOTTOM_GAP: f64 = 8.0;

#[derive(Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Picker,
    Review,
}

/// 小窗显示的内容
#[derive(Clone, Default, Serialize)]
pub struct View {
    pub mode: Mode,
    pub language: String,
    /// 识别出的原文
    pub original: String,
    pub translation: String,
    /// 译文已粘贴进输入框（重新翻译时会替换它）；否则只显示在小窗里，由用户复制
    pub pasted: bool,
    /// 翻译失败的原因
    pub error: String,
}

#[derive(Default)]
pub struct Session {
    pub view: View,
    /// 开始翻译时的前台窗口（macOS 上是前台程序的进程号）：选完语言、重新翻译后把焦点还给它
    pub target: isize,
}

#[derive(Default)]
pub struct TranslateState(pub Mutex<Session>);

/// 核对小窗开着（提示小窗据此放到它上方，Esc 用来关闭它）
static REVIEW_OPEN: AtomicBool = AtomicBool::new(false);

pub fn is_review_open() -> bool {
    REVIEW_OPEN.load(Ordering::SeqCst)
}

/// 翻译录音开始：在悬浮条上方显示目标语言
pub fn show_picker(app: &AppHandle, language: &str) {
    let target = popup::foreground();
    {
        let state = app.state::<TranslateState>();
        let mut s = state.0.lock().unwrap();
        s.target = target;
        s.view = View { mode: Mode::Picker, language: language.to_string(), ..View::default() };
    }
    REVIEW_OPEN.store(false, Ordering::SeqCst);
    crate::refresh_dismiss_key();
    let Some(window) = window(app) else { return };
    place_picker(&window, false);
}

/// 贴在悬浮条正上方（悬浮条离屏幕底部 BOTTOM_GAP、高 HEIGHT；小窗四周有 8 的阴影留白）；
/// 展开候选列表时底边不动，向上长高
fn place_picker(window: &tauri::WebviewWindow, list_open: bool) {
    let gap = (overlay::BOTTOM_GAP + overlay::HEIGHT) as f64 - 2.0;
    let height = if list_open { PICKER_HEIGHT + PICKER_LIST_HEIGHT } else { PICKER_HEIGHT };
    popup::show(window, PICKER_WIDTH, height, gap);
}

/// 翻译完成（或失败）：在悬浮条的位置显示原文和译文，供核对、修改后重新翻译
pub fn show_review(app: &AppHandle, view: View) {
    app.state::<TranslateState>().0.lock().unwrap().view = View { mode: Mode::Review, ..view };
    REVIEW_OPEN.store(true, Ordering::SeqCst);
    crate::refresh_dismiss_key();
    let Some(window) = window(app) else { return };
    popup::show(&window, REVIEW_WIDTH, REVIEW_HEIGHT, BOTTOM_GAP);
    crate::notice::make_room(app);
}

/// 收起选语言的小窗（翻译录音被取消时）
pub fn close_picker(app: &AppHandle) {
    if !is_review_open() {
        close(app);
    }
}

pub fn close(app: &AppHandle) {
    let target = app.state::<TranslateState>().0.lock().unwrap().target;
    if let Some(w) = app.get_webview_window(LABEL) {
        // 用户点过小窗（它成了焦点窗口）时把焦点还给原来的输入框
        if target != 0 && w.is_focused().unwrap_or(false) {
            popup::restore_foreground(target);
        }
        let _ = w.close();
    }
}

/// 取得小窗（没有就新建），并让页面按当前内容刷新
fn window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    let view = app.state::<TranslateState>().0.lock().unwrap().view.clone();
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.emit("translate-updated", &view);
        return Some(w);
    }
    match WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("translate.html".into()))
        .title("Aoide")
        .inner_size(PICKER_WIDTH, PICKER_HEIGHT)
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
                    REVIEW_OPEN.store(false, Ordering::SeqCst);
                    crate::refresh_dismiss_key();
                }
            });
            Some(w)
        }
        Err(e) => {
            log::error!("Translate window failed: {e}");
            None
        }
    }
}

/// 把焦点还给开始翻译时的输入框
pub fn restore_target(app: &AppHandle) {
    let target = app.state::<TranslateState>().0.lock().unwrap().target;
    if target != 0 {
        popup::restore_foreground(target);
    }
}

// === 小窗页面调用的命令 ===

#[tauri::command]
pub fn get_translate(state: State<TranslateState>) -> View {
    state.0.lock().unwrap().view.clone()
}

/// 选定目标语言：存进设置；在选语言的小窗里选的，选完把焦点还给输入框
#[tauri::command]
pub fn set_translate_language(app: AppHandle, ctl: State<Arc<Controller>>, language: String, restore_focus: bool) -> Result<(), String> {
    let language = language.trim().to_string();
    if language.is_empty() {
        return Err(crate::i18n::tr("请填写目标语言", "Enter a target language", "翻訳先の言語を入力してください").into());
    }
    ctl.set_translate_language(&language)?;
    app.state::<TranslateState>().0.lock().unwrap().view.language = language;
    if restore_focus {
        restore_target(&app);
    }
    Ok(())
}

/// 按修改后的原文重新翻译；译文已粘贴进输入框时把它换成新的。返回新的译文
#[tauri::command]
pub async fn retranslate(ctl: State<'_, Arc<Controller>>, original: String, language: String) -> Result<String, String> {
    let ctl = ctl.inner().clone();
    tauri::async_runtime::spawn_blocking(move || ctl.retranslate(&original, &language))
        .await
        .map_err(|e| e.to_string())?
}

/// 选语言的小窗展开 / 收起候选列表
#[tauri::command]
pub fn resize_translate_picker(app: AppHandle, open: bool) {
    let picker = app.state::<TranslateState>().0.lock().unwrap().view.mode == Mode::Picker;
    if let (true, Some(w)) = (picker, app.get_webview_window(LABEL)) {
        place_picker(&w, open);
    }
}

#[tauri::command]
pub fn copy_translation(state: State<TranslateState>) -> bool {
    let text = state.0.lock().unwrap().view.translation.clone();
    crate::clipboard::set_text(&text)
}

#[tauri::command]
pub fn close_translate(app: AppHandle) {
    close(&app);
}
