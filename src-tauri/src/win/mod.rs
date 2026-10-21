//! Windows 平台实现。与 macos/mod.rs 提供相同的接口，其他模块统一通过 crate::platform 调用

mod com;
pub mod focus;
pub mod hook;
pub mod input;
pub mod mute;
pub mod overlay;
pub mod popup;
pub mod sound;

use tauri::{App, AppHandle, Manager};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{SC_KEYMENU, WM_SYSCOMMAND};

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

/// 系统权限：Windows 上录音和键盘钩子都不需要用户授权
pub mod permissions {
    use crate::Permissions;

    pub fn status() -> Permissions {
        Permissions { microphone: "granted", accessibility: true }
    }

    pub fn request(_kind: &str) {}
}

/// 主窗口（设置）打开时调用：不让键盘打开窗口菜单（见 no_key_menu）
pub fn main_window_shown(app: &AppHandle) {
    let Some(hwnd) = app.get_webview_window("settings").and_then(|w| w.hwnd().ok()) else { return };
    let hwnd = hwnd.0 as isize;
    // 子类化必须在窗口所属的线程（主线程）上进行
    let _ = app.run_on_main_thread(move || unsafe {
        let _ = SetWindowSubclass(HWND(hwnd as _), Some(no_key_menu), 1, 0);
    });
}

/// 主窗口关闭时调用；Windows 上没有要做的
pub fn main_window_closed(_app: &AppHandle) {}

/// 有的电脑上设置窗口在前台时，键盘钩子收不到按键（见 hook.rs），Alt 照常交给了 Windows：
/// 单按 Alt 会让窗口进入菜单模式、吞掉下一个按键；Alt+空格（语音修订的默认快捷键）会弹出窗口菜单。
/// 这两种都是 Windows 发给外层窗口的 SC_KEYMENU 命令，这里直接丢掉（鼠标打开窗口菜单、Alt+F4 不受影响）
unsafe extern "system" fn no_key_menu(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if msg == WM_SYSCOMMAND && (wparam.0 as u32 & 0xFFF0) == SC_KEYMENU {
        return LRESULT(0);
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

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
