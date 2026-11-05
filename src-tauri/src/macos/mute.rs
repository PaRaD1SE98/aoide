//! 录音期间静音其他程序的声音，结束后恢复（macOS）
//!
//! macOS 没有 Windows 音量合成器那样按程序静音的接口，这里静音的是默认输出设备：
//! 只在它原本没被静音时静音，恢复时只取消我们静音的设备，用户原本就静音的保持原样。
//! 开始提示音播完后才静音，结束时先恢复再播结束音（见 controller.rs），提示音都听得到。
//! 被静音的设备同时记在文件里：程序被强退来不及恢复时，下次启动按文件恢复。
//!
//! 蓝牙耳机的麦克风一打开，耳机就从音乐模式切到通话模式，麦克风关掉后再切回来。
//! 切换要一两秒，期间设的静音可能被切换冲掉，也可能在切回音乐模式后才生效。所以：
//! - 录音期间一直盯着：设备的静音被冲掉了就再静音，默认输出换了设备也一并静音；
//! - 恢复后再盯几秒：被切换重新静音的，再取消一次。

use super::cf::{CFTypeRef, CF};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// 被我们静音的设备（设备 UID，重新插拔后也不变），恢复时按 UID 找回
static MUTED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// 当前是否应处于静音状态。快速开始又停止时，恢复可能抢在静音之前执行；
/// 静音在锁内检查这个标志，避免录音已结束却留下静音
static WANTED: AtomicBool = AtomicBool::new(false);
/// 每次开始或结束加一。录音期间和恢复后盯着设备的线程，发现它变了就退出
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// 录音期间检查设备静音状态的间隔
const WATCH_EVERY: Duration = Duration::from_millis(250);
/// 一次录音里，同一设备的静音被冲掉后最多再静音几次：用户自己按了取消静音时不和他一直较劲
const MAX_REMUTES: usize = 4;
/// 恢复后隔多久再检查一次（累计约 4 秒，蓝牙耳机切回音乐模式一般在这之内）
const RECHECK_AFTER: [u64; 5] = [200, 400, 700, 1200, 1500];

type AudioObjectId = u32;

#[repr(C)]
struct Address {
    selector: u32,
    scope: u32,
    element: u32,
}

const fn code(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SYSTEM_OBJECT: AudioObjectId = 1;
const DEFAULT_OUTPUT: u32 = code(b"dOut");
const DEVICES: u32 = code(b"dev#");
const DEVICE_UID: u32 = code(b"uid ");
const NAME: u32 = code(b"lnam");
const TRANSPORT: u32 = code(b"tran");
const MUTE: u32 = code(b"mute");
const SCOPE_GLOBAL: u32 = code(b"glob");
const SCOPE_OUTPUT: u32 = code(b"outp");
/// 主通道；没有主通道静音开关的设备逐个声道静音
const ELEMENTS: [u32; 3] = [0, 1, 2];

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyDataSize(id: AudioObjectId, addr: *const Address, qsize: u32, qdata: *const c_void, size: *mut u32) -> i32;
    fn AudioObjectGetPropertyData(id: AudioObjectId, addr: *const Address, qsize: u32, qdata: *const c_void, size: *mut u32, data: *mut c_void) -> i32;
    fn AudioObjectSetPropertyData(id: AudioObjectId, addr: *const Address, qsize: u32, qdata: *const c_void, size: u32, data: *const c_void) -> i32;
    fn AudioObjectHasProperty(id: AudioObjectId, addr: *const Address) -> u8;
    fn AudioObjectIsPropertySettable(id: AudioObjectId, addr: *const Address, settable: *mut u8) -> i32;
}

/// 读取定长的属性值
fn get<T: Default>(id: AudioObjectId, addr: &Address) -> Option<T> {
    let mut value = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    let status = unsafe { AudioObjectGetPropertyData(id, addr, 0, std::ptr::null(), &mut size, &mut value as *mut T as *mut c_void) };
    (status == 0).then_some(value)
}

fn default_output() -> Option<AudioObjectId> {
    let addr = Address { selector: DEFAULT_OUTPUT, scope: SCOPE_GLOBAL, element: 0 };
    get::<AudioObjectId>(SYSTEM_OBJECT, &addr).filter(|&id| id != 0)
}

