//! 辅助功能 API（AXUIElement）的封装：读取焦点程序、焦点元素和它们的属性
//!
//! Chromium / Electron / Firefox 程序（Chrome、VS Code、Slack……）默认不对外提供辅助功能树（网页、界面里的输入框都读不到），
//! 要由读取方打开：
//! - 读取程序元素的 AXRole：Apple 建议程序以此判断有辅助工具在用。Firefox 121 起据此打开
//!   （Mozilla bug 1845364）；Chrome 据此打开基本模式，但 macOS 14 起可能不含网页内容
//! - Electron 程序认 AXManualAccessibility，设置后立即生效
//!
//! 不设置 VoiceOver 用的 AXEnhancedUserInterface（Chrome 的完整模式只认它）：它让程序一直处于读屏模式，
//! 在 claude.ai 等网页上会让输入框失去焦点，之后的粘贴都落空（OpenWhispr #1116）。
//! 所以浏览器的网页内容可能始终读不到，判断不了时由调用方按「不确定」处理（见 focus.rs）

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
/// 每隔多久重新给浏览器设置一次开关（以防浏览器自己关掉了辅助功能）
const BROWSER_REASSERT: Duration = Duration::from_secs(30);
/// 浏览器的焦点停在网页本身或整个窗口时，网页里的内容可能还没准备好：每隔多久再读一次、最多等多久
const WEB_AREA_POLL: Duration = Duration::from_millis(100);
const WEB_AREA_WAIT: Duration = Duration::from_millis(300);

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

/// Firefox 系浏览器的 bundle id（前缀）
const GECKO_BROWSERS: &[&str] = &[
    "org.mozilla.",
    "io.gitlab.librewolf-community",
    "app.zen-browser.",
    "net.waterfox.",
    "org.torproject.",
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Chromium,
    Gecko,
    Other,
}

impl Kind {
    fn of(pid: i32) -> Self {
        let Some(id) = bundle_id(pid) else { return Kind::Other };
        if CHROMIUM_BROWSERS.iter().any(|b| id.starts_with(b)) {
            Kind::Chromium
        } else if GECKO_BROWSERS.iter().any(|b| id.starts_with(b)) {
            Kind::Gecko
        } else {
            Kind::Other
        }
    }

    pub fn is_browser(self) -> bool {
        self != Kind::Other
    }

    /// 第一次读取后要等多久辅助功能树才准备好（浏览器按需建树）
    fn delay(self) -> Duration {
        match self {
            Kind::Chromium => Duration::from_millis(500),
            Kind::Gecko => Duration::from_millis(1000),
            Kind::Other => Duration::ZERO,
        }
    }
}

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
        self.attr_or_error(name).ok()
    }

    /// 读属性；失败时返回辅助功能 API 的错误码（写日志用）
    fn attr_or_error(&self, name: &str) -> Result<CF, i32> {
        let mut value: CFTypeRef = std::ptr::null();
        let err = unsafe { AXUIElementCopyAttributeValue(self.0.as_ptr(), CF::string(name).as_ptr(), &mut value) };
        if err == AX_SUCCESS {
            CF::owned(value).ok_or(err)
        } else {
            Err(err)
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
    kind: Kind,
    /// 第一次打开的时间：之后 kind.delay() 内辅助功能树可能还没准备好
    first_request: Instant,
    last_request: Instant,
}

static ENABLED: Mutex<Vec<Enabled>> = Mutex::new(Vec::new());

/// 打开程序的辅助功能树，返回程序类别和还要等多久才能读到（只有刚打开浏览器的辅助功能树时需要等）
fn enable_tree(app: &Element, pid: i32) -> (Kind, Duration) {
    let mut enabled = ENABLED.lock().unwrap();
    let e = match enabled.iter().position(|e| e.pid == pid) {
        Some(i) => &mut enabled[i],
        None => {
            let kind = Kind::of(pid);
            let role = app.string("AXRole");
            // Electron 的开关；其他程序不认这个属性，设置失败也无妨
            let manual = app.set_bool("AXManualAccessibility", true);
            log::info!("AX: enable accessibility tree of pid {pid} ({kind:?}): role={role:?}, manual={manual}");
            let now = Instant::now();
            enabled.push(Enabled { pid, kind, first_request: now, last_request: now });
            return (kind, kind.delay());
        }
    };
    if e.kind == Kind::Other {
        return (Kind::Other, Duration::ZERO);
    }
    // 隔一段时间再读一次角色，浏览器关掉了辅助功能树也能重新打开
    if e.last_request.elapsed() >= BROWSER_REASSERT {
        app.string("AXRole");
        e.last_request = Instant::now();
    }
    (e.kind, e.kind.delay().saturating_sub(e.first_request.elapsed()))
}

/// 程序的 bundle id
pub(super) fn bundle_id(pid: i32) -> Option<String> {
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

/// 前台程序的进程号，由系统（NSWorkspace）记录，不用问前台程序本身
pub(super) fn frontmost_pid() -> Option<i32> {
    objc2::rc::autoreleasepool(|_| unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return None;
        }
        let app: *mut AnyObject = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return None;
        }
        let pid: i32 = msg_send![app, processIdentifier];
        (pid > 0).then_some(pid)
    })
}

