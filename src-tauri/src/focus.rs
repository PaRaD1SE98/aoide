//! 焦点输入框：能否输入、光标前后的文字、跟踪全文（各平台的实现见 win/focus.rs、macos/focus.rs）

pub use crate::platform::focus::{can_type_here, caret_context, selected_text, warm_up, TextWatch};

/// 焦点处能否输入文字
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Typable {
    Yes,
    No,
    /// 判断不了（macOS 上 Chrome、Firefox 的网页内容读不到）：照样粘贴，文字留在剪贴板里备用
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Unknown,
}

impl From<bool> for Typable {
    fn from(yes: bool) -> Self {
        if yes {
            Typable::Yes
        } else {
            Typable::No
        }
    }
}

/// 光标前后的文字（光标处有选中内容时，不含选中部分：粘贴会替换掉它）
#[derive(Debug, Default, PartialEq)]
pub struct CaretContext {
    pub before: String,
    pub after: String,
}

impl CaretContext {
    pub fn is_empty(&self) -> bool {
        self.before.trim().is_empty() && self.after.trim().is_empty()
    }
}
