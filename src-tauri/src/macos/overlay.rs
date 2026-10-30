//! 悬浮条窗口（macOS）
//!
//! 不激活（NSWindowStyleMaskNonactivatingPanel）、出现在所有桌面和全屏程序之上的 NSPanel，
//! 内容是窗口图层上的一张图片。绘制线程逐帧用 tiny-skia 画好（见 overlay.rs），
//! 交给主线程贴到窗口上（AppKit 只能在主线程操作）；主线程卡顿时只是少画几帧。
//! 平时鼠标穿透；鼠标移到胶囊或展开后的按钮上时才接收鼠标（点了也不会激活 Aoide、抢走输入框的焦点）。
//! 悬停和点击按每帧读到的鼠标位置、左键状态判断（见 overlay.rs 的 Anim::pointer）

use super::cf::{CFDataCreate, CFTypeRef, CF};
use super::{on_main, CGPoint, CGRect, Screen};
use crate::overlay::{self, Anim, BOTTOM_GAP, FOLLOW_MS, FRAME_MS, HEIGHT, WINDOW_WIDTH};
use dispatch2::DispatchQueue;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const STYLE_NONACTIVATING_PANEL: usize = 1 << 7;
const BACKING_BUFFERED: usize = 2;
/// NSStatusWindowLevel：在普通窗口和浮动窗口之上
const STATUS_WINDOW_LEVEL: isize = 25;
/// 出现在所有桌面、不随桌面切换移动、不参与窗口循环、可以盖在全屏程序上
const COLLECTION_BEHAVIOR: usize = 1 << 0 | 1 << 4 | 1 << 6 | 1 << 8;
const ALPHA_PREMULTIPLIED_LAST: u32 = 1;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    static kCGColorSpaceSRGB: CFTypeRef;
    fn CGColorSpaceCreateWithName(name: CFTypeRef) -> CFTypeRef;
    fn CGEventCreate(source: CFTypeRef) -> CFTypeRef;
    fn CGEventGetLocation(event: CFTypeRef) -> CGPoint;
    fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
    fn CGDataProviderCreateWithCFData(data: CFTypeRef) -> CFTypeRef;
    fn CGImageCreate(
        width: usize,
        height: usize,
        bits_per_component: usize,
        bits_per_pixel: usize,
        bytes_per_row: usize,
        space: CFTypeRef,
        bitmap_info: u32,
        provider: CFTypeRef,
        decode: *const f64,
        should_interpolate: bool,
        intent: i32,
    ) -> CFTypeRef;
}

/// 悬浮窗（NSPanel），在主线程创建
static PANEL: AtomicUsize = AtomicUsize::new(0);

const COMBINED_SESSION_STATE: i32 = 0;
const MOUSE_BUTTON_LEFT: u32 = 0;
/// 状态变化的通知：隐藏期间绘制线程在这里等待
static WAKE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// 创建悬浮窗和绘制线程（整个进程调用一次）
pub fn init() {
    DispatchQueue::main().exec_async(|| unsafe {
        let panel = create_panel();
        PANEL.store(panel as usize, Ordering::SeqCst);
    });
    std::thread::Builder::new().name("overlay".into()).spawn(run).expect("spawn overlay thread");
}

/// 状态变了：唤醒绘制线程（任意线程可调用）
pub fn refresh() {
    let (flag, cvar) = &WAKE;
    *flag.lock().unwrap() = true;
    cvar.notify_one();
}

unsafe fn create_panel() -> *mut AnyObject {
    let rect = CGRect::new(0.0, 0.0, WINDOW_WIDTH as f64, HEIGHT as f64);
    let panel: *mut AnyObject = msg_send![class!(NSPanel), alloc];
    let panel: *mut AnyObject = msg_send![
        panel,
        initWithContentRect: rect,
        styleMask: STYLE_NONACTIVATING_PANEL,
        backing: BACKING_BUFFERED,
        defer: false
    ];
    if panel.is_null() {
        log::error!("Overlay window failed");
        return panel;
    }
    let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
    let _: () = msg_send![panel, setOpaque: false];
    let _: () = msg_send![panel, setBackgroundColor: clear];
    let _: () = msg_send![panel, setHasShadow: false];
    let _: () = msg_send![panel, setIgnoresMouseEvents: true];
    let _: () = msg_send![panel, setHidesOnDeactivate: false];
    let _: () = msg_send![panel, setReleasedWhenClosed: false];
    let _: () = msg_send![panel, setLevel: STATUS_WINDOW_LEVEL];
    let _: () = msg_send![panel, setCollectionBehavior: COLLECTION_BEHAVIOR];
    let view: *mut AnyObject = msg_send![panel, contentView];
    if !view.is_null() {
        let _: () = msg_send![view, setWantsLayer: true];
    }
    panel
}

/// 悬浮条在哪块屏幕、窗口位置（AppKit 坐标）
struct Place {
    screen: CGRect,
    frame: CGRect,
    scale: f64,
    /// 主屏高度：CoreGraphics 坐标（原点在左上）换算成 AppKit 坐标（原点在左下）用
    primary_height: f64,
}

/// 输入焦点所在屏幕可用区域的底部中间
fn place_at_focus() -> Option<Place> {
    let (Screen { frame, visible, scale }, primary_height) = super::focus_screen()?;
    let (w, h) = (WINDOW_WIDTH as f64, HEIGHT as f64);
    let x = visible.origin.x + (visible.size.width - w) / 2.0;
    let y = visible.origin.y + BOTTOM_GAP as f64;
    Some(Place { screen: frame, frame: CGRect::new(x.round(), y.round(), w, h), scale, primary_height })
}

