//! 辅助功能 API（AXUIElement）的封装：读取焦点程序、焦点元素和它们的属性
//!
//! Chromium / Electron 程序（Chrome、VS Code、Slack……）默认不对外提供辅助功能树（网页、界面里的输入框都读不到），
//! 要由读取方打开：
//! - Electron 程序认 AXManualAccessibility，设置后立即生效
//! - Chrome 等浏览器只认 VoiceOver 用的 AXEnhancedUserInterface，而且要等 2 秒才生效
//!   （Chromium 的 chrome_browser_application_mac.mm）。这个开关在别的程序上有副作用（窗口动画、窗口管理工具），
//!   所以只对已知的 Chromium 浏览器设置

use super::cf::{CFRange, CFTypeRef, CF};
use super::CGPoint;
use std::ffi::c_void;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const AX_SUCCESS: i32 = 0;
const VALUE_CGPOINT: u32 = 1;
const VALUE_CGSIZE: u32 = 2;
const VALUE_CFRANGE: u32 = 4;
/// 调用目标程序的超时（秒）：目标程序卡死也不会拖住我们
const TIMEOUT: f32 = 0.3;
/// Chromium 浏览器收到 AXEnhancedUserInterface 后 2 秒才打开辅助功能树，再留一点建树的时间
const CHROMIUM_DELAY: Duration = Duration::from_millis(2500);
/// 每隔多久重新给 Chromium 浏览器设置一次开关（以防浏览器自己关掉了辅助功能）
const CHROMIUM_REASSERT: Duration = Duration::from_secs(30);

/// Chromium 系浏览器的 bundle id（前缀）
const CHROMIUM_BROWSERS: &[&str] = &[
    "com.google.Chrome",
    "org.chromium.",
    "com.microsoft.edgemac",
    "com.brave.Browser",
    "com.vivaldi.Vivaldi",
    "com.operasoftware.Opera",
    "company.thebrowser.",
    "ai.perplexity.comet",
    "ru.yandex.desktop.yandex-browser",
    "com.naver.Whale",
];

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(element: CFTypeRef, attribute: CFTypeRef, value: *mut CFTypeRef) -> i32;
    fn AXUIElementSetAttributeValue(element: CFTypeRef, attribute: CFTypeRef, value: CFTypeRef) -> i32;
    fn AXUIElementIsAttributeSettable(element: CFTypeRef, attribute: CFTypeRef, settable: *mut u8) -> i32;
    fn AXUIElementCopyParameterizedAttributeValue(
        element: CFTypeRef,
        attribute: CFTypeRef,
        parameter: CFTypeRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementGetPid(element: CFTypeRef, pid: *mut i32) -> i32;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, timeout: f32) -> i32;
    fn AXValueCreate(kind: u32, value: *const c_void) -> CFTypeRef;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: CFTypeRef) -> CFTypeRef;
    fn CGEventGetLocation(event: CFTypeRef) -> CGPoint;
}

/// 一个界面元素（程序、窗口、输入框……）
#[derive(Clone)]
pub struct Element(CF);

impl Element {
    fn system_wide() -> Option<Self> {
        let e = Self(CF::owned(unsafe { AXUIElementCreateSystemWide() })?);
        // 对系统级元素设置的超时对所有元素生效
        unsafe { AXUIElementSetMessagingTimeout(e.0.as_ptr(), TIMEOUT) };
        Some(e)
    }

    fn application(pid: i32) -> Option<Self> {
        CF::owned(unsafe { AXUIElementCreateApplication(pid) }).map(Self)
    }

    pub fn attr(&self, name: &str) -> Option<CF> {
        let mut value: CFTypeRef = std::ptr::null();
        let err = unsafe { AXUIElementCopyAttributeValue(self.0.as_ptr(), CF::string(name).as_ptr(), &mut value) };
        if err == AX_SUCCESS {
            CF::owned(value)
        } else {
            None
        }
    }

    /// 值为另一个元素的属性（如 AXFocusedUIElement）
    pub fn element(&self, name: &str) -> Option<Element> {
        self.attr(name).map(Element)
    }

    pub fn string(&self, name: &str) -> Option<String> {
        self.attr(name)?.to_string()
    }

    pub fn bool(&self, name: &str) -> Option<bool> {
        self.attr(name)?.to_bool()
    }

    pub fn number(&self, name: &str) -> Option<i64> {
        self.attr(name)?.to_i64()
    }

    pub fn is_settable(&self, name: &str) -> bool {
        let mut settable = 0u8;
        let err = unsafe { AXUIElementIsAttributeSettable(self.0.as_ptr(), CF::string(name).as_ptr(), &mut settable) };
        err == AX_SUCCESS && settable != 0
    }

    fn set_bool(&self, name: &str, value: bool) -> bool {
        let v = unsafe { if value { super::cf::kCFBooleanTrue } else { super::cf::kCFBooleanFalse } };
        unsafe { AXUIElementSetAttributeValue(self.0.as_ptr(), CF::string(name).as_ptr(), v) == AX_SUCCESS }
    }

    /// 取 AXValue 包装的结构体（点、尺寸、范围）
    fn ax_value<T: Default>(&self, name: &str, kind: u32) -> Option<T> {
        let value = self.attr(name)?;
        let mut out = T::default();
        let ok = unsafe { AXValueGetValue(value.as_ptr(), kind, &mut out as *mut T as *mut c_void) };
        (ok != 0).then_some(out)
    }

    pub fn range(&self, name: &str) -> Option<CFRange> {
        self.ax_value(name, VALUE_CFRANGE)
    }

    pub fn point(&self, name: &str) -> Option<CGPoint> {
        self.ax_value(name, VALUE_CGPOINT)
    }

