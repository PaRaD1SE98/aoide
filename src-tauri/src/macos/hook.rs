//! 键盘钩子（macOS：CGEventTap）。按键换算成 Windows 虚拟键码后交给 hotkeys.rs 的状态机判定
//!
//! 要能拦截按键（而不只是监听），需要「辅助功能」权限；没授权时创建会失败，这里每秒重试一次，
//! 用户在系统设置里打开后就开始工作，不用重启 Aoide。
//! 修饰键没有按下 / 松开事件，只有「标志位变化」事件，从标志位判断是按下还是松开（见 keycode.rs）

use super::cf::{CFTypeRef, CF};
use super::keycode;
use crate::i18n::tr;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Duration;

type CGEventRef = *mut c_void;
type TapCallback = unsafe extern "C" fn(proxy: *mut c_void, kind: u32, event: CGEventRef, info: *mut c_void) -> CGEventRef;

const SESSION_EVENT_TAP: u32 = 1;
const HEAD_INSERT: u32 = 0;
const TAP_DEFAULT: u32 = 0;
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
const FIELD_KEYCODE: u32 = 9;
pub(super) const FIELD_USER_DATA: u32 = 42;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(tap: u32, place: u32, options: u32, events: u64, callback: TapCallback, info: *mut c_void) -> CFTypeRef;
    fn CGEventTapEnable(tap: CFTypeRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CFTypeRef, field: u32) -> i64;
    fn CGEventGetFlags(event: CFTypeRef) -> u64;
    fn CGEventSetFlags(event: CFTypeRef, flags: u64);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopCommonModes: CFTypeRef;
    fn CFMachPortCreateRunLoopSource(alloc: CFTypeRef, port: CFTypeRef, order: isize) -> CFTypeRef;
    fn CFRunLoopGetCurrent() -> CFTypeRef;
    fn CFRunLoopAddSource(run_loop: CFTypeRef, source: CFTypeRef, mode: CFTypeRef);
    fn CFRunLoopRun();
}

/// 钩子本身：被系统暂停（回调超时等）后据此重新启用
static TAP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// 安装钩子（整个进程只调用一次）
pub fn install() {
    std::thread::Builder::new()
        .name("keyboard-hook".into())
        .spawn(|| unsafe {
            let events = 1u64 << KEY_DOWN | 1 << KEY_UP | 1 << FLAGS_CHANGED;
            let mut warned = false;
            let tap = loop {
                // 没有辅助功能权限时不去创建：创建失败会让系统弹出授权提示，和启动时按顺序弹出的提示撞在一起
                // （见 permissions.rs 的 request_at_startup）。授权后再创建
                let tap = if super::accessibility_trusted(false) {
                    CGEventTapCreate(SESSION_EVENT_TAP, HEAD_INSERT, TAP_DEFAULT, events, tap_proc, std::ptr::null_mut())
                } else {
                    std::ptr::null()
                };
                if !tap.is_null() {
                    break tap;
                }
                if !warned {
                    warned = true;
                    log::warn!("Keyboard hook failed (no Accessibility permission?), retrying until granted");
                }
                std::thread::sleep(Duration::from_secs(1));
            };
            TAP.store(tap as *mut c_void, Ordering::SeqCst);
            let Some(source) = CF::owned(CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0)) else {
                log::error!("Keyboard hook: run loop source failed");
                return;
            };
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source.as_ptr(), kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);
            log::info!("Keyboard hook installed");
            CFRunLoopRun();
        })
        .expect("spawn hook thread");
}

/// 诊断信息（Windows 上有，见 win/hook.rs）
pub fn diagnose() -> String {
    String::new()
}

/// 录制快捷键时一个按键都没收到：提示用户可能的原因
pub fn no_events_hint() -> &'static str {
    tr(
        "没有收到任何按键：请在「系统设置 → 隐私与安全性 → 辅助功能」中允许 Aoide（列表里已有 Aoide 时，先用「−」删除再重新添加）",
        "No key presses were received: allow Aoide in System Settings → Privacy & Security → Accessibility (if Aoide is already listed, remove it with \"−\" and add it again)",
        "キー入力を受け取れませんでした。「システム設定 → プライバシーとセキュリティ → アクセシビリティ」で Aoide を許可してください（すでに一覧にある場合は「−」で削除してから追加し直してください）",
    )
}

unsafe extern "C" fn tap_proc(_proxy: *mut c_void, kind: u32, event: CGEventRef, _info: *mut c_void) -> CGEventRef {
    match kind {
        TAP_DISABLED_BY_TIMEOUT | TAP_DISABLED_BY_USER_INPUT => {
            // 系统暂停了钩子（回调太慢、安全输入模式等）：重新启用
            let tap = TAP.load(Ordering::SeqCst);
            if !tap.is_null() {
                CGEventTapEnable(tap, true);
            }
            return event;
        }
        KEY_DOWN | KEY_UP | FLAGS_CHANGED => {}
        _ => return event,
    }
    // Aoide 自己模拟的按键（粘贴用的 Command+V 等）不参与热键判定
    if CGEventGetIntegerValueField(event, FIELD_USER_DATA) == crate::input::AOIDE_INPUT_MARK {
        return event;
    }
    let code = CGEventGetIntegerValueField(event, FIELD_KEYCODE) as u16;
    let flags = CGEventGetFlags(event);
    let swallow = if kind != FLAGS_CHANGED {
        crate::hotkeys::on_key(keycode::to_vk(code), kind == KEY_DOWN)
    } else if code == keycode::CAPS_LOCK {
        // 大写锁定每按一次只有一个事件（锁定 / 解锁），当作按下并松开
        let vk = keycode::to_vk(code);
        let down = crate::hotkeys::on_key(vk, true);
        crate::hotkeys::on_key(vk, false) || down
    } else if let Some(down) = keycode::modifier_down(code, flags) {
        crate::hotkeys::on_key(keycode::to_vk(code), down)
    } else {
        false
    };
    if swallow {
        return std::ptr::null_mut();
    }
    // 拦截了修饰键的事件，系统仍记着它按着：从放行的按键上去掉它的标志位
    let held = crate::hotkeys::held_main_key();
    if !held.is_empty() {
        CGEventSetFlags(event, keycode::strip(flags, &held));
    }
    event
}
