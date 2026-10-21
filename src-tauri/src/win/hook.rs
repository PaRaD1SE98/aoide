//! 键盘钩子（Windows）：低层键盘钩子 WH_KEYBOARD_LL 为主，Raw Input 补漏。按键交给 hotkeys.rs 的状态机判定
//!
//! 有的电脑上，Aoide 自己的窗口（设置窗口）在前台时，系统不把按键交给低层钩子，按键却照样送到了窗口里：
//! 两个进程的权限级别相同、WebView2 版本相同，重装钩子也不行，原因不明；浏览器等其他程序在前台时一切正常。
//! Raw Input 是系统从键盘设备送出的原始按键流，和低层钩子是两条独立的路，而且包含所有按键
//! （包括 Alt+空格这类会被系统截走、页面收不到的组合）。所以 Aoide 的窗口在前台时，钩子漏掉的按键由 Raw Input 补上。
//! 钩子正常时同一个按键两条路都会收到：两者都在钩子线程上处理，按「键码、按下 / 松开、时间」互相去重。
//! Raw Input 不能拦截按键，但这时按键本来就是发给 Aoide 自己的窗口，拦不拦都无妨

use crate::i18n::tr;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::{w, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThread, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    SetThreadPriority, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEKEYBOARD,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow,
    GetGUIThreadInfo, GetMessageW, GetWindowThreadProcessId, RegisterClassW, SetWindowsHookExW,
    GUITHREADINFO, HC_ACTION, HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, WH_KEYBOARD_LL,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WM_KEYDOWN, WM_SYSKEYDOWN, WNDCLASSW,
};

const RI_KEY_BREAK: u16 = 1;
const RI_KEY_E0: u16 = 2;
/// 同一个按键在钩子和 Raw Input 里的时间戳最多差多少（毫秒）
const SAME_EVENT_MS: u32 = 100;

/// 第一次靠 Raw Input 补上按键时记一条日志（说明这台电脑上钩子收不到发往 Aoide 窗口的按键）
static RAW_USED: AtomicBool = AtomicBool::new(false);

/// 一个按键事件：(键码, 是否按下, 时间戳)
type KeyEvent = (u32, bool, u32);

thread_local! {
    /// 钩子线程上：两条路各自最近处理过的按键，另一条路据此去重
    static FROM_HOOK: RefCell<VecDeque<KeyEvent>> = const { RefCell::new(VecDeque::new()) };
    static FROM_RAW: RefCell<VecDeque<KeyEvent>> = const { RefCell::new(VecDeque::new()) };
}

/// 记下一条路处理过的按键（只留最近几个）
fn remember(list: &RefCell<VecDeque<KeyEvent>>, event: KeyEvent) {
    let mut list = list.borrow_mut();
    if list.len() >= 16 {
        list.pop_front();
    }
    list.push_back(event);
}

/// 另一条路是否已经处理过同一个按键；是的话把那条记录用掉（按住不放的重复按下也能一一对上）
fn take_same(list: &RefCell<VecDeque<KeyEvent>>, (vk, down, time): KeyEvent) -> bool {
    let mut list = list.borrow_mut();
    let found = list.iter().position(|&(v, d, t)| v == vk && d == down && t.abs_diff(time) <= SAME_EVENT_MS);
    found.map(|i| list.remove(i)).is_some()
}

/// 前台窗口是否属于 Aoide
fn aoide_in_front() -> bool {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid)) };
    pid == unsafe { GetCurrentProcessId() }
}

/// 调试用：设置环境变量 AOIDE_SIMULATE_BLIND_HOOK=1 时，钩子假装收不到发往 Aoide 窗口的按键，
/// 用来在钩子正常的电脑上验证 Raw Input 补漏
#[cfg(debug_assertions)]
fn simulate_blind_hook() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("AOIDE_SIMULATE_BLIND_HOOK").is_some())
}

