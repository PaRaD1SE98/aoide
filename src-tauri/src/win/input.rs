//! 模拟键盘输入（Windows：SendInput）

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_LEFT, VK_SHIFT,
};

/// 写在 Aoide 自己模拟的按键上（dwExtraInfo），键盘钩子据此跳过它们（见 hook.rs）。
/// 只跳过自己发的，而不是所有模拟按键：AutoHotkey 重映射、键鼠共享软件（Mouse without Borders、
/// Synergy 等）转发的按键都带「模拟」标记，全部忽略的话这些用户按什么键都不会被识别
pub const AOIDE_INPUT_MARK: usize = 0x414F_4944; // "AOID"

fn key(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: AOIDE_INPUT_MARK },
        },
    }
}

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// 快捷键：Ctrl + 某键（如 Ctrl+V / Ctrl+C）
pub fn shortcut(letter: char) {
    let vk = VIRTUAL_KEY(letter.to_ascii_uppercase() as u16);
    send(&[
        key(VK_CONTROL, 0, KEYBD_EVENT_FLAGS(0)),
        key(vk, 0, KEYBD_EVENT_FLAGS(0)),
        key(vk, 0, KEYEVENTF_KEYUP),
        key(VK_CONTROL, 0, KEYEVENTF_KEYUP),
    ]);
}

/// 等用户松开修饰键：Windows 上不需要（钩子吞掉了热键，模拟的 Ctrl+C 不受影响）
pub fn wait_modifiers_released(_timeout: std::time::Duration) -> bool {
    true
}

/// 按住 Shift 按 n 次 ←：从光标往前选中 n 个字符（重新翻译时选中之前粘贴的译文）。
/// 方向键要带「扩展键」标志，否则会被当成小键盘上的 4，NumLock 打开时 Shift 会失效
pub fn select_back(n: usize) {
    let mut inputs = vec![key(VK_SHIFT, 0, KEYBD_EVENT_FLAGS(0))];
    for _ in 0..n {
        inputs.push(key(VK_LEFT, 0, KEYEVENTF_EXTENDEDKEY));
        inputs.push(key(VK_LEFT, 0, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP));
    }
    inputs.push(key(VK_SHIFT, 0, KEYEVENTF_KEYUP));
    send(&inputs);
}

pub fn backspace() {
    send(&[key(VK_BACK, 0, KEYBD_EVENT_FLAGS(0)), key(VK_BACK, 0, KEYEVENTF_KEYUP)]);
}

/// 直接注入一个 Unicode 字符（走 VK_PACKET，不经过键位映射，不会触发应用快捷键）
pub fn unicode_char(c: char) {
    let mut buf = [0u16; 2];
    let inputs: Vec<INPUT> = c
        .encode_utf16(&mut buf)
        .iter()
        .flat_map(|&u| {
            [
                key(VIRTUAL_KEY(0), u, KEYEVENTF_UNICODE),
                key(VIRTUAL_KEY(0), u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
            ]
        })
        .collect();
    send(&inputs);
}
