//! 全局快捷键：低层键盘钩子（WH_KEYBOARD_LL）
//!
//! 不用 RegisterHotKey 之类的「按下即触发」机制：单修饰键热键（如 right alt）在
//! keydown 瞬间就会触发，导致 "right alt+space" 永远轮不到；长按的按键重复还会反复
//! 触发开始/停止。这里是一个状态机，单键热键在松开时才判定：
//!   - 按下→松开期间没按其他键 = 单击 → 切换普通录音 / 停止修订
//!   - 按住热键期间按下组合键（如 space）→ 开始修订录音
//! 热键的按下/松开事件全部拦截，系统看不到 Alt：不会弹窗口菜单，也不会卡键
//! （代价：该键原本的系统组合键不可用，属于有意为之）。

use crate::keys::{self, Hotkey};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Duration;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

#[derive(Clone, Copy, Debug)]
pub enum HotkeyEvent {
    /// 主热键单击
    Tap,
    Revise,
    ReviseStop,
    /// 取消本次任务（只在任务进行中触发）
    Cancel,
}

/// 是否有任务在进行中：只有这时才拦截取消键，平时该键照常传给其他程序
static CANCEL_ARMED: AtomicBool = AtomicBool::new(false);

pub fn set_cancel_armed(armed: bool) {
    CANCEL_ARMED.store(armed, Ordering::SeqCst);
}

#[derive(Default)]
struct Bindings {
    main: Option<Hotkey>,
    /// 修订热键形如 "<单键主热键>+<键>" 时，记录那个组合键（按住主热键时按下即触发）
    revise_hold: Option<Vec<u32>>,
    revise: Option<Hotkey>,
    /// 与主热键相同时为 None（合并为单击逻辑）
    revise_stop: Option<Hotkey>,
    cancel: Option<Hotkey>,
}

#[derive(Default)]
struct HookState {
    pressed: HashSet<u32>,
    tap_down: bool,
    tap_combo: bool,
    /// 被拦截了按下事件的组合热键触发键，松开事件也要拦截
    swallowed: HashSet<u32>,
}

struct Capture {
    pressed: HashSet<u32>,
    order: Vec<u32>,
    tx: Sender<Option<String>>,
}

