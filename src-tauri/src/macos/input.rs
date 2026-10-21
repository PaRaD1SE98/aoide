//! 模拟键盘输入（macOS：CGEventPost）
//!
//! 每个事件都明确设置修饰键标志：用户可能还按着热键（如按住右 Option 触发语音修订时立刻模拟 Command+C），
//! 不能让它混进模拟的快捷键里

use super::cf::{CFTypeRef, CF};
use super::hook::FIELD_USER_DATA;
use super::keycode::{self, FLAG_COMMAND};

/// 写在 Aoide 自己模拟的按键上（事件的用户数据字段），键盘钩子据此跳过它们（见 hook.rs）
pub const AOIDE_INPUT_MARK: i64 = 0x414F_4944; // "AOID"

/// 事件源：不继承系统当前的修饰键状态
const SOURCE_STATE_PRIVATE: i32 = -1;
const HID_EVENT_TAP: u32 = 0;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceCreate(state: i32) -> CFTypeRef;
    fn CGEventCreateKeyboardEvent(source: CFTypeRef, keycode: u16, down: bool) -> CFTypeRef;
    fn CGEventKeyboardSetUnicodeString(event: CFTypeRef, len: usize, text: *const u16);
    fn CGEventSetFlags(event: CFTypeRef, flags: u64);
    fn CGEventSetIntegerValueField(event: CFTypeRef, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: CFTypeRef);
}

fn post(code: u16, down: bool, flags: u64, text: &[u16]) {
    unsafe {
        let source = CF::owned(CGEventSourceCreate(SOURCE_STATE_PRIVATE));
        let src = source.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
        let Some(event) = CF::owned(CGEventCreateKeyboardEvent(src, code, down)) else { return };
        CGEventSetFlags(event.as_ptr(), flags);
        CGEventSetIntegerValueField(event.as_ptr(), FIELD_USER_DATA, AOIDE_INPUT_MARK);
        if !text.is_empty() {
            CGEventKeyboardSetUnicodeString(event.as_ptr(), text.len(), text.as_ptr());
        }
        CGEventPost(HID_EVENT_TAP, event.as_ptr());
    }
}

/// 快捷键：Command + 某键（如 Command+V / Command+C）
pub fn shortcut(letter: char) {
    let Some(code) = keycode::letter(letter) else { return };
    post(keycode::KEY_COMMAND, true, FLAG_COMMAND, &[]);
    post(code, true, FLAG_COMMAND, &[]);
    post(code, false, FLAG_COMMAND, &[]);
    post(keycode::KEY_COMMAND, false, 0, &[]);
}

pub fn backspace() {
    post(keycode::KEY_DELETE, true, 0, &[]);
    post(keycode::KEY_DELETE, false, 0, &[]);
}

/// 直接注入一个 Unicode 字符（事件携带文字本身，不经过键位映射，不会触发应用快捷键）
pub fn unicode_char(c: char) {
    let mut buf = [0u16; 2];
    let text: &[u16] = c.encode_utf16(&mut buf);
    post(0, true, 0, text);
    post(0, false, 0, text);
}