fn all_devices() -> Vec<AudioObjectId> {
    let addr = Address { selector: DEVICES, scope: SCOPE_GLOBAL, element: 0 };
    let mut size = 0u32;
    unsafe {
        if AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &addr, 0, std::ptr::null(), &mut size) != 0 {
            return Vec::new();
        }
        let mut ids = vec![0 as AudioObjectId; size as usize / std::mem::size_of::<AudioObjectId>()];
        if AudioObjectGetPropertyData(SYSTEM_OBJECT, &addr, 0, std::ptr::null(), &mut size, ids.as_mut_ptr() as *mut c_void) != 0 {
            return Vec::new();
        }
        ids.truncate(size as usize / std::mem::size_of::<AudioObjectId>());
        ids
    }
}

/// 取字符串属性（UID、名称）
fn string_property(id: AudioObjectId, selector: u32) -> Option<String> {
    let addr = Address { selector, scope: SCOPE_GLOBAL, element: 0 };
    let s: CFTypeRef = get::<usize>(id, &addr)? as CFTypeRef;
    CF::owned(s)?.to_string()
}

fn device_uid(id: AudioObjectId) -> Option<String> {
    string_property(id, DEVICE_UID)
}

/// 日志里的设备描述：名称和连接方式（blue = 蓝牙，bltn = 内置，usb = USB）
fn describe(id: AudioObjectId) -> String {
    let name = string_property(id, NAME).unwrap_or_else(|| format!("device {id}"));
    let transport = get::<u32>(id, &Address { selector: TRANSPORT, scope: SCOPE_GLOBAL, element: 0 })
        .map(|t| String::from_utf8_lossy(&t.to_be_bytes()).trim().to_string())
        .unwrap_or_else(|| "?".into());
    format!("\"{name}\" ({transport})")
}

/// 设备上可以设置静音的通道：有主通道开关就只用它
fn mute_elements(id: AudioObjectId) -> Vec<u32> {
    let settable = |element| {
        let addr = Address { selector: MUTE, scope: SCOPE_OUTPUT, element };
        let mut ok = 0u8;
        unsafe { AudioObjectHasProperty(id, &addr) != 0 && AudioObjectIsPropertySettable(id, &addr, &mut ok) == 0 && ok != 0 }
    };
    if settable(ELEMENTS[0]) {
        return vec![ELEMENTS[0]];
    }
    ELEMENTS[1..].iter().copied().filter(|&e| settable(e)).collect()
}

fn is_muted(id: AudioObjectId, element: u32) -> bool {
    get::<u32>(id, &Address { selector: MUTE, scope: SCOPE_OUTPUT, element }).is_some_and(|m| m != 0)
}

fn set_muted(id: AudioObjectId, element: u32, muted: bool) -> bool {
    let addr = Address { selector: MUTE, scope: SCOPE_OUTPUT, element };
    let value = u32::from(muted);
    let size = std::mem::size_of::<u32>() as u32;
    unsafe { AudioObjectSetPropertyData(id, &addr, 0, std::ptr::null(), size, &value as *const u32 as *const c_void) == 0 }
}

/// 设备的所有静音通道都设为 muted；返回是否至少设成功一个
fn set_device(id: AudioObjectId, elements: &[u32], muted: bool) -> bool {
    elements.iter().fold(false, |any, &e| set_muted(id, e, muted) || any)
}

fn all_muted(id: AudioObjectId, elements: &[u32]) -> bool {
    !elements.is_empty() && elements.iter().all(|&e| is_muted(id, e))
}

fn any_muted(id: AudioObjectId, elements: &[u32]) -> bool {
    elements.iter().any(|&e| is_muted(id, e))
}

/// 录音开始：静音默认输出设备，并在录音期间盯着它
pub fn begin() {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    WANTED.store(true, Ordering::SeqCst);
    mute_output();
    std::thread::spawn(move || {
        let mut remutes = 0;
        loop {
            std::thread::sleep(WATCH_EVERY);
            if GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }
            if remutes < MAX_REMUTES && keep_muted() {
                remutes += 1;
            }
        }
    });
}

/// 录音结束：恢复（可重复调用），之后再盯几秒
pub fn end() {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    WANTED.store(false, Ordering::SeqCst);
    let restored = restore();
    if restored.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for ms in RECHECK_AFTER {
            std::thread::sleep(Duration::from_millis(ms));
            if GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }
            unmute_again(&restored);
        }
    });
}

