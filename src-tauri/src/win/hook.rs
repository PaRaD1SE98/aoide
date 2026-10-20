//! 键盘钩子（Windows：低层键盘钩子 WH_KEYBOARD_LL）。按键交给 hotkeys.rs 的状态机判定

use crate::i18n::tr;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// 安装钩子（整个进程只调用一次）
pub fn install() {
    std::thread::Builder::new()
        .name("keyboard-hook".into())
        .spawn(|| unsafe {
            // 低层钩子必须及时返回，超时多次会被系统悄悄移除；系统繁忙（如加载模型）时也要优先调度
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
            let hmod = GetModuleHandleW(None).ok();
            match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), hmod.map(Into::into), 0) {
                Ok(_) => log::info!("Keyboard hook installed"),
                Err(e) => {
                    log::error!("Keyboard hook failed: {e}");
                    return;
                }
            }
            // 低层钩子需要安装线程有消息循环
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        })
        .expect("spawn hook thread");
}

/// 录制快捷键时一个按键都没收到：提示用户可能的原因
pub fn no_events_hint() -> &'static str {
    tr(
        "没有收到任何按键：键盘监听可能被安全软件拦截。请把 Aoide 加入信任后，在托盘菜单退出并重新打开 Aoide",
        "No key presses were received: the keyboard listener may be blocked by security software. Allow Aoide in it, then quit Aoide from the tray menu and start it again",
    )
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        // Aoide 自己模拟的按键（粘贴用的 Ctrl+V 等）不参与热键判定。其他程序模拟的按键照常处理：
        // AutoHotkey 重映射、键鼠共享软件转发的按键都带 LLKHF_INJECTED，不能一概忽略
        let own = kb.flags.0 & LLKHF_INJECTED.0 != 0 && kb.dwExtraInfo == crate::input::AOIDE_INPUT_MARK;
        let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        if !own && crate::hotkeys::on_key(kb.vkCode, down) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}
