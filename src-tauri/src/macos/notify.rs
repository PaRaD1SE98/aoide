//! 系统通知（macOS：UserNotifications）
//!
//! Tauri 的通知插件在 macOS 上用的是早已弃用的 NSUserNotificationCenter：不申请通知权限，
//! Aoide 在前台（比如设置窗口开着）时通知会被系统直接丢掉。这里改用 UNUserNotificationCenter：
//! 启动时申请通知权限，Aoide 在前台时也照常显示。
//! 只有打包成 .app 运行时可用（开发模式下的裸程序没有 bundle，调用会抛异常），否则由调用方退回 Tauri 的插件

use super::cf::{CFTypeRef, CF};
use block2::{Block, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol};
use objc2::{class, define_class, msg_send, AnyThread};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

#[link(name = "UserNotifications", kind = "framework")]
extern "C" {}

const AUTHORIZE_SOUND: usize = 1 << 1;
const AUTHORIZE_ALERT: usize = 1 << 2;
const PRESENT_SOUND: usize = 1 << 1;
const PRESENT_LIST: usize = 1 << 3;
const PRESENT_BANNER: usize = 1 << 4;

define_class!(
    // SAFETY: NSObject 没有子类化的要求，Delegate 没有实现 Drop
    #[unsafe(super(NSObject))]
    #[name = "AoideNotificationDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    impl Delegate {
        /// Aoide 在前台时也显示通知（不实现这个方法时系统会直接丢掉）
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(&self, _center: *mut AnyObject, _notification: *mut AnyObject, handler: &Block<dyn Fn(usize)>) {
            handler.call((PRESENT_BANNER | PRESENT_LIST | PRESENT_SOUND,));
        }
    }
);

/// 通知中心；不在 .app 里运行时为 None
fn center() -> Option<*mut AnyObject> {
    static CENTER: OnceLock<usize> = OnceLock::new();
    let ptr = *CENTER.get_or_init(|| unsafe {
        let bundle: *mut AnyObject = msg_send![class!(NSBundle), mainBundle];
        let id: *mut AnyObject = if bundle.is_null() { std::ptr::null_mut() } else { msg_send![bundle, bundleIdentifier] };
        if id.is_null() {
            log::info!("Notifications: not running from an app bundle, using the Tauri plugin");
            return 0;
        }
        let center: *mut AnyObject = msg_send![class!(UNUserNotificationCenter), currentNotificationCenter];
        if center.is_null() {
            return 0;
        }
        // 通知中心只弱引用代理对象，这里让它常驻
        let delegate: Retained<Delegate> = msg_send![Delegate::alloc(), init];
        let delegate = Retained::into_raw(delegate) as *mut AnyObject;
        let _: () = msg_send![center, setDelegate: delegate];
        center as usize
    });
    (ptr != 0).then_some(ptr as *mut AnyObject)
}

/// NSError 的说明文字
unsafe fn describe(error: *mut AnyObject) -> String {
    if error.is_null() {
        return String::new();
    }
    let text: *mut AnyObject = msg_send![error, localizedDescription];
    CF::retained(text as CFTypeRef).and_then(|s| s.to_string()).unwrap_or_default()
}

/// 启动时调用：第一次运行时系统会询问是否允许 Aoide 发送通知
pub fn request_permission() {
    let Some(center) = center() else { return };
    let handler = RcBlock::new(|granted: Bool, error: *mut AnyObject| {
        let reason = unsafe { describe(error) };
        log::info!("Notification permission: granted={} {reason}", granted.as_bool());
    });
    unsafe {
        let _: () = msg_send![center, requestAuthorizationWithOptions: AUTHORIZE_ALERT | AUTHORIZE_SOUND, completionHandler: &*handler];
    }
}

/// 显示一条通知；不在 .app 里运行时返回 false
pub fn show(title: &str, body: &str) -> bool {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let Some(center) = center() else { return false };
    unsafe {
        let content: *mut AnyObject = msg_send![class!(UNMutableNotificationContent), new];
        // new 返回的对象归我们所有，用完释放
        let Some(_owner) = CF::owned(content as CFTypeRef) else { return false };
        let (title, body) = (CF::string(title), CF::string(body));
        let _: () = msg_send![content, setTitle: title.as_ptr() as *mut AnyObject];
        let _: () = msg_send![content, setBody: body.as_ptr() as *mut AnyObject];
        let id = CF::string(&format!("aoide-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed)));
        let request: *mut AnyObject = msg_send![
            class!(UNNotificationRequest),
            requestWithIdentifier: id.as_ptr() as *mut AnyObject,
            content: content,
            trigger: std::ptr::null_mut::<AnyObject>()
        ];
        let handler = RcBlock::new(|error: *mut AnyObject| {
            if !error.is_null() {
                log::warn!("Notification failed: {}", describe(error));
            }
        });
        let _: () = msg_send![center, addNotificationRequest: request, withCompletionHandler: &*handler];
    }
    true
}