/// 启动时调用：上次运行留下了未恢复的静音（被强退、崩溃），现在恢复
pub fn recover() {
    let Some(uids) = std::fs::read_to_string(pending_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
    else {
        let _ = std::fs::remove_file(pending_file());
        return;
    };
    log::info!("Recovering {} audio device(s) left muted by the last run", uids.len());
    MUTED.lock().unwrap().extend(uids);
    restore();
}

fn pending_file() -> PathBuf {
    crate::config::data_dir().join("muted-devices.json")
}

fn save_pending(uids: &[String]) {
    let _ = std::fs::create_dir_all(crate::config::data_dir());
    let json = serde_json::to_string(uids).expect("device uids are serializable");
    if let Err(e) = std::fs::write(pending_file(), json) {
        log::warn!("Save muted devices failed: {e}");
    }
}

fn mute_output() {
    let mut muted = MUTED.lock().unwrap();
    if !WANTED.load(Ordering::SeqCst) {
        return;
    }
    let Some(id) = default_output() else {
        log::warn!("Mute: no default output device");
        return;
    };
    mute_device(id, &mut muted);
}

/// 静音一个还没被我们静音的设备
fn mute_device(id: AudioObjectId, muted: &mut Vec<String>) {
    let Some(uid) = device_uid(id) else { return };
    if muted.contains(&uid) {
        return;
    }
    let elements = mute_elements(id);
    if elements.is_empty() {
        log::info!("Mute: output {} has no mute control", describe(id));
        return;
    }
    if all_muted(id, &elements) {
        log::info!("Mute: output {} is already muted", describe(id));
        return;
    }
    // 先落盘再静音：静音之后、写文件之前被强退也不会漏掉
    let mut pending = muted.clone();
    pending.push(uid.clone());
    save_pending(&pending);
    if set_device(id, &elements, true) {
        log::info!("Muted output {} while recording (reads back {})", describe(id), state(id, &elements));
        muted.push(uid);
    } else {
        log::warn!("Mute: setting mute on output {} failed", describe(id));
    }
}

/// 录音期间每隔一会儿调用：静音被冲掉的设备再静音，默认输出换成的新设备也静音。返回是否重新静音了
fn keep_muted() -> bool {
    let mut muted = MUTED.lock().unwrap();
    if !WANTED.load(Ordering::SeqCst) {
        return false;
    }
    let mut remuted = false;
    for id in all_devices() {
        if !device_uid(id).is_some_and(|uid| muted.contains(&uid)) {
            continue;
        }
        let elements = mute_elements(id);
        if !elements.is_empty() && !all_muted(id, &elements) && set_device(id, &elements, true) {
            log::info!("Output {} was unmuted while recording (Bluetooth mode switch?), muted it again", describe(id));
            remuted = true;
        }
    }
    if let Some(id) = default_output() {
        if device_uid(id).is_some_and(|uid| !muted.contains(&uid)) {
            log::info!("Default output changed to {} while recording", describe(id));
            mute_device(id, &mut muted);
        }
    }
    remuted
}

/// 恢复后每隔一会儿调用：恢复过的设备又变成静音（蓝牙耳机切回音乐模式时带回了静音），再取消一次
fn unmute_again(restored: &[String]) {
    let _guard = MUTED.lock().unwrap();
    if WANTED.load(Ordering::SeqCst) {
        return;
    }
    for id in all_devices() {
        if !device_uid(id).is_some_and(|uid| restored.contains(&uid)) {
            continue;
        }
        let elements = mute_elements(id);
        if any_muted(id, &elements) {
            let ok = set_device(id, &elements, false);
            log::info!("Output {} was muted again after restoring, unmuted it {}", describe(id), if ok { "again" } else { "- failed" });
        }
    }
}

fn state(id: AudioObjectId, elements: &[u32]) -> &'static str {
    if all_muted(id, elements) {
        "muted"
    } else if any_muted(id, elements) {
        "partly muted"
    } else {
        "unmuted"
    }
}

/// 取消我们静音的设备；返回这些设备的 UID（恢复后还要再盯几秒）
fn restore() -> Vec<String> {
    let mut muted = MUTED.lock().unwrap();
    if muted.is_empty() {
        // 记了文件但没静音成功时，文件也要清掉
        let _ = std::fs::remove_file(pending_file());
        return Vec::new();
    }
    for id in all_devices() {
        if device_uid(id).is_some_and(|uid| muted.contains(&uid)) {
            let elements = mute_elements(id);
            if !set_device(id, &elements, false) {
                log::warn!("Restore: unmuting output {} failed", describe(id));
            }
            log::info!("Restored output {} (reads back {})", describe(id), state(id, &elements));
        }
    }
    // 找不到的设备（已拔掉）也不再记着：重新接上时它的静音状态由系统保存，用户能看到
    let restored = std::mem::take(&mut *muted);
    let _ = std::fs::remove_file(pending_file());
    restored
}