struct Shared {
    bindings: RwLock<Bindings>,
    state: Mutex<HookState>,
    capture: Mutex<Option<Capture>>,
    tx: Sender<HotkeyEvent>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

/// 安装钩子（整个进程只调用一次）；事件通过 tx 发出
pub fn install(tx: Sender<HotkeyEvent>) {
    let _ = SHARED.set(Shared {
        bindings: RwLock::default(),
        state: Mutex::default(),
        capture: Mutex::default(),
        tx,
    });
    std::thread::Builder::new()
        .name("keyboard-hook".into())
        .spawn(|| unsafe {
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

/// 按配置更新快捷键；返回无法解析的快捷键文本
pub fn update(hotkey: &str, revise: &str, revise_stop: &str, cancel: &str) -> Vec<String> {
    let mut bad = Vec::new();
    let mut parse = |s: &str| {
        let hk = Hotkey::parse(s);
        if hk.is_none() && !s.trim().is_empty() {
            bad.push(s.to_string());
        }
        hk
    };
    let main = parse(hotkey);
    let revise = parse(revise);
    let revise_stop = parse(revise_stop).filter(|h| Some(h) != main.as_ref());
    let cancel = parse(cancel);

    let mut b = Bindings { main, revise, revise_stop, cancel, revise_hold: None };
    if let (Some(main), Some(rev)) = (&b.main, &b.revise) {
        if main.is_single() && rev.parts.len() == 2 && rev.parts[0] == main.parts[0] {
            b.revise_hold = Some(rev.parts[1].clone());
        }
    }
    match (&b.main, &b.revise_hold) {
        (Some(m), Some(_)) => log::info!("Hotkeys: tap='{}', revise='{}' (hold)", m.text, b.revise.as_ref().unwrap().text),
        (Some(m), None) => log::info!("Hotkeys: tap='{}', revise={:?}", m.text, b.revise.as_ref().map(|h| &h.text)),
        _ => log::warn!("Hotkeys: no main hotkey"),
    }
    if let Some(shared) = SHARED.get() {
        *shared.bindings.write().unwrap() = b;
        *shared.state.lock().unwrap() = HookState::default();
    }
    bad
}

/// 录制一个快捷键（设置页用）：期间拦截所有按键，全部松开时返回组合；Esc 取消
pub fn capture(timeout: Duration) -> Option<String> {
    let shared = SHARED.get()?;
    let (tx, rx) = mpsc::channel();
    *shared.capture.lock().unwrap() = Some(Capture { pressed: HashSet::new(), order: Vec::new(), tx });
    let result = rx.recv_timeout(timeout).ok().flatten();
    shared.capture.lock().unwrap().take();
    result
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        // 自己（及其他程序）模拟的按键不参与热键判定，比如粘贴用的 Ctrl+V
        let injected = kb.flags.0 & LLKHF_INJECTED.0 != 0;
        let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        if !injected {
            if let Some(shared) = SHARED.get() {
                if handle(shared, kb.vkCode, down) {
                    return LRESULT(1);
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// 返回 true 表示拦截该事件
fn handle(shared: &Shared, vk: u32, down: bool) -> bool {
    if let Some(cap) = shared.capture.lock().unwrap().as_mut() {
        handle_capture(cap, vk, down);
        return true;
    }

    let mut st = shared.state.lock().unwrap();
    let repeat = down && st.pressed.contains(&vk);
    if down {
        st.pressed.insert(vk);
    } else {
        st.pressed.remove(&vk);
    }
    let b = shared.bindings.read().unwrap();
    let send = |ev| {
        let _ = shared.tx.send(ev);
    };

    if let Some(cancel) = &b.cancel {
        // 未在任务中时不拦截，但仍要放行之前被拦截了按下事件的松开事件
        if (CANCEL_ARMED.load(Ordering::SeqCst) || !down) && combo(cancel, vk, down, repeat, &mut st) {
            if down && !repeat {
                send(HotkeyEvent::Cancel);
            }
            return true;
        }
    }

    if let Some(main) = &b.main {
        if main.is_single() {
            if main.trigger().contains(&vk) {
                if down {
                    if !st.tap_down {
                        st.tap_down = true;
                        st.tap_combo = false;
                    }
                } else {
                    let was_tap = st.tap_down && !st.tap_combo;
                    st.tap_down = false;
                    if was_tap {
                        send(HotkeyEvent::Tap);
                    }
                }
                return true;
            }
            if st.tap_down {
                if let Some(hold) = &b.revise_hold {
                    if hold.contains(&vk) {
                        if down {
                            st.tap_combo = true;
                            if !repeat {
                                send(HotkeyEvent::Revise);
                            }
                        }
                        return true; // 吞掉组合键，避免打进输入框
                    }
                }
                if down {
                    st.tap_combo = true; // 按住热键期间按了别的键，松开时不算单击
                }
            }
        } else if combo(main, vk, down, repeat, &mut st) {
            if down && !repeat {
                send(HotkeyEvent::Tap);
            }
            return true;
        }
    }
    if b.revise_hold.is_none() {
        if let Some(rev) = &b.revise {
            if combo(rev, vk, down, repeat, &mut st) {
                if down && !repeat {
                    send(HotkeyEvent::Revise);
                }
                return true;
            }
        }
    }
    if let Some(stop) = &b.revise_stop {
        if combo(stop, vk, down, repeat, &mut st) {
            if down && !repeat {
                send(HotkeyEvent::ReviseStop);
            }
            return true;
        }
    }
    false
}

/// 普通组合键：触发键按下时修饰键都按住即命中；命中的按下/松开都拦截
fn combo(hk: &Hotkey, vk: u32, down: bool, _repeat: bool, st: &mut HookState) -> bool {
    if !hk.trigger().contains(&vk) {
        return false;
    }
    if !down {
        return st.swallowed.remove(&vk);
    }
    let mods_held = hk
        .modifiers()
        .iter()
        .all(|m| m.iter().any(|k| st.pressed.contains(k)));
    if mods_held {
        st.swallowed.insert(vk);
    }
    mods_held
}

fn handle_capture(cap: &mut Capture, vk: u32, down: bool) {
    if down {
        if vk == keys::VK_ESCAPE && cap.order.is_empty() {
            let _ = cap.tx.send(None);
            return;
        }
        cap.pressed.insert(vk);
        if !cap.order.contains(&vk) {
            cap.order.push(vk);
        }
    } else {
        cap.pressed.remove(&vk);
        if cap.pressed.is_empty() && !cap.order.is_empty() {
            let n = cap.order.len();
            let text = cap
                .order
                .iter()
                .enumerate()
                .map(|(i, &k)| keys::name_of(k, i + 1 < n))
                .collect::<Vec<_>>()
                .join("+");
            let _ = cap.tx.send(Some(text));
            cap.order.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RALT: u32 = 0xA5;
    const SPACE: u32 = 0x20;
    const ESC: u32 = 0x1B;
    const A: u32 = 0x41;

    #[test]
    fn state_machine() {
        let (tx, rx) = mpsc::channel();
        let _ = SHARED.set(Shared {
            bindings: RwLock::default(),
            state: Mutex::default(),
            capture: Mutex::default(),
            tx,
        });
        update("right alt", "right alt+space", "right alt", "esc");
        let sh = SHARED.get().unwrap();
        let events = || rx.try_iter().map(|e| format!("{e:?}")).collect::<Vec<_>>();

        // 单击右 Alt：按下、松开都被拦截，松开时触发
        assert!(handle(sh, RALT, true));
        assert!(events().is_empty());
        assert!(handle(sh, RALT, false));
        assert_eq!(events(), ["Tap"]);

        // 长按的重复按下只算一次
        handle(sh, RALT, true);
        handle(sh, RALT, true);
        handle(sh, RALT, false);
        assert_eq!(events(), ["Tap"]);

        // 按住右 Alt 再按空格：触发修订，空格被吞掉，松开右 Alt 不算单击
        handle(sh, RALT, true);
        assert!(handle(sh, SPACE, true));
        assert!(handle(sh, SPACE, false));
        handle(sh, RALT, false);
        assert_eq!(events(), ["Revise"]);

        // 按住右 Alt 期间按了别的键：放行该键，松开不算单击
        handle(sh, RALT, true);
        assert!(!handle(sh, A, true));
        assert!(!handle(sh, A, false));
        handle(sh, RALT, false);
        assert!(events().is_empty());

        // 没有任务时 Esc 照常放行
        set_cancel_armed(false);
        assert!(!handle(sh, ESC, true));
        assert!(!handle(sh, ESC, false));
        assert!(events().is_empty());

        // 任务进行中：拦截 Esc 并触发取消；任务结束后松开事件也要一并拦截
        set_cancel_armed(true);
        assert!(handle(sh, ESC, true));
        set_cancel_armed(false);
        assert!(handle(sh, ESC, false));
        assert_eq!(events(), ["Cancel"]);
    }
}
