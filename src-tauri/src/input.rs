//! 模拟键盘输入（SendInput）

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_BACK, VK_CONTROL,
};

fn key(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Ctrl + 某键（如 Ctrl+V / Ctrl+C）
pub fn ctrl(letter: char) {
    let vk = VIRTUAL_KEY(letter.to_ascii_uppercase() as u16);
    send(&[
        key(VK_CONTROL, 0, KEYBD_EVENT_FLAGS(0)),
        key(vk, 0, KEYBD_EVENT_FLAGS(0)),
        key(vk, 0, KEYEVENTF_KEYUP),
        key(VK_CONTROL, 0, KEYEVENTF_KEYUP),
    ]);
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
