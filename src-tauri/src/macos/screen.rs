//! 截取前台窗口的内容（屏幕上下文，见 screen.rs）（macOS）
//!
//! 用 ScreenCaptureKit 的 SCScreenshotManager（macOS 14 起）只截前台程序最前面的那个窗口：盖在上面的其他窗口、
//! Aoide 的悬浮条都不会进来。CGWindowListCreateImage 在 macOS 14 已弃用，15 的 SDK 里不能再用；
//! 更早的系统没有 SCScreenshotManager，不提供截图。
//!
//! 需要「屏幕录制」权限：打开屏幕上下文时请求（见 request_access）。没有权限时不截图，录音时也不弹窗。
//! 系统设置里授权后，可能要重新打开 Aoide 才生效

use super::cf::{CFTypeRef, CF};
use super::CGRect;
use crate::screen::Shot;
use block2::RcBlock;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::msg_send;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFTypeRef;
    fn CGImageGetWidth(image: CFTypeRef) -> usize;
    fn CGImageGetHeight(image: CFTypeRef) -> usize;
    fn CGColorSpaceCreateDeviceRGB() -> CFTypeRef;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CFTypeRef,
        bitmap_info: u32,
    ) -> CFTypeRef;
    fn CGContextDrawImage(context: CFTypeRef, rect: CGRect, image: CFTypeRef);
}

#[link(name = "ScreenCaptureKit", kind = "framework")]
extern "C" {}

const WINDOW_LIST_ON_SCREEN_ONLY: u32 = 1;
const WINDOW_LIST_EXCLUDE_DESKTOP: u32 = 16;
/// 每像素 4 字节 RGBX（不要透明度）
const IMAGE_ALPHA_NONE_SKIP_LAST: u32 = 5;
/// 等 ScreenCaptureKit 交回截图的最长时间
const WAIT: Duration = Duration::from_secs(2);

/// 是否有屏幕录制权限
pub fn has_access() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// 请求屏幕录制权限（打开屏幕上下文时调用）：没有的话系统会提示用户去系统设置里打开，并把 Aoide 加进列表。
/// 返回现在是否有权限
pub fn request_access() -> bool {
    has_access() || unsafe { CGRequestScreenCaptureAccess() }
}

pub fn capture_window() -> Option<Shot> {
    if !has_access() {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            log::warn!("Screen context: no Screen Recording permission, not taking screenshots");
        }
        return None;
    }
    let pid = super::ax::frontmost_pid()?;
    if pid == std::process::id() as i32 {
        return None; // 前台是 Aoide 自己的窗口（设置页），没有可参考的内容
    }
    let window = front_window(pid)?;
    let (Some(content_class), Some(filter_class), Some(config_class), Some(manager_class)) = (
        AnyClass::get(c"SCShareableContent"),
        AnyClass::get(c"SCContentFilter"),
        AnyClass::get(c"SCStreamConfiguration"),
        AnyClass::get(c"SCScreenshotManager"),
    ) else {
        log::info!("Screen context: ScreenCaptureKit screenshots need macOS 14 or later");
        return None;
    };

    let (tx, rx) = mpsc::channel::<Option<Shot>>();
    let handler = RcBlock::new(move |content: *mut AnyObject, _error: *mut AnyObject| unsafe {
        let Some(target) = find_window(content, window) else {
            let _ = tx.send(None);
            return;
        };
        let filter: *mut AnyObject = msg_send![filter_class, alloc];
        let filter: *mut AnyObject = msg_send![filter, initWithDesktopIndependentWindow: target];
        // 按窗口在屏幕上的实际像素截：内容区（点）× 每点的像素数
        let rect: CGRect = msg_send![filter, contentRect];
        let scale: f32 = msg_send![filter, pointPixelScale];
        let config: *mut AnyObject = msg_send![config_class, new];
        let _: () = msg_send![config, setWidth: (rect.size.width * scale as f64).round() as usize];
        let _: () = msg_send![config, setHeight: (rect.size.height * scale as f64).round() as usize];
        let _: () = msg_send![config, setShowsCursor: Bool::NO];
        let tx = tx.clone();
        // 截图只在回调期间有效：在回调里转成像素
        let done = RcBlock::new(move |image: CFTypeRef, _error: *mut AnyObject| {
            let _ = tx.send(to_shot(image));
        });
        let _: () = msg_send![manager_class, captureImageWithFilter: filter, configuration: config, completionHandler: &*done];
        let _: () = msg_send![filter, release];
        let _: () = msg_send![config, release];
    });
    unsafe {
        let _: () = msg_send![
            content_class,
            getShareableContentExcludingDesktopWindows: Bool::YES,
            onScreenWindowsOnly: Bool::YES,
            completionHandler: &*handler
        ];
    }
    rx.recv_timeout(WAIT).ok().flatten()
}

/// 程序 pid 最前面的普通窗口（第 0 层）的编号：屏幕上的窗口列表按从前到后排列
fn front_window(pid: i32) -> Option<u32> {
    let list = CF::owned(unsafe { CGWindowListCopyWindowInfo(WINDOW_LIST_ON_SCREEN_ONLY | WINDOW_LIST_EXCLUDE_DESKTOP, 0) })?;
    let number = |w: &CF, key: &str| w.dictionary_value(key).and_then(|v| v.to_i64());
    let window = (0..)
        .map_while(|i| list.array_item(i))
        .find(|w| number(w, "kCGWindowOwnerPID") == Some(pid as i64) && number(w, "kCGWindowLayer") == Some(0))?;
    number(&window, "kCGWindowNumber").map(|n| n as u32)
}

/// SCShareableContent 里编号为 window_id 的 SCWindow
unsafe fn find_window(content: *mut AnyObject, window_id: u32) -> Option<*mut AnyObject> {
    if content.is_null() {
        return None;
    }
    let windows: *mut AnyObject = msg_send![content, windows];
    let count: usize = msg_send![windows, count];
    (0..count).map(|i| -> *mut AnyObject { msg_send![windows, objectAtIndex: i] }).find(|&w| {
        let id: u32 = msg_send![w, windowID];
        id == window_id
    })
}

/// CGImage 转成 RGB 像素再编码（见 Shot::from_rgb）
fn to_shot(image: CFTypeRef) -> Option<Shot> {
    if image.is_null() {
        return None;
    }
    unsafe {
        let (w, h) = (CGImageGetWidth(image), CGImageGetHeight(image));
        if w < 50 || h < 50 {
            return None;
        }
        let mut rgbx = vec![0u8; w * h * 4];
        let space = CF::owned(CGColorSpaceCreateDeviceRGB())?;
        let context = CF::owned(CGBitmapContextCreate(rgbx.as_mut_ptr().cast(), w, h, 8, w * 4, space.as_ptr(), IMAGE_ALPHA_NONE_SKIP_LAST))?;
        CGContextDrawImage(context.as_ptr(), CGRect::new(0.0, 0.0, w as f64, h as f64), image);
        drop(context);
        let rgb = rgbx.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        Shot::from_rgb(w as u32, h as u32, rgb)
    }
}