    pub fn size(&self, name: &str) -> Option<super::CGSize> {
        self.ax_value(name, VALUE_CGSIZE)
    }

    /// 文字中的一段（AXStringForRange，范围按 UTF-16 计）
    pub fn string_for_range(&self, range: CFRange) -> Option<String> {
        unsafe {
            let param = CF::owned(AXValueCreate(VALUE_CFRANGE, &range as *const CFRange as *const c_void))?;
            let mut value: CFTypeRef = std::ptr::null();
            let err = AXUIElementCopyParameterizedAttributeValue(
                self.0.as_ptr(),
                CF::string("AXStringForRange").as_ptr(),
                param.as_ptr(),
                &mut value,
            );
            if err != AX_SUCCESS {
                return None;
            }
            CF::owned(value)?.to_string()
        }
    }

    fn pid(&self) -> Option<i32> {
        let mut pid = 0;
        (unsafe { AXUIElementGetPid(self.0.as_ptr(), &mut pid) } == AX_SUCCESS).then_some(pid)
    }

    pub fn same_as(&self, other: &Element) -> bool {
        self.0.same_as(&other.0)
    }
}

/// 已经打开过辅助功能树的程序
struct Enabled {
    pid: i32,
    chromium: bool,
    /// 第一次给 Chromium 浏览器设置开关的时间：之后 CHROMIUM_DELAY 内辅助功能树还没打开
    first_request: Option<Instant>,
    last_request: Option<Instant>,
}

static ENABLED: Mutex<Vec<Enabled>> = Mutex::new(Vec::new());

/// 打开程序的辅助功能树，返回还要等多久才能读到（只有刚给 Chromium 浏览器设置完开关时需要等）
fn enable_tree(app: &Element, pid: i32) -> Duration {
    let mut enabled = ENABLED.lock().unwrap();
    let i = match enabled.iter().position(|e| e.pid == pid) {
        Some(i) => i,
        None => {
            // Electron 的开关；其他程序不认这个属性，设置失败也无妨
            let manual = app.set_bool("AXManualAccessibility", true);
            let chromium = bundle_id(pid).is_some_and(|id| CHROMIUM_BROWSERS.iter().any(|b| id.starts_with(b)));
            log::info!("AX: enable accessibility tree of pid {pid}: manual={manual}, chromium={chromium}");
            enabled.push(Enabled { pid, chromium, first_request: None, last_request: None });
            enabled.len() - 1
        }
    };
    let e = &mut enabled[i];
    if !e.chromium {
        return Duration::ZERO;
    }
    // 每次设置都会让浏览器重新开始 2 秒倒计时，所以不能每次读取都设置，隔一段时间重申一次
    if e.last_request.is_none_or(|t| t.elapsed() >= CHROMIUM_REASSERT) {
        let ok = app.set_bool("AXEnhancedUserInterface", true);
        let now = Instant::now();
        e.first_request.get_or_insert(now);
        e.last_request = Some(now);
        log::info!("AX: AXEnhancedUserInterface for pid {pid}: {ok}");
    }
    e.first_request.map_or(Duration::ZERO, |t| CHROMIUM_DELAY.saturating_sub(t.elapsed()))
}

/// 程序的 bundle id
fn bundle_id(pid: i32) -> Option<String> {
    objc2::rc::autoreleasepool(|_| unsafe {
        let app: *mut AnyObject = msg_send![class!(NSRunningApplication), runningApplicationWithProcessIdentifier: pid];
        if app.is_null() {
            return None;
        }
        // NSString 与 CFString 可以互相转换
        let id: *mut AnyObject = msg_send![app, bundleIdentifier];
        CF::retained(id as CFTypeRef)?.to_string()
    })
}

/// 当前焦点所在的程序，以及还要等多久才能读到它的辅助功能树
fn focused_application_and_delay() -> Option<(Element, Duration)> {
    let app = Element::system_wide()?.element("AXFocusedApplication")?;
    let Some(pid) = app.pid() else {
        return Some((app, Duration::ZERO));
    };
    let delay = enable_tree(&app, pid);
    // 用按进程号创建的元素：系统级元素上设置的超时也对它生效
    Some((Element::application(pid).unwrap_or(app), delay))
}

/// 当前焦点所在的程序
fn focused_application() -> Option<Element> {
    focused_application_and_delay().map(|(app, _)| app)
}

/// 当前的键盘焦点元素。刚给 Chromium 浏览器打开辅助功能树时，先等它生效
/// （录音开始时显示悬浮条就会打开，通常等到转写完已经生效，不用再等）
pub fn focused_element() -> Option<Element> {
    let (app, delay) = focused_application_and_delay()?;
    if !delay.is_zero() {
        log::info!("AX: waiting {} ms for the accessibility tree", delay.as_millis());
        std::thread::sleep(delay);
    }
    app.element("AXFocusedUIElement")
        .or_else(|| Element::system_wide()?.element("AXFocusedUIElement"))
}

/// 输入焦点大致所在的位置（左上角为原点的屏幕坐标）：焦点窗口的中心，读不到时用鼠标位置
pub fn focus_point() -> CGPoint {
    let window = focused_application().and_then(|app| app.element("AXFocusedWindow"));
    if let Some((pos, size)) = window.and_then(|w| Some((w.point("AXPosition")?, w.size("AXSize")?))) {
        return CGPoint { x: pos.x + size.width / 2.0, y: pos.y + size.height / 2.0 };
    }
    unsafe {
        let event = CGEventCreate(std::ptr::null());
        if event.is_null() {
            return CGPoint::default();
        }
        let event = CF::owned(event).unwrap();
        CGEventGetLocation(event.as_ptr())
    }
}