/// 安装钩子和 Raw Input（整个进程只调用一次）
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
            register_raw_input();
            // 低层钩子和 Raw Input 都需要这个线程有消息循环
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message == WM_INPUT {
                    on_raw_input(msg.lParam, msg.time);
                }
                DispatchMessageW(&msg); // WM_INPUT 要交给 DefWindowProc 做清理
            }
        })
        .expect("spawn hook thread");
}

/// 在钩子线程上建一个只收消息的窗口，接收键盘的 Raw Input。
/// 每个进程每类设备只能登记一个接收窗口：这会取代 tao 自己登记的那个（它只用来产生设备事件，Tauri 没有用到）
unsafe fn register_raw_input() {
    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
    let hinstance = GetModuleHandleW(None).unwrap_or_default();
    let class = w!("AoideRawInput");
    RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(wnd_proc), hInstance: hinstance.into(), lpszClassName: class, ..Default::default() });
    let hwnd = match CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        w!(""),
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        Some(hinstance.into()),
        None,
    ) {
        Ok(h) => h,
        Err(e) => {
            log::error!("Raw input window failed: {e}");
            return;
        }
    };
    let keyboard = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x06, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
    match RegisterRawInputDevices(&[keyboard], std::mem::size_of::<RAWINPUTDEVICE>() as u32) {
        Ok(()) => log::info!("Raw keyboard input registered"),
        Err(e) => log::error!("Raw keyboard input failed: {e}"),
    }
}

/// Raw Input 收到一个按键：Aoide 的窗口在前台、钩子又没处理过它时，交给状态机
unsafe fn on_raw_input(lparam: LPARAM, time: u32) {
    let handle = HRAWINPUT(lparam.0 as _);
    let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut size = 0u32;
    GetRawInputData(handle, RID_INPUT, None, &mut size, header);
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    if GetRawInputData(handle, RID_INPUT, Some(buf.as_mut_ptr().cast()), &mut size, header) == u32::MAX {
        return;
    }
    let raw = &*(buf.as_ptr() as *const RAWINPUT);
    if raw.header.dwType != RIM_TYPEKEYBOARD.0 {
        return;
    }
    let kb = raw.data.keyboard;
    // Aoide 自己模拟的按键（粘贴用的 Ctrl+V 等）不算；0xFF 是部分组合键附带的假按键
    if kb.ExtraInformation as usize == crate::input::AOIDE_INPUT_MARK || kb.VKey == 0xFF || !aoide_in_front() {
        return;
    }
    let vk = raw_vk(kb.VKey, kb.MakeCode, kb.Flags);
    let event = (vk, kb.Flags & RI_KEY_BREAK == 0, time);
    if FROM_HOOK.with(|l| take_same(l, event)) {
        return; // 钩子已经处理过
    }
    FROM_RAW.with(|l| remember(l, event));
    if !RAW_USED.swap(true, Ordering::Relaxed) {
        log::warn!("Keyboard hook is missing keys sent to an Aoide window; using raw input for them. {}", diagnose());
    }
    crate::hotkeys::on_raw_key(event.0, event.1);
}

/// Raw Input 的修饰键不分左右，换成钩子报告的那种区分左右的虚拟键码
fn raw_vk(vkey: u16, make_code: u16, flags: u16) -> u32 {
    let e0 = flags & RI_KEY_E0 != 0;
    match vkey {
        0x12 => if e0 { 0xA5 } else { 0xA4 }, // Alt
        0x11 => if e0 { 0xA3 } else { 0xA2 }, // Ctrl
        0x10 => if make_code == 0x36 { 0xA1 } else { 0xA0 }, // Shift（右 Shift 的扫描码是 0x36）
        v => v as u32,
    }
}

/// 诊断（钩子漏掉按键时写进日志）：Aoide 和当前接收键盘输入的窗口所在进程，各自的程序名和完整性级别
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
    let level = integrity_level(process).map_or("unknown".into(), |(name, rid)| format!("{name} {rid:#x}"));
    let _ = CloseHandle(process);
    format!("{name} (pid {pid}, {level} integrity)")
}

