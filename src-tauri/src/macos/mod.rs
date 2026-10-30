//! macOS 平台实现。与 win/mod.rs 提供相同的接口，其他模块统一通过 crate::platform 调用
//!
//! 需要两项系统权限：辅助功能（监听和拦截热键、模拟粘贴、读取焦点输入框）和麦克风。
//! 系统 API 大多直接声明 C 函数调用（CoreFoundation / CoreGraphics / 辅助功能 / CoreAudio），
//! AppKit 对象用 objc2 的 msg_send! 收发消息

mod ax;
mod cf;
pub mod focus;
pub mod hook;
pub mod input;
mod keycode;
pub mod mute;
pub mod overlay;
pub mod permissions;
pub mod popup;
pub mod screen;
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

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn LMGetKbdType() -> u8;
    fn KBGetLayoutType(keyboard_type: i16) -> u32;
}

/// 进程启动时最先调用；macOS 上没有要做的
pub fn prepare() {}

/// Tauri 应用创建后、创建任何窗口前调用
pub fn setup(app: &mut App) {
    // 菜单栏程序：平时不在程序坞显示图标，打开设置窗口时才显示（见 main_window_shown）
    app.set_activation_policy(ActivationPolicy::Accessory);
    // 这里不弹系统的授权提示（会被随后打开的设置窗口盖住），由设置页引导用户授权（见 permissions.rs）；
    // 键盘钩子会一直等到授权（见 hook.rs）
    if !accessibility_trusted(false) {
        log::warn!("Accessibility permission not granted yet");
        remove_stale_entry("Accessibility");
    }
    if !screen::has_access() {
        remove_stale_entry("ScreenCapture");
    }
}

/// 清掉系统设置里 Aoide 失效的旧授权条目（service：「辅助功能」是 Accessibility，「屏幕录制」是 ScreenCapture）。
///
/// 程序是临时签名（ad hoc）的，每个版本的签名都不同，而系统按签名认授权：更新后旧条目还在列表里、
/// 开关也开着，却对新版本无效，用户会看到「明明开着却还要授权」，在旧条目上开关也没用。
/// 没有授权时先删掉旧条目，用户在设置页点授权时系统会为新版本重新加一条（见 permissions.rs）。
/// 没有授权时列表里 Aoide 的条目都是无效的，删掉没有损失
fn remove_stale_entry(service: &str) {
    let Some(id) = bundle_identifier() else { return };
    match std::process::Command::new("/usr/bin/tccutil")
        .args(["reset", service, &id])
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => log::info!("Reset {service} entry for {id}"),
        Ok(out) => log::warn!("tccutil reset {service} failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
        Err(e) => log::warn!("tccutil reset {service} failed: {e}"),
    }
}

/// 打包好的 .app 的标识；开发模式下的裸程序没有 bundle，返回 None
fn bundle_identifier() -> Option<String> {
    unsafe {
        let bundle: *mut AnyObject = msg_send![class!(NSBundle), mainBundle];
        if bundle.is_null() {
            return None;
        }
        // NSString 与 CFString 可以互相转换
        let id: *mut AnyObject = msg_send![bundle, bundleIdentifier];
        CF::retained(id as CFTypeRef)?.to_string()
    }
}

/// 本程序的进程名（窗口列表里显示的所属程序名）
pub(super) fn bundle_name() -> Option<String> {
    unsafe {
        let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
        let name: *mut AnyObject = msg_send![info, processName];
        CF::retained(name as CFTypeRef)?.to_string()
    }
}

/// 是否作为打包好的 .app 运行
fn in_app_bundle() -> bool {
    bundle_identifier().is_some()
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

/// 系统首选语言：中文 zh、日语 ja，其他 en
pub fn system_language() -> &'static str {
    let first = CF::owned(unsafe { CFLocaleCopyPreferredLanguages() })
        .and_then(|langs| langs.array_item(0))
        .and_then(|lang| lang.to_string())
        .unwrap_or_default();
    if first.starts_with("zh") {
        "zh"
    } else if first.starts_with("ja") {
        "ja"
    } else {
        "en"
    }
}

/// 是否日文键盘（JIS 布局：空格右侧是「かな」和右 Command，没有右 Option）
pub fn jis_keyboard() -> bool {
    const LAYOUT_JIS: u32 = u32::from_be_bytes(*b"JIS ");
    unsafe { KBGetLayoutType(i16::from(LMGetKbdType())) == LAYOUT_JIS }
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

/// 更新装好后调用：等本进程退出后，再重新打开 Aoide.app（调用方随后退出）。
/// 不用 Tauri 的 restart：它在本进程退出前就启动新实例，新实例的单实例检查会把自己交给还没退出的旧进程，
/// 结果更新装好了却没有重新打开
pub fn relaunch_after_exit() {
    use std::os::unix::process::CommandExt;
    let bundle = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().nth(3).map(std::path::Path::to_path_buf)) // Aoide.app/Contents/MacOS/aoide
        .filter(|b| b.extension().is_some_and(|e| e == "app"));
    let Some(bundle) = bundle else {
        log::warn!("Relaunch skipped: not running from an app bundle");
        return;
    };
    // 程序包路径作为 $0 传给脚本，不用处理引号；放进单独的进程组，本进程退出时不受影响
    let script = format!("while kill -0 {} 2>/dev/null; do sleep 0.2; done; /usr/bin/open \"$0\"", std::process::id());
    let spawned = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg(&bundle)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn();
    match spawned {
        Ok(_) => log::info!("Will reopen {} after exiting", bundle.display()),
        Err(e) => log::error!("Relaunch failed: {e}"),
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
