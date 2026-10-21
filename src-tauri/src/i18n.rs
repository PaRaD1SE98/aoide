//! 界面语言（中文 / English / 日本語）
//!
//! 后端面向用户的文字（托盘、提示、错误信息、结果卡片标题）在调用处用 tr / trf! 依次给出中、英、日三个版本；
//! 设置页等前端文字见 ui/i18n.js。

use std::sync::atomic::{AtomicU8, Ordering};

/// 支持的语言，顺序与 tr / trf! 的参数顺序一致
const LANGS: [&str; 3] = ["zh", "en", "ja"];

/// 当前语言在 LANGS 里的位置
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// 系统显示语言：zh / en / ja（其他语言按 en）
pub fn system_language() -> &'static str {
    crate::platform::system_language()
}

/// 按配置（auto / zh / en / ja）设置当前语言
pub fn set_language(setting: &str) {
    let lang = if LANGS.contains(&setting) { setting } else { system_language() };
    let index = LANGS.iter().position(|&l| l == lang).unwrap_or(1);
    CURRENT.store(index as u8, Ordering::SeqCst);
}

/// 当前语言：zh / en / ja
pub fn current() -> &'static str {
    LANGS[CURRENT.load(Ordering::SeqCst) as usize]
}

/// 按当前语言三选一
pub fn tr(zh: &'static str, en: &'static str, ja: &'static str) -> &'static str {
    [zh, en, ja][CURRENT.load(Ordering::SeqCst) as usize]
}

/// 带格式化参数的 tr：trf!("中文 {x}", "English {x}", "日本語 {x}")
#[macro_export]
macro_rules! trf {
    ($zh:literal, $en:literal, $ja:literal $(, $arg:expr)* $(,)?) => {
        match $crate::i18n::current() {
            "en" => format!($en $(, $arg)*),
            "ja" => format!($ja $(, $arg)*),
            _ => format!($zh $(, $arg)*),
        }
    };
}
