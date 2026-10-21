//! 焦点输入框：能否输入、光标前后的文字、跟踪全文（各平台的实现见 win/focus.rs、macos/focus.rs）

pub use crate::platform::focus::{can_type_here, caret_context, warm_up, TextWatch};

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
