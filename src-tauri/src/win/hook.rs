//! 键盘钩子（Windows：低层键盘钩子 WH_KEYBOARD_LL）。按键交给 hotkeys.rs 的状态机判定
//!
//! 同一按键会依次经过所有程序的低层钩子，后安装的先收到；其中一个拦下（不往下传）后面的就收不到了。
//! Aoide 启动最早，排在最后：有用户反馈设置窗口在前台时热键和快捷键录制都没反应，关掉设置窗口就恢复，
//! 别的电脑上却正常，像是设置窗口获得焦点时有别的钩子先把按键拦下了。
//! 所以设置窗口获得焦点、开始录制快捷键时都重新安装一次钩子，让 Aoide 排到最前面（见 reinstall）

use crate::i18n::tr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, LPARAM, LRESULT, WPARAM};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, SetThreadPriority, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetGUIThreadInfo, GetMessageW, GetWindowThreadProcessId,
    PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, GUITHREADINFO, HC_ACTION, HHOOK,
    KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_SYSKEYDOWN,
};

const WM_APP_REINSTALL: u32 = WM_APP + 1;

/// 钩子线程的 ID（重新安装时给它发消息）
static THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// 设置窗口在前台时是否收到过按键（第一次收到时记一条日志，便于排查上面那种问题）
static RECEIVED_IN_FRONT: AtomicBool = AtomicBool::new(false);

unsafe fn set_hook() -> Option<HHOOK> {
    let hmod = GetModuleHandleW(None).ok();
    match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), hmod.map(Into::into), 0) {
        Ok(hook) => Some(hook),
        Err(e) => {
            log::error!("Keyboard hook failed: {e}");
            None
        }
    }
}

/// 安装钩子（整个进程只调用一次）
pub fn install() {
    std::thread::Builder::new()
        .name("keyboard-hook".into())
        .spawn(|| unsafe {
            // 低层钩子必须及时返回，超时多次会被系统悄悄移除；系统繁忙（如加载模型）时也要优先调度
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
            let Some(mut hook) = set_hook() else { return };
            log::info!("Keyboard hook installed");
            THREAD_ID.store(GetCurrentThreadId(), Ordering::SeqCst);
            // 低层钩子需要安装线程有消息循环
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message == WM_APP_REINSTALL {
                    // 先装新的再卸旧的：中间不会有按键漏掉
                    if let Some(new) = set_hook() {
                        let _ = UnhookWindowsHookEx(hook);
                        hook = new;
                        log::info!("Keyboard hook reinstalled");
                    }
                }
            }
        })
        .expect("spawn hook thread");
}

/// 重新安装钩子，让它排到其他程序的低层键盘钩子前面（任意线程可调用）
pub fn reinstall() {
    let tid = THREAD_ID.load(Ordering::SeqCst);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_APP_REINSTALL, WPARAM(0), LPARAM(0));
        }
    }
}

/// 诊断（钩子收不到按键时写进日志）：Aoide 和当前接收键盘输入的窗口所在进程，各自的程序名和完整性级别。
/// Windows 不把发往完整性级别更高的进程的按键交给低层钩子（UIPI）；设置页里接收按键的是 WebView2 的进程
pub fn diagnose() -> String {
    unsafe {
        let fg = GetForegroundWindow();
        let tid = GetWindowThreadProcessId(fg, None);
        let mut gti = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        let focus = if GetGUIThreadInfo(tid, &mut gti).is_ok() && !gti.hwndFocus.is_invalid() { gti.hwndFocus } else { fg };
        let mut pid = 0;
        GetWindowThreadProcessId(focus, Some(&mut pid));
        format!("Aoide: {}; keyboard focus: {}", describe_process(GetCurrentProcessId()), describe_process(pid))
    }
}

unsafe fn describe_process(pid: u32) -> String {
    let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return format!("pid {pid} (no access)");
    };
    let mut buf = [0u16; 260];
    let mut len = buf.len() as u32;
    let name = match QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) {
        Ok(()) => String::from_utf16_lossy(&buf[..len as usize]).rsplit('\\').next().unwrap_or_default().to_string(),
        Err(_) => "?".into(),
    };
    let level = integrity_level(process).unwrap_or("unknown");
    let _ = CloseHandle(process);
    format!("{name} (pid {pid}, {level} integrity)")
}

unsafe fn integrity_level(process: HANDLE) -> Option<&'static str> {
    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
    let mut buf = [0u64; 16]; // TOKEN_MANDATORY_LABEL 加上它指向的 SID
    let mut len = 0;
    let ok = GetTokenInformation(token, TokenIntegrityLevel, Some(buf.as_mut_ptr().cast()), std::mem::size_of_val(&buf) as u32, &mut len);
    let _ = CloseHandle(token);
    ok.ok()?;
    let sid = (*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL)).Label.Sid;
    let rid = *GetSidSubAuthority(sid, u32::from(*GetSidSubAuthorityCount(sid)) - 1);
    Some(match rid {
        0x4000.. => "System",
        0x3000.. => "High",
        0x2000.. => "Medium",
        0x1000.. => "Low",
        _ => "Untrusted",
    })
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
        if !own && !RECEIVED_IN_FRONT.load(Ordering::Relaxed) {
            let mut pid = 0;
            GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
            if pid == GetCurrentProcessId() && !RECEIVED_IN_FRONT.swap(true, Ordering::Relaxed) {
                // 写日志可能较慢，放到别的线程，钩子回调要尽快返回
                std::thread::spawn(|| log::info!("Keyboard hook is receiving key events while an Aoide window is in front"));
            }
        }
        if !own && crate::hotkeys::on_key(kb.vkCode, down) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 诊断信息能读出本进程的程序名和完整性级别
    #[test]
    fn describes_own_process() {
        let me = unsafe { describe_process(GetCurrentProcessId()) };
        println!("{me}");
        assert!(me.contains(".exe (pid "), "{me}");
        assert!(["Medium integrity", "High integrity", "System integrity"].iter().any(|l| me.contains(l)), "{me}");
        println!("{}", diagnose());
    }
}
