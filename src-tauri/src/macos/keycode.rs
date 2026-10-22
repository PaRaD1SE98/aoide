//! macOS 键码（kVK_*，按键的物理位置）与 Windows 虚拟键码之间的换算，以及修饰键的事件标志位
//!
//! 热键状态机和快捷键名字（keys.rs）都按 Windows 虚拟键码工作；没有对应的键换成 0x1000 + 键码，
//! 照样能录制和匹配（显示为 vk0x10xx）

use crate::keys::VK_FN;

/// 事件标志位里的通用修饰键位（不分左右）
pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_OPTION: u64 = 0x0008_0000;
pub const FLAG_COMMAND: u64 = 0x0010_0000;
pub const FLAG_FN: u64 = 0x0080_0000;

pub const CAPS_LOCK: u16 = 0x39;
pub const KEY_COMMAND: u16 = 0x37;
pub const KEY_DELETE: u16 = 0x33;
pub const KEY_LEFT: u16 = 0x7B;

/// 修饰键：(键码, 虚拟键码, 区分左右的标志位, 通用标志位)。fn 没有区分左右的位
const MODIFIERS: &[(u16, u32, u64, u64)] = &[
    (0x3B, 0xA2, 0x0000_0001, FLAG_CONTROL), // 左 Control
    (0x3E, 0xA3, 0x0000_2000, FLAG_CONTROL), // 右 Control
    (0x38, 0xA0, 0x0000_0002, FLAG_SHIFT),   // 左 Shift
    (0x3C, 0xA1, 0x0000_0004, FLAG_SHIFT),   // 右 Shift
    (0x37, 0x5B, 0x0000_0008, FLAG_COMMAND), // 左 Command
    (0x36, 0x5C, 0x0000_0010, FLAG_COMMAND), // 右 Command
    (0x3A, 0xA4, 0x0000_0020, FLAG_OPTION),  // 左 Option
    (0x3D, 0xA5, 0x0000_0040, FLAG_OPTION),  // 右 Option
    (0x3F, VK_FN, 0, FLAG_FN),               // fn（🌐）
];

/// 区分左右的修饰键位全部
const DEVICE_BITS: u64 = 0x0000_207F;

/// 其他键：(键码, 虚拟键码)。字母、数字按美式键盘的位置
const KEYS: &[(u16, u32)] = &[
    (0x00, 0x41), (0x0B, 0x42), (0x08, 0x43), (0x02, 0x44), (0x0E, 0x45), (0x03, 0x46), // A B C D E F
    (0x05, 0x47), (0x04, 0x48), (0x22, 0x49), (0x26, 0x4A), (0x28, 0x4B), (0x25, 0x4C), // G H I J K L
    (0x2E, 0x4D), (0x2D, 0x4E), (0x1F, 0x4F), (0x23, 0x50), (0x0C, 0x51), (0x0F, 0x52), // M N O P Q R
    (0x01, 0x53), (0x11, 0x54), (0x20, 0x55), (0x09, 0x56), (0x0D, 0x57), (0x07, 0x58), // S T U V W X
    (0x10, 0x59), (0x06, 0x5A),                                                         // Y Z
    (0x1D, 0x30), (0x12, 0x31), (0x13, 0x32), (0x14, 0x33), (0x15, 0x34),               // 0 1 2 3 4
    (0x17, 0x35), (0x16, 0x36), (0x1A, 0x37), (0x1C, 0x38), (0x19, 0x39),               // 5 6 7 8 9
    (0x29, 0xBA), (0x18, 0xBB), (0x2B, 0xBC), (0x1B, 0xBD), (0x2F, 0xBE), (0x2C, 0xBF), // ; = , - . /
    (0x32, 0xC0), (0x21, 0xDB), (0x2A, 0xDC), (0x1E, 0xDD), (0x27, 0xDE),               // ` [ \ ] '
    (0x24, 0x0D), (0x30, 0x09), (0x31, 0x20), (KEY_DELETE, 0x08), (0x35, 0x1B),         // Return Tab Space Delete Esc
    (0x75, 0x2E), (0x72, 0x2D), (0x73, 0x24), (0x77, 0x23), (0x74, 0x21), (0x79, 0x22), // ⌦ Help(Insert) Home End PgUp PgDn
    (0x7B, 0x25), (0x7E, 0x26), (0x7C, 0x27), (0x7D, 0x28),                             // ← ↑ → ↓
    (CAPS_LOCK, 0x14),
    (0x7A, 0x70), (0x78, 0x71), (0x63, 0x72), (0x76, 0x73), (0x60, 0x74), (0x61, 0x75), // F1–F6
    (0x62, 0x76), (0x64, 0x77), (0x65, 0x78), (0x6D, 0x79), (0x67, 0x7A), (0x6F, 0x7B), // F7–F12
    (0x69, 0x7C), (0x6B, 0x7D), (0x71, 0x7E), (0x6A, 0x7F), (0x40, 0x80), (0x4F, 0x81), // F13–F18
    (0x50, 0x82), (0x5A, 0x83),                                                         // F19 F20
    (0x52, 0x60), (0x53, 0x61), (0x54, 0x62), (0x55, 0x63), (0x56, 0x64),               // 小键盘 0–4
    (0x57, 0x65), (0x58, 0x66), (0x59, 0x67), (0x5B, 0x68), (0x5C, 0x69),               // 小键盘 5–9
];