/// 完整性级别的名字和确切数值（同一档里还有细分，如 Medium Plus = 0x2100）
unsafe fn integrity_level(process: HANDLE) -> Option<(&'static str, u32)> {
    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
    let mut buf = [0u64; 16]; // TOKEN_MANDATORY_LABEL 加上它指向的 SID
    let mut len = 0;
    let ok = GetTokenInformation(token, TokenIntegrityLevel, Some(buf.as_mut_ptr().cast()), std::mem::size_of_val(&buf) as u32, &mut len);
    let _ = CloseHandle(token);
    ok.ok()?;
    let sid = (*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL)).Label.Sid;
    let rid = *GetSidSubAuthority(sid, u32::from(*GetSidSubAuthorityCount(sid)) - 1);
    let name = match rid {
        0x4000.. => "System",
        0x3000.. => "High",
        0x2000.. => "Medium",
        0x1000.. => "Low",
        _ => "Untrusted",
    };
    Some((name, rid))
}

/// 录制快捷键时一个按键都没收到：提示用户可能的原因
pub fn no_events_hint() -> &'static str {
    tr(
        "没有收到任何按键：键盘监听可能被安全软件拦截。请把 Aoide 加入信任后，在托盘菜单退出并重新打开 Aoide",
        "No key presses were received: the keyboard listener may be blocked by security software. Allow Aoide in it, then quit Aoide from the tray menu and start it again",
        "キー入力を受け取れませんでした。キーボードの監視がセキュリティソフトにブロックされている可能性があります。Aoide を許可してから、トレイメニューで終了して Aoide を起動し直してください",
    )
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        // Aoide 自己模拟的按键（粘贴用的 Ctrl+V 等）不参与热键判定。其他程序模拟的按键照常处理：
        // AutoHotkey 重映射、键鼠共享软件转发的按键都带 LLKHF_INJECTED，不能一概忽略
        let own = kb.flags.0 & LLKHF_INJECTED.0 != 0 && kb.dwExtraInfo == crate::input::AOIDE_INPUT_MARK;
        #[cfg(debug_assertions)]
        if simulate_blind_hook() && aoide_in_front() {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        if !own {
            let event = (kb.vkCode, matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN), kb.time);
            // Raw Input 先到、已经处理过的就不再处理（通常是钩子先到）
            if FROM_RAW.with(|l| take_same(l, event)) {
                return CallNextHookEx(None, code, wparam, lparam);
            }
            FROM_HOOK.with(|l| remember(l, event));
            if crate::hotkeys::on_key(event.0, event.1) {
                return LRESULT(1);
            }
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
        assert!(["Medium 0x", "High 0x", "System 0x"].iter().any(|l| me.contains(l)), "{me}");
        println!("{}", diagnose());
    }

    /// 两条路的去重：同一个按键（键码、按下 / 松开相同，时间相近）只算一次，按住不放的重复按下逐个对上
    #[test]
    fn matches_same_key_events() {
        let list = RefCell::new(VecDeque::new());
        remember(&list, (0xA5, true, 1000));
        remember(&list, (0xA5, true, 1030)); // 按住不放的重复按下
        assert!(!take_same(&list, (0xA5, false, 1000))); // 松开不是同一个事件
        assert!(!take_same(&list, (0x20, true, 1000)));
        assert!(!take_same(&list, (0xA5, true, 1500))); // 时间差太多
        assert!(take_same(&list, (0xA5, true, 1001)));
        assert!(take_same(&list, (0xA5, true, 1031)));
        assert!(!take_same(&list, (0xA5, true, 1031))); // 已经用掉
    }

    #[test]
    fn maps_raw_modifiers() {
        assert_eq!(raw_vk(0x12, 0x38, RI_KEY_E0), 0xA5); // 右 Alt
        assert_eq!(raw_vk(0x12, 0x38, 0), 0xA4);
        assert_eq!(raw_vk(0x11, 0x1D, RI_KEY_E0), 0xA3);
        assert_eq!(raw_vk(0x10, 0x36, 0), 0xA1);
        assert_eq!(raw_vk(0x10, 0x2A, RI_KEY_BREAK), 0xA0);
        assert_eq!(raw_vk(0x20, 0x39, 0), 0x20);
    }
}