/// 鼠标在窗口里的位置（逻辑坐标，原点在窗口左上角）和左键是否按着。CoreGraphics 的这两个调用不必在主线程
fn pointer(p: &Place) -> Option<(f32, f32, bool)> {
    unsafe {
        let event = CF::owned(CGEventCreate(std::ptr::null()))?;
        let at = CGEventGetLocation(event.as_ptr());
        let x = at.x - p.frame.origin.x;
        let y = (p.frame.origin.y + p.frame.size.height) - (p.primary_height - at.y);
        Some((x as f32, y as f32, CGEventSourceButtonState(COMBINED_SESSION_STATE, MOUSE_BUTTON_LEFT)))
    }
}

/// 窗口是否接收鼠标（不接收时点击落到下面的程序）
fn accept_mouse(accept: bool) {
    DispatchQueue::main().exec_async(move || unsafe {
        let panel = PANEL.load(Ordering::SeqCst) as *mut AnyObject;
        if !panel.is_null() {
            let _: () = msg_send![panel, setIgnoresMouseEvents: !accept];
        }
    });
}

/// 绘制线程：隐藏时等待状态变化，显示期间每帧画一张图交给主线程
fn run() {
    let mut anim = Anim::new();
    let mut place: Option<Place> = None;
    let mut last_follow = Instant::now();
    let mut raise = false;
    let mut accepting = false;
    loop {
        if !anim.visible {
            let (flag, cvar) = &WAKE;
            let mut woke = flag.lock().unwrap();
            while !*woke {
                woke = cvar.wait(woke).unwrap();
            }
            *woke = false;
        }
        if anim.show() {
            place = place_at_focus();
            last_follow = Instant::now();
            raise = true; // 每次显示都重新放到最前，压过后来出现的置顶窗口
        }
        if !anim.visible {
            continue;
        }
        if !anim.frame() {
            if accepting {
                accepting = false;
                accept_mouse(false);
            }
            on_main(|| unsafe {
                let panel = PANEL.load(Ordering::SeqCst) as *mut AnyObject;
                if !panel.is_null() {
                    let _: () = msg_send![panel, orderOut: std::ptr::null_mut::<AnyObject>()];
                }
            });
            continue;
        }
        // 跟随输入焦点换屏
        if overlay::current() != overlay::Mode::Hidden && last_follow.elapsed().as_millis() >= FOLLOW_MS {
            last_follow = Instant::now();
            if let Some(p) = place_at_focus() {
                let moved = place.as_ref().is_some_and(|old| old.screen != p.screen);
                if moved || place.is_none() {
                    place = Some(p);
                }
                if moved {
                    overlay::moved_screen();
                }
            }
        }
        if let Some(p) = &place {
            // 鼠标悬停、点按钮；只在鼠标落在胶囊或按钮上时接收鼠标
            if let Some((x, y, down)) = pointer(p) {
                if let Some(part) = anim.pointer(x, y, down) {
                    overlay::clicked(part);
                }
                let accept = anim.hit(x, y).is_some();
                if accept != accepting {
                    accepting = accept;
                    accept_mouse(accept);
                }
            }
            anim.scale = p.scale as f32;
            if let Some(image) = anim.draw().and_then(|pm| image(&pm)) {
                present(image, p.frame, p.scale, anim.alpha as f64, std::mem::take(&mut raise));
            }
        }
        std::thread::sleep(Duration::from_millis(FRAME_MS.into()));
    }
}

/// tiny-skia 的预乘 RGBA 像素 → CGImage
fn image(pm: &tiny_skia::Pixmap) -> Option<CF> {
    unsafe {
        let data = CF::owned(CFDataCreate(std::ptr::null(), pm.data().as_ptr(), pm.data().len() as isize))?;
        let provider = CF::owned(CGDataProviderCreateWithCFData(data.as_ptr()))?;
        let space = CF::owned(CGColorSpaceCreateWithName(kCGColorSpaceSRGB))?;
        let (w, h) = (pm.width() as usize, pm.height() as usize);
        CF::owned(CGImageCreate(w, h, 8, 32, w * 4, space.as_ptr(), ALPHA_PREMULTIPLIED_LAST, provider.as_ptr(), std::ptr::null(), false, 0))
    }
}

/// 在主线程上更新窗口：位置、图片、整体透明度（不等待）
fn present(image: CF, frame: CGRect, scale: f64, alpha: f64, raise: bool) {
    DispatchQueue::main().exec_async(move || unsafe {
        let panel = PANEL.load(Ordering::SeqCst) as *mut AnyObject;
        if panel.is_null() {
            return;
        }
        let _: () = msg_send![panel, setFrame: frame, display: false];
        let view: *mut AnyObject = msg_send![panel, contentView];
        let layer: *mut AnyObject = if view.is_null() { std::ptr::null_mut() } else { msg_send![view, layer] };
        if !layer.is_null() {
            let _: () = msg_send![layer, setContentsScale: scale];
            let _: () = msg_send![layer, setContents: image.as_ptr() as *mut AnyObject];
        }
        let _: () = msg_send![panel, setAlphaValue: alpha];
        let visible: bool = msg_send![panel, isVisible];
        if raise || !visible {
            let _: () = msg_send![panel, orderFrontRegardless];
        }
    });
}
