//! 焦点输入框（Windows）：可输入检测、光标前后的文字、跟踪全文
//!
//! 可输入检测分两层：
//! 1. UI Automation 判断焦点控件类型 —— 快，覆盖绝大多数应用。窗口类名在这里没用
//!    （Chrome、Qt、UWP 整个窗口只有一个宿主类），只信任 Edit / ComboBox / Document；
//!    单独的可写 ValuePattern 不可信（资源管理器的文件项也有，用于重命名）。
//! 2. UIA 判不可输入时，打一个探针字符实测（Tk、部分终端等应用对 UIA 不透明）：
//!    有插入迹象（UIA 文本变化或系统光标移动）→ 退格删掉探针，判定可输入；
//!    没有任何插入迹象 → 判定不可输入。
//!    探针选无键位映射的 Unicode 字符，注入走 VK_PACKET，不会触发应用快捷键。

use super::com::ComGuard;
use crate::focus::{CaretContext, Typable};
use crate::input;
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationValuePattern, TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start,
    TextUnit_Character, UIA_ComboBoxControlTypeId, UIA_DocumentControlTypeId,
    UIA_EditControlTypeId, UIA_IsPasswordPropertyId, UIA_IsTextEditPatternAvailablePropertyId,
    UIA_IsValuePatternAvailablePropertyId, UIA_TextPatternId, UIA_ValueIsReadOnlyPropertyId,
    UIA_ValuePatternId, UIA_PROPERTY_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
};

const PROBE_CHAR: char = '∙';

fn uia() -> windows::core::Result<IUIAutomation> {
    unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
}

