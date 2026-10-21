//! 系统权限（macOS）：麦克风、辅助功能
//!
//! 麦克风可以由程序发起请求：系统弹出「允许 / 不允许」对话框，回答后得到结果。首次打开时先请求它，
//! 回答后才打开设置窗口，免得窗口盖住对话框（见 lib.rs）。
//! 辅助功能只能由用户在系统设置里打开，系统的提示框什么时候关掉程序也不知道；设置页顶部列出缺少的权限，
//! 点按钮时才弹出提示 / 打开系统设置的对应页面，授权后页面自动更新（见 ui/app.js）

use crate::Permissions;
use block2::RcBlock;
use objc2::runtime::{AnyObject, Bool};
use objc2::{class, msg_send};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {
    static AVMediaTypeAudio: *mut AnyObject;
}

const PANE_ACCESSIBILITY: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
const PANE_MICROPHONE: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone";

/// 麦克风授权状态：granted / denied / undetermined（还没问过）
fn microphone() -> &'static str {
    let status: isize = unsafe { msg_send![class!(AVCaptureDevice), authorizationStatusForMediaType: AVMediaTypeAudio] };
    match status {
        0 => "undetermined",
        3 => "granted",
        _ => "denied", // 1：受家长控制等限制；2：用户拒绝过
    }
}

pub fn status() -> Permissions {
    Permissions { microphone: microphone(), accessibility: super::accessibility_trusted(false) }
}

/// 还没问过麦克风权限时弹出系统对话框，并等到用户回答（阻塞调用线程）。
/// 不在 .app 里运行时（开发模式）不请求：缺少用途说明时系统会直接结束进程
pub fn request_microphone() {
    if !super::in_app_bundle() || microphone() != "undetermined" {
        return;
    }
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });
    unsafe {
        let _: () = msg_send![class!(AVCaptureDevice), requestAccessForMediaType: AVMediaTypeAudio, completionHandler: &*handler];
    }
    log::info!("Microphone permission: granted={}", rx.recv().unwrap_or(false));
}

/// 是否已经弹过辅助功能的系统提示（每次运行只弹一次，之后直接打开系统设置）
static PROMPTED: AtomicBool = AtomicBool::new(false);

/// 设置页上点了「允许」/「打开系统设置」
pub fn request(kind: &str) {
    match kind {
        "microphone" if microphone() == "undetermined" => request_microphone(),
        "microphone" => {
            let _ = super::open(PANE_MICROPHONE);
        }
        "accessibility" => {
            // 第一次：系统提示框会把 Aoide 加进辅助功能列表，并提供「打开系统设置」按钮
            if PROMPTED.swap(true, Ordering::SeqCst) || super::accessibility_trusted(true) {
                let _ = super::open(PANE_ACCESSIBILITY);
            }
        }
        _ => {}
    }
}