pub fn to_vk(code: u16) -> u32 {
    MODIFIERS
        .iter()
        .map(|&(c, vk, ..)| (c, vk))
        .chain(KEYS.iter().copied())
        .find(|&(c, _)| c == code)
        .map_or(0x1000 + code as u32, |(_, vk)| vk)
}

/// 字母在美式键盘上的键码（模拟 Command+V 等快捷键用）
pub fn letter(c: char) -> Option<u16> {
    let vk = c.to_ascii_uppercase() as u32;
    KEYS.iter().find(|&&(_, v)| v == vk && c.is_ascii_alphabetic()).map(|&(code, _)| code)
}

/// 修饰键的「标志位变化」事件：按下还是松开。不是修饰键返回 None
pub fn modifier_down(code: u16, flags: u64) -> Option<bool> {
    let &(_, _, device, generic) = MODIFIERS.iter().find(|m| m.0 == code)?;
    // 有的虚拟键盘、远程桌面不填区分左右的位，只能看通用位
    if device == 0 || flags & DEVICE_BITS == 0 {
        return Some(flags & generic != 0);
    }
    Some(flags & device != 0)
}

/// 从事件标志位里去掉这些修饰键（虚拟键码）；同类的另一侧还按着时保留通用位
pub fn strip(flags: u64, vks: &[u32]) -> u64 {
    let mut out = flags;
    for &vk in vks {
        if let Some(&(_, _, device, generic)) = MODIFIERS.iter().find(|m| m.1 == vk) {
            out &= !device;
            let sibling = MODIFIERS.iter().any(|m| m.3 == generic && m.2 != device && out & m.2 != 0);
            if !sibling {
                out &= !generic;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_keys() {
        assert_eq!(to_vk(0x3D), 0xA5); // 右 Option = right alt
        assert_eq!(to_vk(0x31), 0x20); // 空格
        assert_eq!(to_vk(0x00), 0x41); // A
        assert_eq!(to_vk(0x66), 0x1066); // 英数（JIS）：没有对应的虚拟键码
        assert_eq!(letter('v'), Some(0x09));
        assert_eq!(letter('C'), Some(0x08));
        assert_eq!(letter('1'), None);
    }

    #[test]
    fn modifier_flags() {
        let ropt = FLAG_OPTION | 0x40;
        assert_eq!(modifier_down(0x3D, ropt), Some(true));
        assert_eq!(modifier_down(0x3D, 0), Some(false));
        // 左 Option 还按着时松开右 Option
        assert_eq!(modifier_down(0x3D, FLAG_OPTION | 0x20), Some(false));
        // 不填区分左右的位
        assert_eq!(modifier_down(0x3D, FLAG_OPTION), Some(true));
        assert_eq!(modifier_down(0x31, ropt), None);
        // 去掉右 Option；左 Option 也按着时保留通用位
        assert_eq!(strip(ropt | FLAG_SHIFT | 0x02, &[0xA5]), FLAG_SHIFT | 0x02);
        assert_eq!(strip(ropt | 0x20, &[0xA5]), FLAG_OPTION | 0x20);
        assert_eq!(strip(FLAG_FN, &[VK_FN]), 0);
    }
}
