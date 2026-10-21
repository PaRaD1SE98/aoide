//! 焦点输入框（macOS：辅助功能 API）：可输入检测、光标前后的文字、跟踪全文
//!
//! 可输入检测与 Windows 一样分两层：
//! 1. 看焦点元素的角色：文本框、文本区域、组合框、搜索框，且文字可写
//! 2. 判不出来时打一个探针字符实测（终端、自绘界面等）：文字或选区变了 → 退格删掉探针，判定可输入；
//!    没有任何插入迹象 → 判定不可输入。Windows 还能看系统光标有没有移动，macOS 没有这个信号，
//!    文字和选区都读不到的控件不打探针，直接判定不可输入（结果显示在卡片里）
//!
//! 例外：Chrome、Firefox 的网页内容可能始终读不到（见 ax.rs），焦点只停在网页或窗口上、
//! 或读不到焦点元素时，判定为「不确定」：照样粘贴，文字留在剪贴板里（与 OpenWhispr 等同类工具一致）
//!
//! 文字的位置、长度都按 UTF-16 计（辅助功能 API 的约定）

use super::ax::{self, Element};
use super::cf::CFRange;
use crate::focus::{CaretContext, Typable};
use crate::input;
use std::time::Duration;

const PROBE_CHAR: char = '∙';
const PROBE_POLL: Duration = Duration::from_millis(50);
const PROBE_WAIT: Duration = Duration::from_millis(500);

/// 最多读取光标前 / 后多少个字符（只取光标附近，长文档也不会慢）
const CONTEXT_BEFORE: isize = 600;
const CONTEXT_AFTER: isize = 300;
/// 读取全文的上限（再长的文档就不跟踪了）
const MAX_TEXT: isize = 20_000;

/// 首次调用辅助功能 API 较慢，提前调用一次
pub fn warm_up() {
    let trusted = super::accessibility_trusted(false);
    let focused = ax::focused_element().is_some();
    log::info!("AX warmed up: trusted={trusted}, focused element={focused}");
}

fn is_secure(elem: &Element) -> bool {
    elem.string("AXSubrole").as_deref() == Some("AXSecureTextField")
}

/// 角色层：焦点元素是否为可写的文本控件
fn is_editable(elem: &Element) -> bool {
    let role = elem.string("AXRole").unwrap_or_default();
    if elem.bool("AXEnabled") == Some(false) {
        return false;
    }
    let text_role = matches!(role.as_str(), "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField");
    let editable = text_role && elem.is_settable("AXValue");
    log::info!("Focus AX: role={role} -> editable={editable}");
    editable
}

/// 文字和选区：探针前后各读一次，任一变化即说明字符插进去了
fn snapshot(elem: Option<&Element>) -> (Option<String>, Option<CFRange>) {
    elem.map_or((None, None), |e| (e.string("AXValue"), e.range("AXSelectedTextRange")))
}

/// 探针层：实际打一个字符验证能否输入，成功则退格删除
fn try_type_probe(elem: Option<&Element>) -> bool {
    let (before_text, before_sel) = snapshot(elem);
    // 文字和选区都读不到：插没插进去都看不出来，打了也删不掉，不打
    if before_text.is_none() && before_sel.is_none() {
        log::info!("Type probe skipped: focused control is not readable");
        return false;
    }
    input::unicode_char(PROBE_CHAR);
    // 浏览器把网页里的变化同步给辅助功能树要一会儿：最多等 PROBE_WAIT，看到变化就停
    let mut waited = Duration::ZERO;
    let (mut after_text, mut after_sel);
    let inserted = loop {
        std::thread::sleep(PROBE_POLL);
        waited += PROBE_POLL;
        (after_text, after_sel) = snapshot(elem);
        let changed = (before_text.is_some() && after_text != before_text) || (before_sel.is_some() && after_sel != before_sel);
        if changed || waited >= PROBE_WAIT {
            break changed;
        }
    };
    if inserted {
        input::backspace();
        std::thread::sleep(Duration::from_millis(50));
    }
    log::info!(
        "Type probe: inserted={inserted} (text_signal={}, selection_signal={})",
        before_text.is_some() || after_text.is_some(),
        before_sel.is_some() || after_sel.is_some()
    );
    inserted
}

/// 综合判定：角色快路径 → 浏览器里读不到网页内容时判为不确定 → 探针实测兜底
pub fn can_type_here() -> Typable {
    let (elem, kind) = ax::focused_element_and_kind();
    if elem.is_none() {
        log::info!("Focus AX: no focused element");
    }
    if elem.as_ref().is_some_and(is_editable) {
        return Typable::Yes;
    }
    if kind.is_browser() && elem.as_ref().is_none_or(ax::is_page_level) {
        log::info!("Focus AX: {kind:?} page content is not readable -> unknown");
        return Typable::Unknown;
    }
    if try_type_probe(elem.as_ref()) {
        Typable::Yes
    } else {
        Typable::No
    }
}

/// 文字的总长度（UTF-16）
fn length(elem: &Element) -> Option<isize> {
    elem.number("AXNumberOfCharacters")
        .map(|n| n as isize)
        .or_else(|| elem.string("AXValue").map(|v| v.encode_utf16().count() as isize))
}

/// 读取一段文字：优先 AXStringForRange，不支持时从全文里截取
fn text_range(elem: &Element, location: isize, length: isize) -> Option<String> {
    if length <= 0 {
        return Some(String::new());
    }
    let range = CFRange { location, length };
    elem.string_for_range(range).or_else(|| {
        let all: Vec<u16> = elem.string("AXValue")?.encode_utf16().collect();
        let from = (location as usize).min(all.len());
        let to = ((location + length) as usize).min(all.len());
        Some(String::from_utf16_lossy(&all[from..to]))
    })
}

fn normalize(s: String) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n").replace('\u{a0}', " ")
}

/// 读取当前焦点输入框中光标前后的文字；密码框、读不到选区的控件返回 None
pub fn caret_context() -> Option<CaretContext> {
    let elem = ax::focused_element()?;
    // 绝不读取密码框，避免把密码发给 LLM
    if is_secure(&elem) {
        return None;
    }
    let sel = elem.range("AXSelectedTextRange")?;
    let total = length(&elem)?;
    let (start, end) = (sel.location.clamp(0, total), (sel.location + sel.length).clamp(0, total));
    let from = (start - CONTEXT_BEFORE).max(0);
    let to = (end + CONTEXT_AFTER).min(total);
    Some(CaretContext {
        before: normalize(text_range(&elem, from, start - from)?),
        after: normalize(text_range(&elem, end, to - end)?),
    })
}

/// 跟踪某个输入框的全文（学习用户对识别结果的修改，见 learn.rs）
pub struct TextWatch {
    elem: Element,
}

impl TextWatch {
    /// 跟踪当前焦点控件；密码框、读不到文字的控件返回 None
    pub fn focused() -> Option<Self> {
        let elem = ax::focused_element()?;
        if is_secure(&elem) {
            return None;
        }
        let watch = Self { elem };
        watch.text().map(|_| watch)
    }

    /// 控件当前的全文（换行统一为 \n）
    pub fn text(&self) -> Option<String> {
        let total = length(&self.elem)?;
        let text = if total > MAX_TEXT { text_range(&self.elem, 0, MAX_TEXT)? } else { self.elem.string("AXValue")? };
        Some(normalize(text))
    }

    /// 焦点是否还在这个控件上
    pub fn still_focused(&self) -> bool {
        ax::focused_element().is_some_and(|f| f.same_as(&self.elem))
    }
}
