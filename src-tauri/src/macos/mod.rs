//! macOS 平台实现。与 win/mod.rs 提供相同的接口，其他模块统一通过 crate::platform 调用
//!
//! 需要两项系统权限：辅助功能（监听和拦截热键、模拟粘贴、读取焦点输入框）和麦克风；另外申请通知权限。
//! 系统 API 大多直接声明 C 函数调用（CoreFoundation / CoreGraphics / 辅助功能 / CoreAudio），
//! AppKit 对象用 objc2 的 msg_send! 收发消息

mod ax;
mod cf;
pub mod focus;
pub mod hook;
pub mod input;
mod keycode;
pub mod mute;
pub mod notify;
pub mod overlay;
pub mod popup;
pub mod sound;

use cf::{CFTypeRef, CF};
use dispatch2::DispatchQueue;
use objc2::encode::{Encode, Encoding};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send, MainThreadMarker};
use tauri::{ActivationPolicy, App, AppHandle, Manager};

// AppKit 由 Tauri（tao）链接，这里声明一下，保证 NSPanel、NSSound 等类在运行时可用
#[link(name = "AppKit", kind = "framework")]
extern "C" {}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> u8;
    static kAXTrustedCheckOptionPrompt: CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFLocaleCopyPreferredLanguages() -> CFTypeRef;
}

/// 进程启动时最先调用；macOS 上没有要做的
pub fn prepare() {}

/// Tauri 应用创建后、创建任何窗口前调用
pub fn setup(app: &mut App) {
    // 菜单栏程序：平时不在程序坞显示图标，打开设置窗口时才显示（见 main_window_shown）
    app.set_activation_policy(ActivationPolicy::Accessory);
    // 没有辅助功能权限时弹出系统的授权提示；键盘钩子会一直等到授权（见 hook.rs）
    if !accessibility_trusted(true) {
        log::warn!("Accessibility permission not granted yet");
    }
    notify::request_permission();
}

/// 主窗口（设置）打开：显示程序坞图标和菜单栏，窗口能正常切换、复制粘贴
pub fn main_window_shown(app: &AppHandle) {
    let _ = app.set_activation_policy(ActivationPolicy::Regular);
    // 新建的窗口不会激活程序：Aoide 在后台时窗口会被压在当前程序后面
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.set_focus();
    }
}

/// 主窗口关闭：回到只在菜单栏显示
pub fn main_window_closed(app: &AppHandle) {
    let _ = app.set_activation_policy(ActivationPolicy::Accessory);
}

/// 是否已获得「辅助功能」权限；prompt 为 true 且没有权限时弹出系统的授权提示
pub fn accessibility_trusted(prompt: bool) -> bool {
    unsafe {
        let value = if prompt { cf::kCFBooleanTrue } else { cf::kCFBooleanFalse };
        let options = CF::dictionary(&[(kAXTrustedCheckOptionPrompt, value)]);
        AXIsProcessTrustedWithOptions(options.as_ptr()) != 0
    }
}

/// 系统首选语言是否为中文
pub fn system_language() -> &'static str {
    let first = CF::owned(unsafe { CFLocaleCopyPreferredLanguages() })
        .and_then(|langs| langs.array_item(0))
        .and_then(|lang| lang.to_string());
    if first.is_some_and(|l| l.starts_with("zh")) {
        "zh"
    } else {
        "en"
    }
}

/// 剪贴板内容每次改变都会变化的序号
pub fn clipboard_sequence() -> u64 {
    unsafe {
        let pasteboard: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
        if pasteboard.is_null() {
            return 0;
        }
        let count: isize = msg_send![pasteboard, changeCount];
        count as u64
    }
}

/// 用默认程序打开文件、文件夹或网址
pub fn open(target: impl AsRef<std::ffi::OsStr>) -> std::io::Result<()> {
    std::process::Command::new("open").arg(target).spawn().map(|_| ())
}

/// 在主线程上执行并等待结果（AppKit 的窗口、屏幕只能在主线程访问）
fn on_main<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    if MainThreadMarker::new().is_some() {
        return f();
    }
    let mut out = None;
    DispatchQueue::main().exec_sync(|| out = Some(f()));
    out.expect("main queue ran the closure")
}

/// 屏幕坐标里的点、尺寸、矩形（单位是点，不是像素）。
/// AppKit 的坐标原点在主屏左下角、y 轴向上；辅助功能和 CoreGraphics 的原点在主屏左上角、y 轴向下
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

unsafe impl Encode for CGRect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}

impl CGRect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { origin: CGPoint { x, y }, size: CGSize { width, height } }
    }

    fn contains(&self, p: CGPoint) -> bool {
        p.x >= self.origin.x
            && p.x < self.origin.x + self.size.width
            && p.y >= self.origin.y
            && p.y < self.origin.y + self.size.height
    }
}

/// 一块屏幕（AppKit 坐标）
#[derive(Clone, Copy, Debug)]
pub struct Screen {
    pub frame: CGRect,
    /// 去掉菜单栏和程序坞后的可用区域
    pub visible: CGRect,
    /// 像素 / 点
    pub scale: f64,
}

/// 所有屏幕，第一块是主屏（有菜单栏、坐标原点所在的那块）
fn screens() -> Vec<Screen> {
    on_main(|| unsafe {
        let list: *mut AnyObject = msg_send![class!(NSScreen), screens];
        if list.is_null() {
            return Vec::new();
        }
        let n: usize = msg_send![list, count];
        (0..n)
            .map(|i| {
                let screen: *mut AnyObject = msg_send![list, objectAtIndex: i];
                Screen {
                    frame: msg_send![screen, frame],
                    visible: msg_send![screen, visibleFrame],
                    scale: msg_send![screen, backingScaleFactor],
                }
            })
            .collect()
    })
}

/// 输入焦点所在的屏幕及主屏高度（用来在两种坐标之间换算）：焦点窗口 → 鼠标所在屏幕 → 主屏
fn focus_screen() -> Option<(Screen, f64)> {
    let point = ax::focus_point();
    let screens = screens();
    let primary_height = screens.first()?.frame.size.height;
    // 换成 AppKit 坐标再找所在屏幕
    let p = CGPoint { x: point.x, y: primary_height - point.y };
    let screen = screens.iter().find(|s| s.frame.contains(p)).unwrap_or(&screens[0]);
    Some((*screen, primary_height))
}
