//! 系统权限（macOS）：麦克风、辅助功能
//!
//! 首次打开（以及更新后权限失效）时按顺序请求，用户处理完一项再请求下一项，最后才打开设置窗口，
//! 免得窗口或下一个对话框盖住还没处理的那个（见 request_at_startup）：
//! 1. 麦克风：系统弹出「允许 / 不允许」，回答后得到结果
//! 2. 辅助功能：系统提示框提供「打开系统设置」和「拒绝」，只能由用户在系统设置里打开开关，
//!    提示框什么时候关掉程序也收不到通知，只能看屏幕上它的窗口还在不在
//!
//! 用户没有同意的权限，设置页顶部会列出来，点按钮再弹出提示 / 打开系统设置的对应页面，授权后页面自动更新（见 ui/app.js）

use crate::Permissions;
use block2::RcBlock;
use objc2::runtime::{AnyObject, Bool};
use objc2::{class, msg_send};
use super::cf::{CFTypeRef, CF};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {
    static AVMediaTypeAudio: *mut AnyObject;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFTypeRef;
}

const WINDOW_LIST_ON_SCREEN_ONLY: u32 = 1;
/// 显示辅助功能授权提示框的系统进程
const ACCESSIBILITY_PROMPT_OWNER: &str = "universalAccessAuthWarn";
/// 可能显示屏幕录制授权提示框的系统进程（不确定，认不出时按新出现的窗口判断，见 wait_for_prompt）
const SCREEN_PROMPT_OWNERS: [&str; 2] = ["universalAccessAuthWarn", "UserNotificationCenter"];
/// 系统设置（旧版叫系统偏好设置）
const SYSTEM_SETTINGS: &str = "com.apple.systempreferences";
/// 等待提示框出现、用户在提示框上点按钮、用户在系统设置里操作的最长时间：判断不了时也不至于一直不开窗口
const PROMPT_APPEAR_WAIT: Duration = Duration::from_secs(3);
const PROMPT_ANSWER_WAIT: Duration = Duration::from_secs(300);
const SETTINGS_WAIT: Duration = Duration::from_secs(600);

const PANE_ACCESSIBILITY: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
const PANE_MICROPHONE: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone";
const PANE_SCREEN_RECORDING: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

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
    // 屏幕录制只在开着屏幕上下文时需要，由调用方按设置补上（见 lib.rs 的 permissions）
    Permissions { microphone: microphone(), accessibility: super::accessibility_trusted(false), screen_recording: true }
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

/// 是否已经弹过辅助功能 / 屏幕录制的系统提示（每次运行只弹一次，之后直接打开系统设置）
static PROMPTED: AtomicBool = AtomicBool::new(false);
static SCREEN_PROMPTED: AtomicBool = AtomicBool::new(false);

/// 启动时按顺序请求缺少的权限：先麦克风，回答后再辅助功能，等用户处理完提示框（以及点了「打开系统设置」后
/// 在那里的操作）才返回；开着屏幕上下文（screen）时最后请求屏幕录制。阻塞调用线程；返回后再打开设置窗口
pub fn request_at_startup(screen: bool) {
    request_microphone();
    if super::in_app_bundle() && !super::accessibility_trusted(false) {
        request_accessibility_at_startup();
    }
    // 首次安装时屏幕上下文默认关着，不会走到这里；更新后（旧条目已在 setup 时清掉）开着的话，
    // 也要等用户处理完提示框再打开窗口，免得提示框被窗口盖住
    if screen && super::in_app_bundle() && !super::screen::has_access() {
        request_screen_at_startup();
    }
}

/// 辅助功能：弹出系统提示框，等用户处理完
fn request_accessibility_at_startup() {
    let before = window_owners();
    PROMPTED.store(true, Ordering::SeqCst);
    // 弹出系统提示框，同时把 Aoide 加进辅助功能列表
    super::accessibility_trusted(true);
    wait_for_prompt("Accessibility", &before, &[ACCESSIBILITY_PROMPT_OWNER], || super::accessibility_trusted(false));
    log::info!("Accessibility permission after the startup prompt: {}", super::accessibility_trusted(false));
}

/// 屏幕录制：弹出系统提示框（提供「打开系统设置」），同时把 Aoide 加进屏幕录制列表，等用户处理完。
/// 在系统设置里打开后，要重新打开 Aoide 才生效（has_access 在这次运行里一直是 false），所以按提示框关掉、
/// 用户离开系统设置来判断处理完了
fn request_screen_at_startup() {
    log::info!("Screen Recording permission missing, asking for it");
    let before = window_owners();
    SCREEN_PROMPTED.store(true, Ordering::SeqCst);
    super::screen::request_access();
    wait_for_prompt("Screen Recording", &before, &SCREEN_PROMPT_OWNERS, super::screen::has_access);
}

/// 等用户处理完系统的授权提示框，以及点了「打开系统设置」后在那里的操作。
/// 提示框认 owners 里的进程名；都不是时，把请求后新出现窗口的进程当作提示框（提示框换了进程名也能等）。
/// granted：是否已经授权（授权了就不用再等）
fn wait_for_prompt(name: &str, before: &[String], owners: &[&str], granted: impl Fn() -> bool) {
    let me = super::bundle_name();
    let find = || {
        let now = window_owners();
        now.iter()
            .find(|o| owners.contains(&o.as_str()))
            .or_else(|| now.iter().find(|o| !before.contains(o) && Some(o.as_str()) != me.as_deref()))
            .cloned()
    };
    let start = Instant::now();
    let prompt = loop {
        if granted() {
            return;
        }
        if let Some(owner) = find() {
            break owner;
        }
        if start.elapsed() > PROMPT_APPEAR_WAIT {
            log::warn!("{name} prompt not found on screen");
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    log::info!("{name} prompt shown by {prompt}, waiting for the user");
    // 提示框还在：用户还没点「打开系统设置」或「拒绝」
    let start = Instant::now();
    while has_window_of(&prompt) && !granted() && start.elapsed() < PROMPT_ANSWER_WAIT {
        std::thread::sleep(Duration::from_millis(200));
    }
    // 点了「打开系统设置」：等用户在那里打开开关，或者离开系统设置
    std::thread::sleep(Duration::from_millis(800));
    let start = Instant::now();
    while !granted() && frontmost_is(SYSTEM_SETTINGS) && start.elapsed() < SETTINGS_WAIT {
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 屏幕上各窗口所属的进程名（只读进程名，不需要屏幕录制权限）
fn window_owners() -> Vec<String> {
    let Some(list) = CF::owned(unsafe { CGWindowListCopyWindowInfo(WINDOW_LIST_ON_SCREEN_ONLY, 0) }) else {
        return Vec::new();
    };
    (0..)
        .map_while(|i| list.array_item(i))
        .filter_map(|w| w.dictionary_value("kCGWindowOwnerName")?.to_string())
        .collect()
}

fn has_window_of(owner: &str) -> bool {
    window_owners().iter().any(|o| o == owner)
}

/// 前台程序是不是 bundle_id
fn frontmost_is(bundle_id: &str) -> bool {
    super::ax::frontmost_pid().and_then(super::ax::bundle_id).as_deref() == Some(bundle_id)
}

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
        "screen" => {
            // 第一次：系统提示框会把 Aoide 加进屏幕录制列表；之后直接打开系统设置的对应页面
            if SCREEN_PROMPTED.swap(true, Ordering::SeqCst) || super::screen::request_access() {
                let _ = super::open(PANE_SCREEN_RECORDING);
            }
        }
        _ => {}
    }
}