fn prop_bool(elem: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> bool {
    unsafe { elem.GetCurrentPropertyValue(id) }
        .ok()
        .and_then(|v: VARIANT| bool::try_from(&v).ok())
        .unwrap_or(false)
}

/// 预热 UIA（首次创建较慢），避免第一次输出时卡顿
pub fn warm_up() {
    let _com = ComGuard::new();
    let result = uia();
    match &result {
        Ok(_) => log::info!("UIA warmed up"),
        Err(e) => log::warn!("UIA warmup failed: {e}"),
    }
}

/// UIA 层：当前键盘焦点是否在可输入控件上；检测不了时返回 false，交给探针层
fn is_focused_editable(uia: &IUIAutomation) -> bool {
    let Ok(elem) = (unsafe { uia.GetFocusedElement() }) else {
        return false;
    };
    unsafe {
        if !elem.CurrentIsEnabled().map(|b| b.as_bool()).unwrap_or(false) {
            return false;
        }
        let ctype = elem.CurrentControlType().unwrap_or_default();
        let value_avail = prop_bool(&elem, UIA_IsValuePatternAvailablePropertyId);
        let read_only = value_avail && prop_bool(&elem, UIA_ValueIsReadOnlyPropertyId);
        let editable = if ctype == UIA_EditControlTypeId {
            !read_only
        } else if ctype == UIA_ComboBoxControlTypeId {
            value_avail && !read_only
        } else if ctype == UIA_DocumentControlTypeId {
            // 富文本编辑区（Word/写字板等）：有 TextEdit 模式或可写 ValuePattern
            prop_bool(&elem, UIA_IsTextEditPatternAvailablePropertyId) || (value_avail && !read_only)
        } else {
            false
        };
        log::info!(
            "Focus UIA: ctype={} value={value_avail} ro={read_only} -> editable={editable}",
            ctype.0
        );
        editable
    }
}

/// 读取焦点元素的文本内容（ValuePattern 或 TextPattern）；读不到返回 None
fn read_focus_text(uia: &IUIAutomation) -> Option<String> {
    unsafe {
        let elem = uia.GetFocusedElement().ok()?;
        if let Ok(p) = elem.GetCurrentPattern(UIA_ValuePatternId) {
            if let Ok(vp) = p.cast::<IUIAutomationValuePattern>() {
                if let Ok(v) = vp.CurrentValue() {
                    return Some(v.to_string());
                }
            }
        }
        if let Ok(p) = elem.GetCurrentPattern(UIA_TextPatternId) {
            if let Ok(tp) = p.cast::<IUIAutomationTextPattern>() {
                if let Ok(t) = tp.DocumentRange().and_then(|r| r.GetText(4096)) {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

/// 前台线程的系统光标位置；没有光标返回 None
fn caret_rect() -> Option<(i32, i32, i32, i32)> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let tid = GetWindowThreadProcessId(hwnd, None);
        let mut gti = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        GetGUIThreadInfo(tid, &mut gti).ok()?;
        if gti.hwndCaret.is_invalid() {
            return None;
        }
        let RECT { left, top, right, bottom } = gti.rcCaret;
        Some((left, top, right, bottom))
    }
}

/// 探针层：实际打一个字符验证能否输入，成功则退格删除
fn try_type_probe(uia: &IUIAutomation) -> bool {
    let before_text = read_focus_text(uia);
    let before_caret = caret_rect();
    input::unicode_char(PROBE_CHAR);
    std::thread::sleep(Duration::from_millis(150));
    let after_text = read_focus_text(uia);
    let after_caret = caret_rect();

    let mut inserted = after_text.as_deref().unwrap_or("") != before_text.as_deref().unwrap_or("");
    if !inserted && before_caret.is_some() && after_caret != before_caret {
        inserted = true;
    }
    if inserted {
        input::backspace();
        std::thread::sleep(Duration::from_millis(50));
    }
    log::info!(
        "Type probe: inserted={inserted} (text_signal={}, caret_signal={})",
        before_text.is_some() || after_text.is_some(),
        before_caret.is_some() || after_caret.is_some()
    );
    inserted
}

/// 综合判定：UIA 快路径 → 探针实测兜底
/// 选中的文字：Windows 上用复制获取（见 clipboard.rs），这里不读
pub fn selected_text() -> Option<String> {
    None
}

pub fn can_type_here() -> Typable {
    let _com = ComGuard::new();
    let uia = match uia() {
        Ok(uia) => uia,
        Err(e) => {
            log::error!("can_type_here error: {e} (assume editable)");
            return Typable::Yes;
        }
    };
    (is_focused_editable(&uia) || try_type_probe(&uia)).into()
}

/// 最多读取光标前 / 后多少个字符（只取光标附近，长文档也不会慢）
const CONTEXT_BEFORE: i32 = 600;
const CONTEXT_AFTER: i32 = 300;

/// 读取当前焦点输入框中光标前后的文字；密码框、不支持 TextPattern 的控件返回 None
pub fn caret_context() -> Option<CaretContext> {
    let _com = ComGuard::new();
    let uia = uia().ok()?;
    let elem = unsafe { uia.GetFocusedElement() }.ok()?;
    context_of(&elem)
}

pub(crate) fn context_of(elem: &IUIAutomationElement) -> Option<CaretContext> {
    // 绝不读取密码框，避免把密码发给 LLM
    if prop_bool(elem, UIA_IsPasswordPropertyId) {
        return None;
    }
    unsafe {
        // 经典 Win32 Edit（老程序、WinForms、对话框）不提供 TextPattern，改用窗口消息读取
        let Some(pattern) = elem.GetCurrentPattern(UIA_TextPatternId).ok() else {
            return edit_context(elem);
        };
        let tp = pattern.cast::<IUIAutomationTextPattern>().ok()?;
        let selection = tp.GetSelection().ok()?;
        if selection.Length().ok()? < 1 {
            return None;
        }
        let caret = selection.GetElement(0).ok()?;

        // 光标前：收缩到选区起点，再把起点往前挪
        let before = caret.Clone().ok()?;
        before.MoveEndpointByRange(TextPatternRangeEndpoint_End, &caret, TextPatternRangeEndpoint_Start).ok()?;
        before.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -CONTEXT_BEFORE).ok()?;
        // 光标后：收缩到选区终点，再把终点往后挪
        let after = caret.Clone().ok()?;
        after.MoveEndpointByRange(TextPatternRangeEndpoint_Start, &caret, TextPatternRangeEndpoint_End).ok()?;
        after.MoveEndpointByUnit(TextPatternRangeEndpoint_End, TextUnit_Character, CONTEXT_AFTER).ok()?;

        let normalize = |s: String| s.replace("\r\n", "\n").replace('\r', "\n");
        Some(CaretContext {
            before: normalize(before.GetText(-1).ok()?.to_string()),
            after: normalize(after.GetText(-1).ok()?.to_string()),
        })
    }
}

/// 经典 Win32 Edit 控件：WM_GETTEXT 取全文、EM_GETSEL 取光标位置（这两个消息跨进程可用）。
/// 用带超时的 SendMessageTimeout，目标程序卡死也不会拖住我们
fn edit_context(elem: &IUIAutomationElement) -> Option<CaretContext> {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetWindowLongW, SendMessageTimeoutW, GWL_STYLE, SMTO_ABORTIFHUNG,
        WM_GETTEXT, WM_GETTEXTLENGTH,
    };
    const EM_GETSEL: u32 = 0x00B0;
    const ES_PASSWORD: i32 = 0x0020;

    unsafe {
        let hwnd = HWND(elem.CurrentNativeWindowHandle().ok()?.0);
        if hwnd.is_invalid() {
            return None;
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut class) as usize;
        if !String::from_utf16_lossy(&class[..n]).to_ascii_uppercase().contains("EDIT") {
            return None;
        }
        if GetWindowLongW(hwnd, GWL_STYLE) & ES_PASSWORD != 0 {
            return None;
        }
        let send = |msg: u32, w: usize, l: isize| -> Option<usize> {
            let mut result = 0usize;
            let ok = SendMessageTimeoutW(hwnd, msg, WPARAM(w), LPARAM(l), SMTO_ABORTIFHUNG, 200, Some(&mut result));
            (ok.0 != 0).then_some(result)
        };
        let len = send(WM_GETTEXTLENGTH, 0, 0)?;
        // EM_GETSEL 的返回值只有 16 位，超长文本拿不到准确位置
        if len > 0xFFFF {
            return None;
        }
        let mut buf = vec![0u16; len + 1];
        let copied = send(WM_GETTEXT, buf.len(), buf.as_mut_ptr() as isize)?;
        buf.truncate(copied);
        let sel = send(EM_GETSEL, 0, 0)?;
        let (start, end) = ((sel & 0xFFFF).min(buf.len()), ((sel >> 16) & 0xFFFF).min(buf.len()));

        let normalize = |v: &[u16]| String::from_utf16_lossy(v).replace("\r\n", "\n").replace('\r', "\n");
        let from = start.saturating_sub(CONTEXT_BEFORE as usize);
        let to = (end + CONTEXT_AFTER as usize).min(buf.len());
        Some(CaretContext { before: normalize(&buf[from..start]), after: normalize(&buf[end.max(start)..to]) })
    }
}

/// 读取全文的上限（再长的文档就不跟踪了）
const MAX_TEXT: i32 = 20_000;

/// 跟踪某个输入框的全文（学习用户对识别结果的修改，见 learn.rs）。
/// UIA 对象只能在创建它的线程里用，所以由跟踪线程自己创建并持有
pub struct TextWatch {
    _com: ComGuard,
    uia: IUIAutomation,
    elem: IUIAutomationElement,
}

impl TextWatch {
    /// 跟踪当前焦点控件；密码框、读不到文字的控件返回 None
    pub fn focused() -> Option<Self> {
        let com = ComGuard::new();
        let uia = uia().ok()?;
        let elem = unsafe { uia.GetFocusedElement() }.ok()?;
        if prop_bool(&elem, UIA_IsPasswordPropertyId) {
            return None;
        }
        let watch = Self { _com: com, uia, elem };
        watch.text().map(|_| watch)
    }

    /// 测试用：跟踪指定窗口（不需要它有焦点）
    #[cfg(test)]
    fn of_window(hwnd: windows::Win32::Foundation::HWND) -> Option<Self> {
        let com = ComGuard::new();
        let uia = uia().ok()?;
        let elem = unsafe { uia.ElementFromHandle(hwnd) }.ok()?;
        if prop_bool(&elem, UIA_IsPasswordPropertyId) {
            return None;
        }
        Some(Self { _com: com, uia, elem })
    }

    /// 控件当前的全文（换行统一为 \n）
    pub fn text(&self) -> Option<String> {
        let normalize = |s: String| s.replace("\r\n", "\n").replace('\r', "\n").replace('\u{a0}', " ");
        unsafe {
            if let Ok(p) = self.elem.GetCurrentPattern(UIA_TextPatternId) {
                if let Ok(tp) = p.cast::<IUIAutomationTextPattern>() {
                    if let Ok(t) = tp.DocumentRange().and_then(|r| r.GetText(MAX_TEXT)) {
                        return Some(normalize(t.to_string()));
                    }
                }
            }
            if let Ok(p) = self.elem.GetCurrentPattern(UIA_ValuePatternId) {
                if let Ok(vp) = p.cast::<IUIAutomationValuePattern>() {
                    if let Ok(v) = vp.CurrentValue() {
                        return Some(normalize(v.to_string()));
                    }
                }
            }
        }
        // 经典 Win32 Edit 不提供这两个模式，改用窗口消息读取
        edit_text(&self.elem).map(normalize)
    }

    /// 焦点是否还在这个控件上
    pub fn still_focused(&self) -> bool {
        unsafe {
            self.uia
                .GetFocusedElement()
                .and_then(|f| self.uia.CompareElements(&f, &self.elem))
                .is_ok_and(|same| same.as_bool())
        }
    }
}

/// 经典 Win32 Edit 的全文
fn edit_text(elem: &IUIAutomationElement) -> Option<String> {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetWindowLongW, SendMessageTimeoutW, GWL_STYLE, SMTO_ABORTIFHUNG, WM_GETTEXT,
        WM_GETTEXTLENGTH,
    };
    const ES_PASSWORD: i32 = 0x0020;
    unsafe {
        let hwnd = HWND(elem.CurrentNativeWindowHandle().ok()?.0);
        if hwnd.is_invalid() {
            return None;
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut class) as usize;
        if !String::from_utf16_lossy(&class[..n]).to_ascii_uppercase().contains("EDIT") || GetWindowLongW(hwnd, GWL_STYLE) & ES_PASSWORD != 0 {
            return None;
        }
        let send = |msg: u32, w: usize, l: isize| -> Option<usize> {
            let mut result = 0usize;
            let ok = SendMessageTimeoutW(hwnd, msg, WPARAM(w), LPARAM(l), SMTO_ABORTIFHUNG, 200, Some(&mut result));
            (ok.0 != 0).then_some(result)
        };
        let len = send(WM_GETTEXTLENGTH, 0, 0)?.min(MAX_TEXT as usize);
        let mut buf = vec![0u16; len + 1];
        let copied = send(WM_GETTEXT, buf.len(), buf.as_mut_ptr() as isize)?;
        buf.truncate(copied);
        Some(String::from_utf16_lossy(&buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::LoadLibraryW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, PostMessageW, SendMessageW, SetWindowTextW,
        TranslateMessage, MSG, WINDOW_STYLE, WM_CLOSE, WM_QUIT, WS_POPUP, WS_VISIBLE,
    };

    /// 这两个测试都在本进程里建控件再用 UIA 读取，并行时 UIA 偶尔取不到窗口；串行执行
    static UI_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const EM_SETSEL: u32 = 0x00B1;
    const ES_MULTILINE: u32 = 0x0004;
    const ES_PASSWORD: u32 = 0x0020;

    /// 在单独的线程里建一个屏幕外的输入控件（带消息循环），返回窗口句柄
    fn spawn_control(class: &'static str, text: &str, sel: (usize, usize), style: u32) -> HWND {
        let (tx, rx) = mpsc::channel();
        let text = text.to_string();
        std::thread::spawn(move || unsafe {
            if class == "RICHEDIT50W" {
                LoadLibraryW(w!("Msftedit.dll")).unwrap();
            }
            let hwnd = CreateWindowExW(
                Default::default(),
                &HSTRING::from(class),
                PCWSTR::null(),
                WS_POPUP | WS_VISIBLE | WINDOW_STYLE(style),
                -4000, -4000, 600, 200,
                None, None, None, None,
            )
            .unwrap();
            SetWindowTextW(hwnd, &HSTRING::from(text.as_str())).unwrap();
            SendMessageW(hwnd, EM_SETSEL, Some(WPARAM(sel.0)), Some(LPARAM(sel.1 as isize)));
            tx.send(hwnd.0 as isize).unwrap();
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
                if msg.message == WM_CLOSE {
                    break;
                }
            }
        });
        HWND(rx.recv().unwrap() as _)
    }

    fn read(hwnd: HWND) -> Option<CaretContext> {
        let _com = ComGuard::new();
        let uia = uia().unwrap();
        let elem = unsafe { uia.ElementFromHandle(hwnd) }.unwrap();
        let ctx = context_of(&elem);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            let _ = PostMessageW(Some(hwnd), WM_QUIT, WPARAM(0), LPARAM(0));
        }
        ctx
    }

    fn ctx(before: &str, after: &str) -> Option<CaretContext> {
        Some(CaretContext { before: before.into(), after: after.into() })
    }

    /// 学习用户修改时读取输入框全文：经典 Edit 和 RichEdit，改动后能读到新内容；密码框不读
    #[test]
    fn watches_full_text() {
        let _lock = UI_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for class in ["EDIT", "RICHEDIT50W"] {
            let h = spawn_control(class, "前文。我用思维到图整理需求", (0, 0), ES_MULTILINE);
            let w = TextWatch::of_window(h).unwrap();
            assert_eq!(w.text().as_deref(), Some("前文。我用思维到图整理需求"), "{class}");
            unsafe { SetWindowTextW(h, &HSTRING::from("前文。我用思维导图整理需求\r\n第二行")).unwrap() };
            assert_eq!(w.text().as_deref(), Some("前文。我用思维导图整理需求\n第二行"), "{class}");
            drop(w);
            let _ = read(h); // 关闭窗口
        }
        let h = spawn_control("EDIT", "secret", (0, 0), ES_PASSWORD);
        assert!(TextWatch::of_window(h).is_none_or(|w| w.text().is_none()));
        let _ = read(h);
    }

    /// 用真实的 Windows 输入控件验证光标前后文字的读取
    #[test]
    fn reads_text_around_caret() {
        let _lock = UI_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 普通 Edit：光标在中间
        let h = spawn_control("EDIT", "Hello world. Goodbye.", (6, 6), ES_MULTILINE);
        assert_eq!(read(h), ctx("Hello ", "world. Goodbye."));

        // RichEdit 中文：光标在逗号后
        let h = spawn_control("RICHEDIT50W", "今天下午三点开会，请准时参加。", (9, 9), ES_MULTILINE);
        assert_eq!(read(h), ctx("今天下午三点开会，", "请准时参加。"));

        // 有选中内容：选中部分不算前后文（粘贴会替换它）
        let h = spawn_control("EDIT", "Hello world. Goodbye.", (6, 12), ES_MULTILINE);
        assert_eq!(read(h), ctx("Hello ", " Goodbye."));

        // 密码框：绝不读取
        let h = spawn_control("EDIT", "secret-password", (3, 3), ES_PASSWORD);
        assert_eq!(read(h), None);

        // 长文本：只取光标附近
        let long: String = "a".repeat(1500) + &"b".repeat(1500);
        let h = spawn_control("EDIT", &long, (1500, 1500), ES_MULTILINE);
        let c = read(h).unwrap();
        assert_eq!(c.before, "a".repeat(CONTEXT_BEFORE as usize));
        assert_eq!(c.after, "b".repeat(CONTEXT_AFTER as usize));
    }
}
