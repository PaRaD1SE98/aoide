//! 界面语言（中文 / English）
//!
//! 后端面向用户的文字（托盘、通知、错误提示、结果卡片标题）在调用处用 tr / trf! 给出中英两个版本；
//! 设置页等前端文字见 ui/i18n.js。

use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Globalization::GetUserDefaultUILanguage;

static ENGLISH: AtomicBool = AtomicBool::new(false);

/// Windows 显示语言是否为中文
pub fn system_language() -> &'static str {
    const LANG_CHINESE: u16 = 0x04;
    if unsafe { GetUserDefaultUILanguage() } & 0x3ff == LANG_CHINESE {
        "zh"
    } else {
        "en"
    }
}

/// 按配置（auto / zh / en）设置当前语言
pub fn set_language(setting: &str) {
    let lang = match setting {
        "zh" | "en" => setting,
        _ => system_language(),
    };
    ENGLISH.store(lang == "en", Ordering::SeqCst);
}

pub fn is_en() -> bool {
    ENGLISH.load(Ordering::SeqCst)
}

/// 中英文二选一
pub fn tr(zh: &'static str, en: &'static str) -> &'static str {
    if is_en() {
        en
    } else {
        zh
    }
}

/// 带格式化参数的 tr：trf!("中文 {x}", "English {x}")
#[macro_export]
macro_rules! trf {
    ($zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::is_en() { format!($en $(, $arg)*) } else { format!($zh $(, $arg)*) }
    };
}
