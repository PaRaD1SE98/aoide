//! 快捷键字符串（如 "right alt+space"）与虚拟键码之间的转换

const VK_LSHIFT: u32 = 0xA0;
const VK_RSHIFT: u32 = 0xA1;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_LMENU: u32 = 0xA4;
const VK_RMENU: u32 = 0xA5;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
pub const VK_ESCAPE: u32 = 0x1B;

/// 名字 → 匹配的虚拟键码（低层钩子报告的修饰键总是区分左右）
fn lookup(name: &str) -> Option<Vec<u32>> {
    let vks = match name {
        "ctrl" | "control" => vec![VK_LCONTROL, VK_RCONTROL],
        "alt" => vec![VK_LMENU, VK_RMENU],
        "shift" => vec![VK_LSHIFT, VK_RSHIFT],
        "windows" | "win" => vec![VK_LWIN, VK_RWIN],
        "left ctrl" => vec![VK_LCONTROL],
        "right ctrl" => vec![VK_RCONTROL],
        "left alt" => vec![VK_LMENU],
        "right alt" | "alt gr" | "altgr" => vec![VK_RMENU],
        "left shift" => vec![VK_LSHIFT],
        "right shift" => vec![VK_RSHIFT],
        "left windows" => vec![VK_LWIN],
        "right windows" => vec![VK_RWIN],
        _ => vec![named_vk(name)?],
    };
    Some(vks)
}

const NAMED: &[(&str, u32)] = &[
    ("backspace", 0x08), ("tab", 0x09), ("enter", 0x0D), ("pause", 0x13),
    ("caps lock", 0x14), ("esc", VK_ESCAPE), ("space", 0x20), ("page up", 0x21),
    ("page down", 0x22), ("end", 0x23), ("home", 0x24), ("left", 0x25), ("up", 0x26),
    ("right", 0x27), ("down", 0x28), ("print screen", 0x2C), ("insert", 0x2D),
    ("delete", 0x2E), ("menu", 0x5D), ("num lock", 0x90), ("scroll lock", 0x91),
    (";", 0xBA), ("=", 0xBB), (",", 0xBC), ("-", 0xBD), (".", 0xBE), ("/", 0xBF),
    ("`", 0xC0), ("[", 0xDB), ("\\", 0xDC), ("]", 0xDD), ("'", 0xDE),
];

fn named_vk(name: &str) -> Option<u32> {
    if let Some(&(_, vk)) = NAMED.iter().find(|(n, _)| *n == name) {
        return Some(vk);
    }
    let bytes = name.as_bytes();
    if bytes.len() == 1 && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return Some(bytes[0].to_ascii_uppercase() as u32);
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x70 + n - 1);
        }
    }
    if let Some(n) = name.strip_prefix("num ").and_then(|n| n.parse::<u32>().ok()) {
        if n <= 9 {
            return Some(0x60 + n);
        }
    }
    None
}

/// 虚拟键码 → 名字。as_modifier=true 时左侧修饰键用通用名（ctrl 而不是 left ctrl）
pub fn name_of(vk: u32, as_modifier: bool) -> String {
    let s = match vk {
        VK_LCONTROL if as_modifier => "ctrl",
        VK_LMENU if as_modifier => "alt",
        VK_LSHIFT if as_modifier => "shift",
        VK_LWIN if as_modifier => "windows",
        VK_LCONTROL => "left ctrl",
        VK_RCONTROL => "right ctrl",
        VK_LMENU => "left alt",
        VK_RMENU => "right alt",
        VK_LSHIFT => "left shift",
        VK_RSHIFT => "right shift",
        VK_LWIN => "left windows",
        VK_RWIN => "right windows",
        0x30..=0x39 | 0x41..=0x5A => return char::from(vk as u8).to_ascii_lowercase().to_string(),
        0x60..=0x69 => return format!("num {}", vk - 0x60),
        0x70..=0x87 => return format!("f{}", vk - 0x70 + 1),
        _ => match NAMED.iter().find(|(_, v)| *v == vk) {
            Some((n, _)) => n,
            None => return format!("vk{vk:#04x}"),
        },
    };
    s.to_string()
}

/// 一个快捷键：parts 中最后一个是触发键，其余是需要按住的修饰键
#[derive(Clone, Debug, PartialEq)]
pub struct Hotkey {
    pub text: String,
    pub parts: Vec<Vec<u32>>,
}

impl Hotkey {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().to_lowercase();
        let parts = text
            .split('+')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| lookup(p).or_else(|| p.strip_prefix("vk0x").and_then(|h| u32::from_str_radix(h, 16).ok()).map(|v| vec![v])))
            .collect::<Option<Vec<_>>>()?;
        if parts.is_empty() {
            return None;
        }
        Some(Self { text, parts })
    }

    pub fn is_single(&self) -> bool {
        self.parts.len() == 1
    }

    pub fn trigger(&self) -> &[u32] {
        self.parts.last().unwrap()
    }

    pub fn modifiers(&self) -> &[Vec<u32>] {
        &self.parts[..self.parts.len() - 1]
    }
}
