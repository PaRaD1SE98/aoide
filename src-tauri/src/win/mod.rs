//! Windows 平台实现。与 macos/mod.rs 提供相同的接口，其他模块统一通过 crate::platform 调用

mod com;
pub mod focus;
pub mod hook;
pub mod input;
pub mod mute;
pub mod overlay;
pub mod popup;
pub mod sound;

use tauri::{App, AppHandle};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;

/// 不加载这些 Vulkan 隐式层：它们会注入所有使用 Vulkan 的程序。Aoide 只用 Vulkan 做计算（whisper.cpp），
/// 游戏叠加层对它毫无用处，而腾讯 WeGame 的叠加层每次加载都会在当前目录写一个加密的 .log 文件。
/// 不整体禁用隐式层：双显卡笔记本上用于选择显卡的驱动层也是隐式层
const DISABLED_VULKAN_LAYERS: &str = "VK_LAYER_TENCENT_wegame_cross_overlay";

/// 进程启动时最先调用：必须在创建任何线程、初始化 Vulkan 之前（Vulkan 加载器在创建实例时读取这个环境变量）
pub fn prepare() {
    let value = match std::env::var("VK_LOADER_LAYERS_DISABLE") {
        Ok(v) if !v.trim().is_empty() => format!("{v},{DISABLED_VULKAN_LAYERS}"),
        _ => DISABLED_VULKAN_LAYERS.to_string(),
    };
    std::env::set_var("VK_LOADER_LAYERS_DISABLE", value);
}

/// Tauri 应用创建后、创建任何窗口前调用
pub fn setup(_app: &mut App) {
    // 禁止窗口幽灵化：界面线程短暂卡顿时，系统不会用带标题栏的默认外壳替换窗口
    unsafe { windows::Win32::UI::WindowsAndMessaging::DisableProcessWindowsGhosting() };
}

/// 主窗口（设置）打开 / 关闭时调用；Windows 上没有要做的
pub fn main_window_shown(_app: &AppHandle) {}
pub fn main_window_closed(_app: &AppHandle) {}

/// Windows 显示语言是否为中文
pub fn system_language() -> &'static str {
    const LANG_CHINESE: u16 = 0x04;
    if unsafe { GetUserDefaultUILanguage() } & 0x3ff == LANG_CHINESE {
        "zh"
    } else {
        "en"
    }
}

/// 剪贴板内容每次改变都会变化的序号
pub fn clipboard_sequence() -> u64 {
    unsafe { GetClipboardSequenceNumber() }.into()
}

/// 用默认程序打开文件、文件夹或网址
pub fn open(target: impl AsRef<std::ffi::OsStr>) -> std::io::Result<()> {
    std::process::Command::new("explorer").arg(target).spawn().map(|_| ())
}
