//! 剪贴板读写、粘贴输出、获取选中文本

use crate::input;
use crate::platform::clipboard_sequence;
use std::time::{Duration, Instant};

pub fn get_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

pub fn set_text(text: &str) -> bool {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(text.to_string()))
        .map_err(|e| log::warn!("Clipboard write failed: {e}"))
        .is_ok()
}

/// 通过剪贴板粘贴输出，并在之后恢复原剪贴板文本。
///
/// 不用逐字符注入：Telegram 等自绘输入框对每个字符事件跑智能标点/emoji
/// 替换逻辑，事件洪流会和缓冲区改写互相踩踏，导致标点多打、文字少打；
/// 粘贴是一次原子写入，没有逐键处理。（原剪贴板是图片等非文本时无法恢复）
pub fn paste_text(text: &str) -> bool {
    let old = get_text();
    if !set_text(text) {
        return false;
    }
    std::thread::sleep(Duration::from_millis(50));
    input::shortcut('v');
    log::info!("Pasted: {text}");
    if let Some(old) = old.filter(|o| o != text) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            set_text(&old);
        });
    }
    true
}

/// 通过 Ctrl+C（macOS 上是 Command+C）获取当前选中的文本，之后恢复原剪贴板。
/// 用剪贴板序列号判断复制是否真的复制了东西，没选中文本时不会误把旧剪贴板当成选区。
pub fn get_selected_text() -> String {
    let old = get_text();
    let seq = clipboard_sequence();
    input::shortcut('c');
    let start = Instant::now();
    while clipboard_sequence() == seq {
        if start.elapsed() > Duration::from_millis(400) {
            return String::new();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(30)); // 等复制方写完
    let selected = get_text().unwrap_or_default();
    if let Some(old) = old.filter(|o| !o.is_empty()) {
        set_text(&old);
    }
    selected.trim().to_string()
}