/// 当前焦点所在的程序、它的类别，以及还要等多久才能读到它的辅助功能树。
/// 前台程序用 NSWorkspace 判断：系统级元素的 AXFocusedApplication 要前台程序自己回答，
/// 辅助功能树没打开的 Chrome、Firefox 不回答，就连是哪个程序都不知道了
fn focused_application_and_delay() -> Option<(Element, Kind, Duration)> {
    // 系统级元素上设置的超时对所有元素生效，所以先创建它
    let system = Element::system_wide()?;
    let pid = frontmost_pid().or_else(|| match system.attr_or_error("AXFocusedApplication") {
        Ok(app) => Element(app).pid(),
        Err(err) => {
            log::info!("AX: no focused application (error {err})");
            None
        }
    })?;
    let app = Element::application(pid)?;
    let (kind, delay) = enable_tree(&app, pid);
    Some((app, kind, delay))
}

/// 当前焦点所在的程序
fn focused_application() -> Option<Element> {
    focused_application_and_delay().map(|(app, ..)| app)
}

/// 当前的键盘焦点元素
pub fn focused_element() -> Option<Element> {
    focused_element_and_kind().0
}

/// 当前的键盘焦点元素，以及焦点程序的类别。刚打开浏览器的辅助功能树时，先等它生效
/// （录音开始时显示悬浮条就会打开，通常等到转写完已经生效，不用再等）
pub fn focused_element_and_kind() -> (Option<Element>, Kind) {
    let Some((app, kind, delay)) = focused_application_and_delay() else {
        return (None, Kind::Other);
    };
    if !delay.is_zero() {
        log::info!("AX: waiting {} ms for the accessibility tree", delay.as_millis());
        std::thread::sleep(delay);
    }
    let read = || app.element("AXFocusedUIElement").or_else(|| Element::system_wide()?.element("AXFocusedUIElement"));
    let mut elem = read();
    if elem.is_none() {
        let err = app.attr_or_error("AXFocusedUIElement").err();
        log::info!("AX: {kind:?} app has no focused element (error {err:?})");
    }
    if !kind.is_browser() {
        return (elem, kind);
    }
    // 浏览器：焦点停在网页本身或整个窗口，可能是网页内容还没对外提供（浏览器按需构建），稍等再读
    let mut waited = Duration::ZERO;
    while waited < WEB_AREA_WAIT && elem.as_ref().is_none_or(is_page_level) {
        std::thread::sleep(WEB_AREA_POLL);
        waited += WEB_AREA_POLL;
        elem = read();
    }
    if !waited.is_zero() {
        let role = elem.as_ref().and_then(|e| e.string("AXRole"));
        log::info!("AX: browser focus after waiting {} ms: {role:?}", waited.as_millis());
    }
    (elem, kind)
}

/// 焦点只停在网页本身或整个窗口上：看不到网页里具体是哪个元素
pub fn is_page_level(elem: &Element) -> bool {
    matches!(elem.string("AXRole").as_deref(), Some("AXWebArea" | "AXWindow"))
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
